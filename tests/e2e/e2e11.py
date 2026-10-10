# /// script
# requires-python = ">=3.10"
# dependencies = ["websockets==17.1"]
# ///
"""e2e11: WS subscriptions and Push, backpressure, the connection health counter, drafts, logout teardown, invites.

The Python port of e2e11.ps1 (issue #114): same scenarios, same check names. Run with `uv run tests/e2e/e2e11.py`.
"""

import time
import uuid
from urllib.parse import quote

from wbf_helpers import (Suite, anchor, be32, conv, describe, hello_pack, json_pack, new_pack, push_events, rd_be32,
                         recv_or_none, send_pack, utf8, ws_close, ws_is_open, ws_open, ws_send)

t = Suite("e2e11")
GSEQKEY = "org.wbftw.wbfuwunel.g_seq"


def esc(s: str) -> str:
    return quote(s, safe="")


def write_config11(db, queue_len: int = 32, draft_max_members: int = 0):
    cfg = t.config_path("e2e11")
    # ⚠️ A to-device window must fit in the send queue (/docs/design/keys/to-device.md 7, asserted at startup), so
    # shrinking the queue for the backpressure scenario shrinks the window with it: a queue of four
    # packs takes four packs of a hundred. Without this the server refuses to start, correctly.
    device_limit = queue_len * 100
    lines = ["[global]", 'server_name = "localhost"', f'database_path = "{db.as_posix()}"', "port = 8015",
             'address = ["127.0.0.1"]', "allow_registration = true",
             "yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true",
             "allow_federation = false", f"wbf_ws_send_queue_len = {queue_len}",
             f"wbf_device_fetch_max_limit = {device_limit}", "wbf_ws_idle_timeout = 120", 'log = "info"']
    if draft_max_members > 0:
        lines.append(f"wbf_draft_max_room_members = {draft_max_members}")
    cfg.write_text("\n".join(lines), encoding="ascii")
    return cfg


def gseq_of(ev):
    if ev and isinstance(ev.get("unsigned"), dict) and GSEQKEY in ev["unsigned"]:
        return int(ev["unsigned"][GSEQKEY])
    return None


def register(name):
    return t.api("POST", "/_matrix/client/v3/register",
                 {"username": name, "password": "pw-pw-pw-pw", "auth": {"type": "m.login.dummy"}})


def create_room(tok, name):
    r = t.api("POST", "/_matrix/client/v3/createRoom", {"preset": "private_chat", "name": name}, tok)
    return r.get("room_id") if r else None


def invite(room, who, tok):
    t.api("POST", f"/_matrix/client/v3/rooms/{esc(room)}/invite", {"user_id": who}, tok)


def join(room, tok):
    t.api("POST", f"/_matrix/client/v3/join/{esc(room)}", "{}", tok)


def leave(room, tok):
    t.api("POST", f"/_matrix/client/v3/rooms/{esc(room)}/leave", "{}", tok)


def kick(room, who, tok):
    t.api("POST", f"/_matrix/client/v3/rooms/{esc(room)}/kick", {"user_id": who}, tok)


# Sends an event of an arbitrary type through the ordinary path; None when the server refuses.
def send_msg_typed(room, type, content, tok):
    r = t.api("PUT", f"/_matrix/client/v3/rooms/{esc(room)}/send/{esc(type)}/{uuid.uuid4().hex}", content, tok)
    return r.get("event_id") if r else None


def send_msg(room, body, tok):
    return send_msg_typed(room, "m.room.message", {"msgtype": "m.text", "body": body}, tok)


# Two different questions, so two functions (e2e11.ps1 answers the first with `Is-Push`):
# - Did the server send this on its own, rather than in reply? `call()` skips those.
#   ⭐ Told apart by a *field* rather than a list of subtypes: every server → client push carries `gap` in its
#   meta and no reply ever does -- Ack, Error and the Batch packs all leave it out. A list has to name each push;
#   the one this port first copied named only 0x14/0x06 and would have taken 0x14/0x07, 0x16/0x06, 0x16/0x08 and
#   the invite pushes for replies (/docs/design/events/invites-on-the-wire.md §4).
# - Is this an Event/Push (0x14/0x06), whose data is events to decode?
def is_server_initiated(p):
    return p is not None and not p.closed and isinstance(p.meta, dict) and "gap" in p.meta


def is_event_push(p):
    return p is not None and not p.closed and p.kind == 0x14 and p.subtype == 6


# Send one pack and wait for one frame, at most 10 s; a silent server fails the run instead of hanging it.
# A server-initiated push already queued for this connection is skipped: the caller wants the reply.
def call(ws, pack):
    ws_send(ws, pack)
    for _ in range(50):
        p = recv_or_none(ws, 10000)
        if p is None:
            raise RuntimeError("no reply within 10 s")
        if p.closed:
            raise RuntimeError(f"server closed the connection: {p}")
        if not is_server_initiated(p):
            return p
    raise RuntimeError("only pushes, no reply")


# Collects every frame until the channel is quiet for quiet_ms; Push packs get .events.
def drain_pushes(ws, quiet_ms: int = 1500):
    packs = []
    while True:
        p = recv_or_none(ws, quiet_ms)
        if p is None or p.closed:
            break
        p.events = push_events(p.data) if is_event_push(p) else []
        packs.append(p)
    return packs


# Opens a connection and proves it usable with a Hello round-trip, retrying within within_ms. A slot freed by
# closing another connection comes back when that connection's task ends, and a connection with packs still
# queued drains them first (DRAIN_TIMEOUT) -- so "I closed one, open another now" is a race a fixed sleep only
# sometimes wins. Returns (ws, waited_ms).
def ws_open_usable(tok, within_ms: int = 8000):
    started = time.monotonic()
    while True:
        ws = None
        try:
            ws = ws_open(tok)
            ws_send(ws, hello_pack("e2e11"))
            p = recv_or_none(ws, 2000)
            if p is not None and not p.closed and p.subtype == 2:
                return ws, int((time.monotonic() - started) * 1000)
        except Exception:
            pass  # a refused upgrade is the same race as a refused Hello
        ws_close(ws)
        time.sleep(0.25)
        if (time.monotonic() - started) * 1000 >= within_ms:
            raise RuntimeError(f"no usable connection within {within_ms} ms")


def subscribe(ws, id, rooms=None, cg_seq=None):
    meta = {}
    if rooms is not None:
        meta["rooms"] = list(rooms)
    if cg_seq is not None:
        meta["cg_seq"] = cg_seq
    return call(ws, json_pack(0x14, 4, conv(id), 0, meta))


def unsubscribe(ws, id, rooms=None):
    meta = {}
    if rooms is not None:
        meta["rooms"] = list(rooms)
    return call(ws, json_pack(0x14, 5, conv(id), 1, meta))


def ids(packs):
    return [e.get("event_id") for p in packs for e in p.events]


# ---------- 0x05 Invite (/docs/design/events/invites-on-the-wire.md §5) ----------
def invite_subscribe(ws, id):
    return call(ws, json_pack(0x05, 4, conv(id), 0, {}))


def invite_unsubscribe(ws, id):
    return call(ws, json_pack(0x05, 5, conv(id), 1, {}))


# One complete pass: Batch packs until `r` reaches 0. Returns every pack, so a caller can check the
# counting as well as the contents. 🚨 `r == 0` is the only thing that says the pass is complete.
def invite_fetch(ws, id, batch=None):
    meta = {} if batch is None else {"batch": batch}
    packs = []
    p = call(ws, json_pack(0x05, 1, conv(id), 0, meta))
    while True:
        packs.append(p)
        if p.kind != 0x05 or p.subtype != 2 or int(p.m("r")) == 0:
            return packs
        p = None
        while p is None:
            q = recv_or_none(ws, 10000)
            if q is None:
                raise RuntimeError("the invite pass stopped before r reached 0")
            if q.closed:
                raise RuntimeError(f"server closed the connection: {q}")
            if not is_server_initiated(q):
                p = q


def invite_entries(packs):
    return [e for p in packs if p.kind == 0x05 and p.subtype == 2 for e in push_events(p.data)]


# The rooms one pass listed, in the order it served them (oldest invite first).
def invite_rooms(packs):
    return [e.get("room_id") for e in invite_entries(packs)]


# Waits for one Invite push (0x05/0x06 Push or 0x05/0x07 Gone), or None.
def recv_invite(ws, ms: int = 4000):
    for _ in range(20):
        p = recv_or_none(ws, ms)
        if p is None or p.closed:
            return None
        if p.kind == 0x05:
            return p
    return None


# A Stream pack (0x02): the meta is the room id itself, as UTF-8 text rather than JSON, and the
# header carries which draft (id) and the author's piece counter (seq).
def stream_pack(subtype, room, draft_id, seq, data=None):
    return new_pack(0x02, subtype, 0, draft_id, seq, utf8(room), data)


def draft_pack(room, seq):
    return stream_pack(0x01, room, 0, seq)


# A piece (Keypoint/Delta/Append): its data starts with the seq it follows, big-endian
# (/docs/design/events/streaming-messages.md 3.0). Keypoint's prev is 0 — it replaces the whole buffer.
def piece_pack(subtype, room, draft_id, seq, prev, payload=b""):
    return stream_pack(subtype, room, draft_id, seq, be32(prev) + payload)


def kind02(packs, subtype=None):
    return [p for p in packs if p.kind == 0x02 and (subtype is None or p.subtype == subtype)]


def is_error(p, code_id):
    return p.subtype == 3 and p.m("code_id") == code_id


def main():
    # ================= Scenario 1: subscribe, push, membership hooks, ignore =================
    t.log("################ Scenario 1: channels and Push ################")
    cfg = write_config11(t.fresh_db("e2e11db-1"))
    server = t.start_server(cfg, "s1")
    reg_a = register("alice"); tok_a = reg_a["access_token"]
    reg_b = register("bob"); tok_b = reg_b["access_token"]
    reg_c = register("carol"); tok_c = reg_c["access_token"]
    r1 = create_room(tok_a, "one")
    invite(r1, reg_b["user_id"], tok_a); join(r1, tok_b)
    invite(r1, reg_c["user_id"], tok_a); join(r1, tok_c)

    # [1.1] alice: one subscribed connection, one not; bob sends -> only the subscribed one gets a Push
    ws_sub = ws_open(tok_a)
    ws_not = ws_open(tok_a)
    hello = call(ws_sub, hello_pack("e2e11"))
    t.check("[1.0] Hello reports push and a connection_id",
            lambda: "push" in (hello.m("features") or []) and hello.m("connection_id") > 0,
            lambda: f"features={','.join(hello.m('features') or [])} connection_id={hello.m('connection_id')}")
    ack = subscribe(ws_sub, 7)
    t.check("[1.1a] account-wide Subscribe -> Ack with joined>=1, latest_g_seq, skipped=[]",
            lambda: ack.subtype == 2 and ack.m("joined") >= 1 and ack.m("latest_g_seq") > 0 and ack.m("skipped") == [],
            describe(ack))
    m1 = send_msg(r1, "hello from bob", tok_b)
    pushes = drain_pushes(ws_sub)
    t.check("[1.1b] bob sends -> the subscribed connection gets one Push with that event, id=7 seq=0, fs=ls=g_seq, gap=false",
            lambda: len(pushes) == 1 and pushes[0].subtype == 6 and pushes[0].id == conv(7) and pushes[0].seq == 0
            and len(pushes[0].events) == 1 and pushes[0].events[0]["event_id"] == m1
            and int(pushes[0].m("fs")) == gseq_of(pushes[0].events[0]) and pushes[0].m("fs") == pushes[0].m("ls")
            and pushes[0].m("gap") is False,
            lambda: f"{describe(pushes[0])} events={len(pushes[0].events)} first={pushes[0].events[0]['event_id']} m1={m1} g={gseq_of(pushes[0].events[0])}")
    nothing = recv_or_none(ws_not, 1000)
    t.check("[1.1c] the unsubscribed connection got nothing", nothing is None, "")

    # [1.2] alice sends over the WS herself -> Ack (event_id) and a Push of the same event on the same connection
    send_meta = {"room_id": r1, "type": "m.room.message", "txn_id": uuid.uuid4().hex}
    ws_send(ws_sub, json_pack(0x14, 2, 0, 5, send_meta, b'{"msgtype":"m.text","body":"from alice over ws"}'))
    # The Push is queued inside append, before the handler's Ack: two frames, Push first.
    frames = drain_pushes(ws_sub)
    ack_send = [f for f in frames if f.kind == 1 and f.subtype == 2]
    pushes = [f for f in frames if is_event_push(f)]
    t.check("[1.2] own Event/Send -> Ack with event_id and a Push of the same event (seq 1), Push first",
            lambda: len(ack_send) == 1 and ack_send[0].m("event_id") and len(pushes) == 1 and pushes[0].seq == 1
            and pushes[0].events[0]["event_id"] == ack_send[0].m("event_id") and frames[0].subtype == 6,
            lambda: f"frames={','.join(f'{f.kind}/{f.subtype}' for f in frames)} ack={ack_send[0].m('event_id') if ack_send else None} push={ids(pushes)}")

    # [1.3] Subscribe with cg_seq: events after the watermark arrive first, then live ones
    mark = gseq_of(pushes[0].events[0])
    m2 = send_msg(r1, "after mark 1", tok_b)
    m3 = send_msg(r1, "after mark 2", tok_b)
    drain_pushes(ws_sub, 800)  # clear the live pushes of m2/m3 on the first connection
    ws_late = ws_open(tok_a)
    ack_late = subscribe(ws_late, 8, None, mark)
    caught = ids(drain_pushes(ws_late))
    t.check("[1.3] Subscribe cg_seq=<mark> -> the two newer events are pushed (catch-up), newest first",
            ack_late.subtype == 2 and caught == [m3, m2], f"caught={','.join(map(str, caught))}")
    m4 = send_msg(r1, "live after catch-up", tok_b)
    live = drain_pushes(ws_late)
    t.check("[1.3b] then live events keep coming on that connection",
            lambda: len(live) == 1 and live[0].events[0]["event_id"] == m4, lambda: ids(live))

    # [1.4] a room alice joins after subscribing is followed (account-wide); a named-room subscription is not
    ws_named = ws_open(tok_a)
    ack_named = subscribe(ws_named, 9, [r1])
    r2 = create_room(tok_b, "two")
    invite(r2, reg_a["user_id"], tok_b); join(r2, tok_a)
    m5 = send_msg(r2, "in room two", tok_b)
    in_sub = drain_pushes(ws_sub)
    in_named = drain_pushes(ws_named, 800)
    named_got = len([p for p in in_named if m5 in ids([p])])
    t.check("[1.4a] account-wide connection gets the new room's event", m5 in ids(in_sub), ids(in_sub))
    t.check("[1.4b] named-room connection does not (it only asked for room one)",
            ack_named.m("joined") == 1 and named_got == 0, f"named joined={ack_named.m('joined')} got={named_got}")

    # [1.4c] the catch-up window of a named subscription holds only those rooms.
    # 🚨 It used to be the account's whole window whatever the subscription said,
    # and the design document asked clients not to advance their watermark on the
    # rooms they had not asked for (wbf-event-push 2.1, rumia R4) — a rule that
    # asks a client not to believe what the server just sent it.
    mark2 = int(ack_named.m("latest_g_seq"))
    m_both1 = send_msg(r1, "after the mark, room one", tok_b)
    m_both2 = send_msg(r2, "after the mark, room two", tok_b)
    # ⚠️ alice already holds four connections here (ws_sub, ws_not, ws_late, ws_named). This frees one rather
    # than opening a fifth: ws_not has done its job ([1.1c]) and goes. It keeps this script off
    # `wbf_ws_max_connections_per_device` whatever the default becomes.
    ws_close(ws_not)
    ws_scoped, _ = ws_open_usable(tok_a)
    subscribe(ws_scoped, 11, [r1], mark2)
    caught_scoped = ids(drain_pushes(ws_scoped, 1500))
    t.check("[1.4c] a named subscription catching up is pushed its own rooms only",
            m_both1 in caught_scoped and m_both2 not in caught_scoped,
            f"caught={','.join(map(str, caught_scoped))} wanted={m_both1} not-wanted={m_both2}")
    # 🚨 The same room named twice is one room, in the Ack's count and in the
    # catch-up: without deduplication the window scans it twice and every event
    # is pushed twice (PR #51 review, rumia and salvia).
    ws_close(ws_named)
    ws_double, waited = ws_open_usable(tok_a)
    t.log(f"  (a slot came back {waited} ms after the subscribed connection was closed)")
    ghost = "!nobody-is-in-this:localhost"
    ack_double = subscribe(ws_double, 12, [r1, r1, ghost, ghost], mark2)
    caught_double = ids(drain_pushes(ws_double, 1500))
    skipped_double = ack_double.m("skipped") or []
    # 📎 `joined` is not what this discriminates: it comes from the registry,
    # whose topics are a set, and a red-light run with no deduplication still
    # said joined=1. What broke there was caught=2 (the catch-up scanned the room
    # twice). `skipped` is the list the request's own names build.
    t.check("[1.4d] naming a room twice catches up once, and a missing room named twice is skipped once",
            len(caught_double) == len(set(caught_double)) and m_both1 in caught_double and len(skipped_double) == 1,
            f"joined={ack_double.m('joined')} caught={len(caught_double)} distinct={len(set(caught_double))} skipped={','.join(skipped_double)}")

    # [1.5] leaving a room stops its pushes; kick stops them too
    leave(r2, tok_a)
    m6 = send_msg(r2, "after alice left", tok_b)
    leave_events = ids(drain_pushes(ws_sub, 1000))
    t.check("[1.5a] after leaving room two, its later message is not pushed (the leave itself may be)",
            m6 not in leave_events, f"got={','.join(map(str, leave_events))}")
    ws_b = ws_open(tok_b)
    subscribe(ws_b, 10)
    kick(r1, reg_b["user_id"], tok_a)
    m7 = send_msg(r1, "after bob was kicked", tok_c)
    bob_after = ids(drain_pushes(ws_b, 1000))
    t.check("[1.5b] after being kicked, bob is not pushed the room's later message",
            m7 not in bob_after, f"got={','.join(map(str, bob_after))}")

    # [1.6] ignoring carol drops her events for alice
    t.api("PUT", f"/_matrix/client/v3/user/{esc(reg_a['user_id'])}/account_data/m.ignored_user_list",
          {"ignored_users": {reg_c["user_id"]: {}}}, tok_a)
    time.sleep(0.3)
    drain_pushes(ws_sub, 800)
    m8 = send_msg(r1, "carol, ignored", tok_c)
    ignored = ids(drain_pushes(ws_sub, 1000))
    t.check("[1.6] an ignored sender's event is not pushed", m8 not in ignored, f"got={','.join(map(str, ignored))}")

    # [1.7] Unsubscribe: named room, then all; idempotent
    u1 = unsubscribe(ws_sub, 7, [r1])
    u2 = unsubscribe(ws_sub, 7, [r1])
    m9 = send_msg(r1, "after unsubscribe", tok_a)
    after_unsub = ids(drain_pushes(ws_sub, 1000))
    t.check("[1.7a] Unsubscribe room one (twice, second is a no-op) -> Ack both, no more pushes for it",
            u1.subtype == 2 and u2.subtype == 2 and m9 not in after_unsub, f"got={','.join(map(str, after_unsub))}")
    u3 = unsubscribe(ws_late, 8)
    t.check("[1.7b] Unsubscribe all -> Ack", u3.subtype == 2, describe(u3))

    # [1.8] HTTP: Subscribe is Unsupported
    over_http = send_pack(json_pack(0x14, 4, conv(11), 0, {}), tok_a)
    t.check("[1.8] Subscribe over HTTP -> Error Unsupported",
            over_http.subtype == 3 and over_http.m("code") == "Unsupported", describe(over_http))
    ws_close(ws_sub, ws_late, ws_scoped, ws_double, ws_b)
    t.stop_server(server)

    # ================= Scenario 2: backpressure: a reader that stops reading gets a gap, never blocks the sender =================
    t.log("################ Scenario 2: gap under backpressure ################")
    cfg2 = write_config11(t.fresh_db("e2e11db-2"), 4)
    server = t.start_server(cfg2, "s2")
    reg_a = register("alice"); tok_a = reg_a["access_token"]
    reg_b = register("bob"); tok_b = reg_b["access_token"]
    r1 = create_room(tok_a, "one")
    invite(r1, reg_b["user_id"], tok_a); join(r1, tok_b)
    ws_a = ws_open(tok_a)
    subscribe(ws_a, 1)
    # alice stops reading; bob sends: each must be acknowledged promptly (the push never blocks append).
    # 60 KB bodies, 100 of them: small packs would all sit in the socket buffers and the server queue would
    # never fill; a few megabytes do fill it. ⚠️ The client library reads into a queue of its own; it stops
    # reading the socket once 16 frames wait there, so the server-side queue still fills.
    started = time.monotonic()
    pad = "x" * 60000
    sent = [send_msg(r1, f"flood {i} {pad}", tok_b) for i in range(1, 101)]
    elapsed = time.monotonic() - started
    t.check("[2.1] 100 sends of 60 KB while the subscriber is not reading all succeed quickly",
            len(sent) == 100 and all(sent) and elapsed < 60, f"{elapsed:.1f} s")
    # now read: the queue (4) plus the socket buffers held some, the rest were dropped
    got_ids = ids(drain_pushes(ws_a, 1500))
    t.check("[2.2a] fewer than 100 arrived (the rest were dropped, append never waited)",
            0 < len(got_ids) < 100, f"arrived={len(got_ids)}")
    # gap is a flag on the NEXT push that gets through (the client cannot be told about a drop by a pack that was dropped)
    m_after = send_msg(r1, "after the flood", tok_b)
    nxt = drain_pushes(ws_a, 1500)
    gap_seen = len([p for p in nxt if p.m("gap") is True])
    t.check("[2.2b] the next Push after the drops carries gap=true",
            gap_seen >= 1 and m_after in ids(nxt), f"next={','.join(str(p.m('gap')) for p in nxt)}")
    sent.append(m_after)
    # fill in with Recent from the watermark; collect all windows
    recent_ids = []
    ws_send(ws_a, json_pack(0x14, 1, conv(99), 0, {"limit": 200, "batch": 20}))
    while True:
        batch = recv_or_none(ws_a, 5000)
        if batch is None or batch.closed:
            break
        if batch.kind == 0x14 and batch.subtype == 3:
            recent_ids += [e.get("event_id") for e in push_events(batch.data)]
        if batch.m("r") == 0:
            break
    missing = len([e for e in sent if e not in recent_ids])
    t.check("[2.3] Recent returns every flooded event (the truth is in the DB, the push only hinted)",
            missing == 0, f"missing={missing} recent={len(recent_ids)}")
    ws_close(ws_a)

    t.log("################ Scenario 3: the connection health counter (wire-format 2.1) ################")
    # A pack whose kind byte is unassigned is framed perfectly: decode refuses it, but its sender speaks
    # the protocol, so it must not spend the budget. PR #38's first version counted every decode error
    # and would close this connection on the eighth one.
    ws_h = ws_open(tok_a)
    unknown_answers = 0
    try:
        for i in range(9):
            answer = call(ws_h, new_pack(0x7F, 1, 0, 0, i, b"", b""))
            if answer.subtype == 3 and answer.m("code") == "UnknownKind" and answer.m("code_id") == 1101:
                unknown_answers += 1
    except Exception as e:
        t.log(f"  (scenario 3: {e})")
    t.check("[3.1] nine packs with an unassigned kind -> UnknownKind each, the connection stays open",
            unknown_answers == 9 and ws_is_open(ws_h), f"answered={unknown_answers} state={ws_h.state.name}")
    hello = None
    try:
        hello = call(ws_h, hello_pack("e2e11.py", 90))
    except Exception as e:
        t.log(f"  (scenario 3 Hello: {e})")
    t.check("[3.2] ... and the connection still answers a real request",
            hello is not None and hello.subtype == 2, describe(hello) if hello else "no reply")
    # Eight frames that really do not decode (the meta is damaged, so the CRC fails): the budget runs out.
    corrupt = bytearray(new_pack(1, 4, 0, 0, 91, b'{"nonce":1}', b""))
    corrupt[20] ^= 0xFF
    corrupt = bytes(corrupt)
    errors, closed = 0, None
    # A connection the earlier checks already lost cannot be tested further, and sending into it is how
    # a failed run turns into a hung one: stop instead.
    for _ in range(8):
        if closed is not None or not ws_is_open(ws_h):
            break
        try:
            ws_send(ws_h, corrupt)
        except Exception as e:
            t.log(f"  (scenario 3 send: {e})")
            break
        f = recv_or_none(ws_h, 5000)
        if f is None:
            continue
        if f.closed:
            closed = f
        elif f.subtype == 3 and f.m("code_id") == 1002:
            errors += 1
    if closed is None:
        f = recv_or_none(ws_h, 5000)
        if f is not None and f.closed:
            closed = f
    t.check("[3.3] eight frames that do not decode -> Corrupt each, then the server closes the connection",
            closed is not None and errors >= 7, f"errors={errors} close={closed}")
    ws_close(ws_h)
    t.stop_server(server)

    # ================= Scenario 4: drafts (/docs/design/events/streaming-messages.md, 0x02 Stream) =================
    t.log("################ Scenario 4: drafts over the channel ################")
    # The member cap is set to 2 rather than filling a room with eleven accounts: the rule is
    # "more members than the cap refuses a Draft", and two sides of that are two sides of it.
    cfg4 = write_config11(t.fresh_db("e2e11db-4"), 32, 2)
    server = t.start_server(cfg4, "s4")
    reg_a = register("alice"); tok_a = reg_a["access_token"]
    reg_b = register("bob"); tok_b = reg_b["access_token"]
    reg_c = register("carol"); tok_c = reg_c["access_token"]
    room = create_room(tok_a, "drafts")
    invite(room, reg_b["user_id"], tok_a); join(room, tok_b)

    ws_a = ws_open(tok_a); call(ws_a, hello_pack("e2e11-a"))
    ws_b = ws_open(tok_b); call(ws_b, hello_pack("e2e11-b"))
    subscribe(ws_a, 40)
    subscribe(ws_b, 41)

    # [4.1] Draft writes a real, pushed anchor event
    draft = call(ws_a, draft_pack(room, 1))
    draft_id = int(draft.m("id") or 0)  # already carries its type byte (wire-format 2.2)
    anchor_types = [e.get("type") for p in drain_pushes(ws_b, 1500) for e in p.events]
    t.check("[4.1a] Draft -> Ack with event_id and g_seq", draft.subtype == 2 and draft.m("event_id") and draft_id > 0, describe(draft))
    t.check("[4.1b] the anchor is a real event: bob is pushed it, type org.wbftw.wbfuwunel.draft",
            "org.wbftw.wbfuwunel.draft" in anchor_types, f"types={','.join(map(str, anchor_types))}")

    # The id's type byte, on this kind specifically (wire-format 2.2, PR #47). The unit
    # tests cover the gate; these two cover the two mistakes a client actually makes:
    # composing the id itself before the server has minted one, and sending a piece with
    # the bare g_seq it read out of the Ack's meta instead of the composed id beside it.
    early_type = call(ws_a, stream_pack(0x01, room, anchor(7), 2))
    t.check("[4.1c] a Draft naming an anchor it does not have yet -> InvalidRequest", is_error(early_type, 1201), describe(early_type))
    bare_id = call(ws_a, piece_pack(0x05, room, int(draft.m("g_seq") or 0), 1, 0, b"x"))
    t.check("[4.1d] a piece carrying the bare g_seq instead of the composed id -> InvalidRequest", is_error(bare_id, 1201), describe(bare_id))
    drain_pushes(ws_a, 800)

    # [4.2] the three piece subtypes reach the room unchanged — and the author's own connection too
    append_data = utf8("piece one")
    delta_data = utf8("piece two")
    key_data = utf8("the whole draft so far")
    ws_send(ws_a, piece_pack(0x05, room, draft_id, 1, 0, append_data))
    ws_send(ws_a, piece_pack(0x04, room, draft_id, 2, 1, delta_data))
    ws_send(ws_a, piece_pack(0x03, room, draft_id, 3, 0, key_data))
    at_b = kind02(drain_pushes(ws_b, 1500))
    at_a = kind02(drain_pushes(ws_a, 1500))
    subtypes_b = ",".join(str(p.subtype) for p in at_b)
    bodies_b = "|".join(p.data[4:].decode("utf-8", errors="replace") for p in at_b)
    prevs_b = ",".join(str(rd_be32(p.data, 0)) for p in at_b)
    meta_b = [p.meta_text for p in at_b]
    t.check("[4.2a] bob receives Append, Delta and Keypoint in order, data byte for byte",
            subtypes_b == "5,4,3" and bodies_b == "piece one|piece two|the whole draft so far",
            f"subtypes={subtypes_b} bodies={bodies_b}")
    t.check("[4.2d] each piece names the one it follows; the Keypoint names none", prevs_b == "0,1,0", f"prevs={prevs_b}")
    # 📎 Stricter than the .ps1, whose `(unique metas) -eq $room` was also true when one of several metas matched.
    t.check("[4.2b] the meta is the room id itself and the header carries the draft id and the piece counter",
            set(meta_b) == {room} and [p.id for p in at_b] == [draft_id] * 3 and [p.seq for p in at_b] == [1, 2, 3],
            lambda: f"meta={meta_b[0] if meta_b else None} ids={','.join(str(p.id) for p in at_b)} seqs={','.join(str(p.seq) for p in at_b)}")
    t.check("[4.2c] the author gets its own pieces back (one broadcast, no special case)", len(at_a) == 3, f"at the author={len(at_a)}")

    # [4.3] the size limit, both sides of it
    too_big = bytes(10241)
    just_fits = bytes(10236)  # plus the 4-byte prev is exactly the limit
    big = call(ws_a, stream_pack(0x03, room, draft_id, 4, too_big))
    t.check("[4.3a] a piece over wbf_draft_max_piece_bytes -> TooLarge", is_error(big, 1103), describe(big))
    ws_send(ws_a, piece_pack(0x03, room, draft_id, 5, 0, just_fits))
    fitted = kind02(drain_pushes(ws_b, 1500))
    t.check("[4.3b] exactly the limit passes", lambda: len(fitted) == 1 and len(fitted[0].data) == 10240,
            lambda: f"packs={len(fitted)} bytes={len(fitted[0].data) if fitted else None}")
    drain_pushes(ws_a, 800)

    # [4.4] the meta is a room id, not JSON
    bad_meta = call(ws_a, new_pack(0x02, 0x05, 0, draft_id, 6, utf8('{"room_id":"!x:localhost"}'), be32(0) + append_data))
    t.check("[4.4] meta that is not a room id -> InvalidRequest", is_error(bad_meta, 1201), describe(bad_meta))

    # [4.4b] the chain itself: the three rules the server enforces without reading the content
    no_prev = call(ws_a, stream_pack(0x05, room, draft_id, 7, utf8("ab")))
    t.check("[4.4b] a piece whose data is too short to carry prev -> InvalidRequest", is_error(no_prev, 1201), describe(no_prev))
    zero_seq = call(ws_a, piece_pack(0x05, room, draft_id, 0, 0, append_data))
    t.check('[4.4c] a piece numbered 0 -> InvalidRequest (0 is reserved for "no base")', is_error(zero_seq, 1201), describe(zero_seq))
    based_keypoint = call(ws_a, piece_pack(0x03, room, draft_id, 7, 6, key_data))
    t.check("[4.4d] a Keypoint that names a base -> InvalidRequest (it replaces the whole draft)", is_error(based_keypoint, 1201), describe(based_keypoint))

    # [4.5] Demand is broadcast like any other piece; a connection that did not subscribe gets nothing
    ws_c = ws_open(tok_c); call(ws_c, hello_pack("e2e11-c"))
    invite(room, reg_c["user_id"], tok_a); join(room, tok_c)
    subscribe(ws_c, 42)
    drain_pushes(ws_a, 800); drain_pushes(ws_b, 800); drain_pushes(ws_c, 800)
    ws_send(ws_c, stream_pack(0x10, room, draft_id, 1))
    demand_a = kind02(drain_pushes(ws_a, 1500), 0x10)
    demand_c = kind02(drain_pushes(ws_c, 1500), 0x10)
    t.check("[4.5] a Demand reaches the author and the asker alike (no special routing)",
            len(demand_a) == 1 and len(demand_c) == 1, f"author={len(demand_a)} asker={len(demand_c)}")
    drain_pushes(ws_b, 800)

    # [4.6] the piece counter is the client's; the server carries it and does not renumber
    ws_send(ws_a, piece_pack(0x05, room, draft_id, 5, 4, append_data))
    ws_send(ws_a, piece_pack(0x05, room, draft_id, 6, 5, append_data))
    ws_send(ws_a, piece_pack(0x05, room, draft_id, 8, 7, append_data))
    counted = ",".join(str(p.seq) for p in kind02(drain_pushes(ws_b, 1500)))
    t.check("[4.6] 5, 6, 8 arrive as 5, 6, 8 (the gap is the client's to notice)", counted == "5,6,8", f"seqs={counted}")
    drain_pushes(ws_a, 800)

    # [4.7] who may write, and to what
    not_author = call(ws_b, piece_pack(0x05, room, draft_id, 9, 8, append_data))
    t.check("[4.7a] a piece from somebody who is not the author -> Forbidden", is_error(not_author, 1302), describe(not_author))
    no_such_draft = call(ws_a, piece_pack(0x05, room, anchor(999999), 9, 8, append_data))
    t.check("[4.7b] a draft id nothing in the room has -> NotFound", is_error(no_such_draft, 1501), describe(no_such_draft))
    ordinary = send_msg(room, "an ordinary message", tok_a)
    ordinary_g = gseq_of(t.api("GET", f"/_matrix/client/v3/rooms/{esc(room)}/event/{ordinary}", None, tok_a)) or 0
    not_a_draft = call(ws_a, piece_pack(0x05, room, anchor(ordinary_g), 9, 8, append_data))
    t.check("[4.7c] an event that is not a draft -> Conflict", is_error(not_a_draft, 1502), describe(not_a_draft))
    http_draft = send_pack(draft_pack(room, 9), tok_a)
    t.check("[4.7d] Stream over HTTP -> Unsupported", is_error(http_draft, 1102), describe(http_draft))
    drain_pushes(ws_a, 800); drain_pushes(ws_b, 800); drain_pushes(ws_c, 800)

    # [4.8] the room is now three people, and this server's cap is two
    too_busy = call(ws_a, draft_pack(room, 10))
    t.check("[4.8] a room past wbf_draft_max_room_members -> Conflict", is_error(too_busy, 1502), describe(too_busy))

    # [4.9] the throttle: 100 pieces at once is more than 30/s with a burst of 60
    for i in range(100):
        ws_send(ws_a, piece_pack(0x05, room, draft_id, 100 + i, 99 + i, append_data))
    answers = drain_pushes(ws_a, 2500)
    refused = len([p for p in answers if p.kind == 0x01 and is_error(p, 1401)])
    relayed = len(kind02(answers))
    t.check("[4.9] a hundred pieces at once: some are relayed, the rest are RateLimited",
            refused > 0 and relayed > 0 and refused + relayed == 100, f"relayed={relayed} rateLimited={refused}")
    drain_pushes(ws_b, 1500); drain_pushes(ws_c, 1500)

    # [4.10] the client's own ending: Abandon redacts the anchor, and the draft is closed for good
    bye = call(ws_a, stream_pack(0x02, room, draft_id, 201))
    redactions = [e for p in drain_pushes(ws_b, 2000) for e in p.events if e.get("type") == "m.room.redaction"]
    after_abandon = call(ws_a, piece_pack(0x05, room, draft_id, 202, 201, append_data))
    t.check("[4.10a] Abandon -> Ack with the redaction event id", bye.subtype == 2 and bye.m("redaction_event_id"), describe(bye))
    t.check("[4.10b] subscribers see the redaction as an ordinary push", len(redactions) >= 1, f"redactions={len(redactions)}")
    t.check("[4.10c] a piece for an abandoned draft -> Conflict", is_error(after_abandon, 1502), describe(after_abandon))

    # [4.11] the seven holes the external review of 2026-09-12 found, each refused where it was let through.
    # A second room, with two members: the first one is at the member cap, so no new draft can be
    # opened there — and a draft that was refused would make every check below pass for the wrong reason.
    room2 = create_room(tok_a, "drafts two")
    invite(room2, reg_b["user_id"], tok_a); join(room2, tok_b)
    ws_b2 = ws_open(tok_b); call(ws_b2, hello_pack("e2e11-b2"))
    subscribe(ws_b2, 45)
    drain_pushes(ws_a, 800); drain_pushes(ws_b2, 800)
    draft2 = call(ws_a, draft_pack(room2, 300))
    id2 = int(draft2.m("id") or 0)
    t.check("[4.11pre] the draft these checks need is open", lambda: draft2.subtype == 2 and draft2.m("g_seq") > 0, describe(draft2))
    drain_pushes(ws_a, 800); drain_pushes(ws_b2, 800)

    # R7: a stranger must not learn, from the difference between the refusals, whether an event exists
    reg_d = register("dave"); tok_d = reg_d["access_token"]
    ws_d = ws_open(tok_d); call(ws_d, hello_pack("e2e11-d"))
    probe_open = call(ws_d, stream_pack(0x10, room2, id2, 1))
    probe_missing = call(ws_d, stream_pack(0x10, room2, anchor(999999), 1))
    t.check("[4.11a] a non-member probing an open draft and a missing one gets the same refusal",
            is_error(probe_open, 1302) and is_error(probe_missing, 1302),
            f"open={probe_open.m('code_id')} missing={probe_missing.m('code_id')}")
    ws_close(ws_d)

    # R3: a Demand declares no data; a large one is refused and the connection closed, a small one is stripped
    ws_send(ws_b2, stream_pack(0x10, room2, id2, 1, utf8("ignored payload")))
    stripped_at_a = kind02(drain_pushes(ws_a, 1500), 0x10)
    t.check("[4.11b] a Demand is relayed with its data stripped",
            lambda: len(stripped_at_a) == 1 and len(stripped_at_a[0].data) == 0,
            lambda: f"packs={len(stripped_at_a)} bytes={len(stripped_at_a[0].data) if stripped_at_a else None}")
    ws_fat = ws_open(tok_b); call(ws_fat, hello_pack("e2e11-fat"))
    fat = call(ws_fat, stream_pack(0x10, room2, id2, 1, bytes(2048)))
    fat_close = recv_or_none(ws_fat, 5000)
    t.check("[4.11c] a Demand carrying 2 KiB -> InvalidRequest and the connection is closed",
            is_error(fat, 1201) and fat_close is not None and fat_close.closed,
            f"code={fat.m('code_id')} closed={fat_close.closed if fat_close else None} reason={fat_close}")
    ws_close(ws_fat)

    # flags: a Stream pack sets none of them
    flagged = call(ws_a, new_pack(0x02, 0x05, 8, id2, 2, utf8(room2), be32(1) + append_data))
    t.check("[4.11d] a Stream pack with a flag set -> InvalidRequest", is_error(flagged, 1201), describe(flagged))

    # R6: the anchor's event type is the server's to write
    forged = send_msg_typed(room, "org.wbftw.wbfuwunel.draft", {"msgtype": "org.wbftw.wbfuwunel.draft", "body": "(draft)"}, tok_a)
    t.check("[4.11e] an ordinary send of the anchor type -> refused, so the room cap cannot be walked around",
            forged is None, f"event_id={forged}")

    # R5: ignoring somebody hides their draft pieces, as it hides their messages
    t.api("PUT", f"/_matrix/client/v3/user/{esc(reg_b['user_id'])}/account_data/m.ignored_user_list",
          {"ignored_users": {reg_a["user_id"]: {}}}, tok_b)
    time.sleep(1)
    drain_pushes(ws_a, 800); drain_pushes(ws_b2, 800)
    ws_send(ws_a, piece_pack(0x05, room2, id2, 2, 1, append_data))
    at_ignorer = kind02(drain_pushes(ws_b2, 1500))
    at_other = kind02(drain_pushes(ws_a, 1500))
    t.check("[4.11f] a piece from an ignored author does not reach the ignorer, and still reaches everyone else",
            len(at_ignorer) == 0 and len(at_other) == 1, f"ignorer={len(at_ignorer)} other={len(at_other)}")
    t.api("PUT", f"/_matrix/client/v3/user/{esc(reg_b['user_id'])}/account_data/m.ignored_user_list",
          {"ignored_users": {}}, tok_b)

    # R4: the room can take a voice away, and an open draft must not be a way around that.
    bob_draft = call(ws_b2, draft_pack(room2, 1))
    bob_id = int(bob_draft.m("id") or 0)
    drain_pushes(ws_b2, 800)
    ws_send(ws_b2, piece_pack(0x05, room2, bob_id, 1, 0, append_data))
    before_mute = kind02(drain_pushes(ws_b2, 1500))
    t.api("PUT", f"/_matrix/client/v3/rooms/{esc(room2)}/state/m.room.power_levels/",
          {"users": {reg_a["user_id"]: 100, reg_b["user_id"]: -1}, "events_default": 0}, tok_a)
    time.sleep(1)
    muted = call(ws_b2, piece_pack(0x05, room2, bob_id, 2, 1, append_data))
    t.check("[4.11g] a piece before the room takes the author's voice away is relayed", len(before_mute) == 1, f"packs={len(before_mute)}")
    t.check("[4.11h] and one after it -> Forbidden: an open draft is not a way around a mute", is_error(muted, 1302), describe(muted))
    # Bob's voice goes back: the next check is about suspension, and a muted user cannot redact
    # either (a redaction is an event too), which would make it pass for the wrong reason.
    t.api("PUT", f"/_matrix/client/v3/rooms/{esc(room2)}/state/m.room.power_levels/",
          {"users": {reg_a["user_id"]: 100}, "events_default": 0}, tok_a)
    time.sleep(1)

    ws_close(ws_a, ws_b, ws_c, ws_b2)

    # R2 and R1 both need a restart: suspension is an admin endpoint (so somebody has to be made an
    # admin, which is a command that runs with the server stopped), and the transaction id can only
    # be shown to survive a restart by restarting.
    t.stop_server(server)
    t.exec_commands(cfg4, [f"user make-user-admin {reg_a['user_id']}"], "admin")
    server = t.start_server(cfg4, "s4b")

    # R1: this is the first connection of a new run, so its number is 1 — the same number alice's
    # first connection had. With the same request seq, the transaction id would be the one from the
    # first run, and the Ack would carry that run's anchor (in the other room).
    ws_a3 = ws_open(tok_a); call(ws_a3, hello_pack("e2e11-a3"))
    after_restart = call(ws_a3, draft_pack(room2, 1))
    t.check("[4.11i] a Draft after a restart writes a new anchor instead of replaying the first run's",
            lambda: after_restart.subtype == 2 and int(after_restart.m("id")) != draft_id,
            f"g_seq={after_restart.m('g_seq')} first run={draft_id}")

    # R2: a suspended account may abandon its draft but not write to it
    suspend = t.api("PUT", f"/_matrix/client/v1/admin/suspend/{esc(reg_b['user_id'])}", {"suspended": True}, tok_a)
    ws_b3 = ws_open(tok_b); call(ws_b3, hello_pack("e2e11-b3"))
    subscribe(ws_b3, 46)
    suspended_piece = call(ws_b3, piece_pack(0x05, room2, bob_id, 3, 1, append_data))
    t.check("[4.11j] a suspended author cannot broadcast a piece",
            suspend is not None and is_error(suspended_piece, 1302),
            f"suspend={suspend.get('suspended') if suspend else None} {describe(suspended_piece)}")
    bye_suspended = call(ws_b3, stream_pack(0x02, room2, bob_id, 301))
    t.check("[4.11k] ... but may still abandon its own draft",
            bye_suspended.subtype == 2 and bye_suspended.m("redaction_event_id"), describe(bye_suspended))

    ws_close(ws_a3, ws_b3)

    # ================= Scenario 5: a logged-out device's connection is torn down (external review #8 back half) =================
    # /docs/design/wire/session-teardown.md: subscriptions hang off the *connection*, and logging out only
    # deleted the token — so events and to-device items kept being pushed at a connection whose session was
    # gone, until it happened to send a frame (at most wbf_ws_idle_timeout, and that is what the attacker
    # who holds a stolen device simply does not do).
    t.log("################ Scenario 5: logging out ends that device's connections ################")

    def login_device(name):
        return t.api("POST", "/_matrix/client/v3/login",
                     {"type": "m.login.password", "identifier": {"type": "m.id.user", "user": name}, "password": "pw-pw-pw-pw"})

    room_t = create_room(tok_a, "teardown")
    reg_d = register("dana"); invite(room_t, reg_d["user_id"], tok_a); join(room_t, reg_d["access_token"])
    dana_one = login_device("dana")
    dana_two = login_device("dana")
    t.check("[5.0] dana has two devices, each with its own token",
            lambda: dana_one["device_id"] != dana_two["device_id"] and dana_one["access_token"] != dana_two["access_token"],
            lambda: f"one={dana_one['device_id']} two={dana_two['device_id']}")

    ws_one = ws_open(dana_one["access_token"])
    ws_two = ws_open(dana_two["access_token"])
    subscribe(ws_one, 70, [room_t])
    subscribe(ws_two, 71, [room_t])
    before = send_msg(room_t, "both devices should see this", tok_a)
    got_one = ids(drain_pushes(ws_one))
    got_two = ids(drain_pushes(ws_two))
    t.check("[5.1] both of dana's devices are pushed the event while both sessions live",
            before in got_one and before in got_two, f"one={got_one} two={got_two}")

    # The logout is an HTTP call on another connection entirely; device one sends NOTHING after it.
    t.api("POST", "/_matrix/client/v3/logout", "{}", dana_one["access_token"])
    closing = recv_or_none(ws_one, 5000)
    # 1008 is what .NET names PolicyViolation.
    t.check("[5.2] device one is closed without having sent anything, and long before the idle timeout (120 s here)",
            closing is not None and closing.closed and closing.code == 1008,
            "frame=nothing within 5 s" if closing is None else f"closed={closing.closed} code={closing.code if closing.closed else describe(closing)}")

    # 🚨 The point of the whole change: the push stops. Before it, this event reached a connection whose
    # token no longer existed.
    after = send_msg(room_t, "only the device still logged in should see this", tok_a)
    leaked = ids(drain_pushes(ws_one, 1200))
    still_there = ids(drain_pushes(ws_two))
    t.check("[5.3] nothing is pushed at the logged-out connection any more", after not in leaked, f"leaked={leaked}")
    t.check("[5.4] and dana's other device, which did not log out, still gets it", after in still_there, f"two={still_there}")

    # ⚠️ The reverse of [5.2]: locking an account deliberately has no teardown path of its own
    # (session-teardown.md §4.3) — revalidate catches it at the connection's next frame, so a client that
    # sends nothing is still there. This pins that decision rather than the absence of a feature.
    lock_target = register("edith")
    ws_locked = ws_open(lock_target["access_token"])
    call(ws_locked, hello_pack("e2e11-lock"))
    t.api("PUT", f"/_synapse/admin/v2/users/{esc(lock_target['user_id'])}", '{"locked":true}', tok_a)
    quiet = recv_or_none(ws_locked, 2500)
    t.check("[5.5] a locked account's connection is NOT torn down: it has to send something first",
            quiet is None, "frame=silence, as designed" if quiet is None else f"frame={describe(quiet)}")
    ws_send(ws_locked, new_pack(1, 4, 0, 0, 1, b"", b""))
    refused = recv_or_none(ws_locked, 5000)
    t.check("[5.6] ... and its next frame is refused with M_USER_LOCKED",
            refused is not None and not refused.closed and refused.subtype == 3 and refused.m("errcode") == "M_USER_LOCKED",
            describe(refused))

    ws_close(ws_one, ws_two, ws_locked)

    # ================= Scenario 6: the invite stream (0x05 Invite, issue #111) =================
    # /docs/design/events/invites-on-the-wire.md. A wbf account had no way to learn it was invited:
    # Event/Push only carries joined rooms, the bridged /joined_rooms only lists joined ones, and asking
    # the room itself is a 403 before you accept. ⭐ So invites get a subscription of their own, and it has
    # the one property the message stream deliberately does not: its catch-up runs to completion.
    #
    # 📎 Two things in that doc e2e cannot reach, and unit tests cover instead:
    #   - §6 the trim of `invite_room_state`: it only applies to state another server sent, and this is
    #     one server (`list_stripped_state_kept` has four tests).
    #   - §7 `Gone(knock)`: the auth rules forbid invite → knock, so no API call reaches that arm. It is
    #     covered by construction — one hook after the match, not one per arm.
    t.log("################ Scenario 6: invites have their own subscription ################")
    reg_f = register("frank"); tok_f = reg_f["access_token"]
    reg_g = register("grace")
    ws_f, _ = ws_open_usable(tok_f)
    ws_g, _ = ws_open_usable(reg_g["access_token"])
    sub_f = invite_subscribe(ws_f, 60)
    invite_subscribe(ws_g, 61)
    t.check("[6.0] Invite/Subscribe is acked with the position it registered at",
            lambda: sub_f.subtype == 2 and sub_f.m("latest_count") is not None and int(sub_f.m("latest_count")) > 0,
            describe(sub_f))

    room_i1 = create_room(tok_a, "invite-one")
    invite(room_i1, reg_f["user_id"], tok_a)
    pushed = recv_invite(ws_f)
    t.check("[6.1] being invited is pushed on the invite stream, with the room and who sent it",
            pushed is not None and pushed.subtype == 6 and pushed.m("room_id") == room_i1
            and pushed.m("inviter") == reg_a["user_id"] and pushed.id == sub_f.id,
            "nothing within 4 s" if pushed is None else describe(pushed))
    t.check("[6.1b] it carries the invite's own position and is_direct, which this one is not",
            lambda: int(pushed.m("is")) > 0 and pushed.m("is_direct") is False,
            lambda: f"is={pushed.m('is')} is_direct={pushed.m('is_direct')}")
    pushed_state = push_events(pushed.data) if pushed is not None else []
    pushed_types = [e.get("type") for e in pushed_state]
    t.check("[6.1c] and the stripped state, including frank's own member event, so sc is never 0 in the ordinary case",
            lambda: int(pushed.m("sc")) == len(pushed_state) and int(pushed.m("sc")) >= 1 and "m.room.name" in pushed_types
            and len([e for e in pushed_state if e.get("type") == "m.room.member" and e.get("state_key") == reg_f["user_id"]]) == 1,
            lambda: f"sc={pushed.m('sc')} types={','.join(map(str, pushed_types))}")

    # 🔴 An invite is not news for everybody: grace is subscribed to the invite stream and was not invited.
    leak_g = recv_invite(ws_g, 1500)
    t.check("[6.2] only the invited user is pushed, not every invite subscriber",
            leak_g is None, "silence, as designed" if leak_g is None else describe(leak_g))

    # 🔴 The streams do not mix: a connection that subscribed to messages only hears nothing about invites.
    ws_f_msg, _ = ws_open_usable(tok_f)
    subscribe(ws_f_msg, 62)
    room_i2 = create_room(tok_a, "invite-two")
    invite(room_i2, reg_f["user_id"], tok_a)
    on_msg_stream = recv_invite(ws_f_msg, 2000)
    on_invite_stream = recv_invite(ws_f)
    t.check("[6.3] Event/Subscribe does not carry invites: the two subscriptions are separate",
            on_msg_stream is None and on_invite_stream is not None and on_invite_stream.m("room_id") == room_i2,
            f"msgStream={'silence' if on_msg_stream is None else describe(on_msg_stream)} "
            f"inviteStream={'nothing' if on_invite_stream is None else on_invite_stream.m('room_id')}")

    # The pass: every pending invite, oldest first, until r reaches 0.
    pass_packs = invite_fetch(ws_f, 63)
    pass_rooms = invite_rooms(pass_packs)
    last = pass_packs[-1]
    t.check("[6.4] one Fetch pass lists every pending invite and ends with r = 0",
            lambda: len(pass_rooms) == 2 and room_i1 in pass_rooms and room_i2 in pass_rooms
            and int(last.m("r")) == 0 and int(last.m("tc")) == 2,
            lambda: f"rooms={','.join(map(str, pass_rooms))} r={last.m('r')} tc={last.m('tc')}")
    t.check("[6.4b] oldest first: the room invited to first is served first",
            lambda: pass_rooms[0] == room_i1, f"order={','.join(map(str, pass_rooms))}")
    entry_one = next((e for e in invite_entries(pass_packs) if e.get("room_id") == room_i1), None)
    t.check("[6.4c] and the entry says the same as the push did: same state, same derived fields",
            lambda: entry_one is not None and len(entry_one["state"]) == int(pushed.m("sc"))
            and entry_one.get("inviter") == reg_a["user_id"] and entry_one.get("is_direct") is False,
            lambda: f"state={'no entry' if entry_one is None else len(entry_one['state'])} push_sc={pushed.m('sc')}")

    # This stream is the connection's catch-up, so it has nowhere to go over HTTP.
    http_fetch = send_pack(json_pack(0x05, 1, conv(64), 0, {}), tok_f)
    http_sub = send_pack(json_pack(0x05, 4, conv(65), 0, {}), tok_f)
    t.check("[6.5] Fetch and Subscribe are both refused over HTTP",
            http_fetch.subtype == 3 and http_fetch.m("code") == "Unsupported"
            and http_sub.subtype == 3 and http_sub.m("code") == "Unsupported",
            f"fetch={describe(http_fetch)} sub={describe(http_sub)}")

    # Declining ends the invite, and the stream says what ended it.
    leave(room_i1, tok_f)
    gone = recv_invite(ws_f)
    t.check("[6.6] declining is pushed as Gone(leave), with the position of the event that did it",
            lambda: gone is not None and gone.subtype == 7 and gone.m("room_id") == room_i1
            and gone.m("membership") == "leave" and int(gone.m("is")) > 0,
            "nothing" if gone is None else describe(gone))
    after_leave = invite_rooms(invite_fetch(ws_f, 66))
    t.check("[6.6b] ... and the next pass no longer lists it",
            room_i1 not in after_leave and room_i2 in after_leave, f"rooms={','.join(map(str, after_leave))}")

    join(room_i2, tok_f)
    gone_join = recv_invite(ws_f)
    t.check("[6.7] accepting is pushed as Gone(join): how the list emptied has one source",
            gone_join is not None and gone_join.subtype == 7 and gone_join.m("room_id") == room_i2
            and gone_join.m("membership") == "join",
            "nothing" if gone_join is None else describe(gone_join))

    # 🚨 The negative of [6.6]: leaving a room nobody invited you to is not an invite ending. This is what
    # pins reading `is_invited` *before* the write — mark_as_left clears the invite either way.
    own_room = create_room(tok_f, "frank-own")
    leave(own_room, tok_f)
    not_gone = recv_invite(ws_f, 2000)
    t.check("[6.8] leaving a room you were never invited to pushes nothing",
            not_gone is None, "silence, as designed" if not_gone is None else describe(not_gone))

    # 🔴 §3.3, the whole reason the pass is a snapshot. While frank has no connection at all: one invite
    # arrives, and another arrives and is then withdrawn. A waterline the client kept would find the first
    # and never learn about the second -- the row it would have to notice is deleted.
    ws_close(ws_f, ws_f_msg)
    room_i3 = create_room(tok_a, "invite-while-away")
    room_i4 = create_room(tok_a, "withdrawn-while-away")
    invite(room_i3, reg_f["user_id"], tok_a)
    invite(room_i4, reg_f["user_id"], tok_a)
    kick(room_i4, reg_f["user_id"], tok_a)
    ws_f2, _ = ws_open_usable(tok_f)
    invite_subscribe(ws_f2, 67)
    away = invite_rooms(invite_fetch(ws_f2, 68))
    t.check("[6.9] an invite that arrived while every connection was closed is still caught up",
            room_i3 in away, f"rooms={','.join(map(str, away))}")
    t.check("[6.10] and one that was withdrawn while away is absent, which is what makes the pass a snapshot",
            room_i4 not in away, f"rooms={','.join(map(str, away))}")

    # Batching: the pass is cut into packs, and the counting has to add up across them.
    room_i5 = create_room(tok_a, "batch-one"); room_i6 = create_room(tok_a, "batch-two")
    invite(room_i5, reg_f["user_id"], tok_a); invite(room_i6, reg_f["user_id"], tok_a)
    batch_packs = [p for p in invite_fetch(ws_f2, 69, 1) if p.kind == 0x05 and p.subtype == 2]
    seqs = [p.seq for p in batch_packs]
    remainders = [int(p.m("r")) for p in batch_packs]
    oldests = [int(p.m("os")) for p in batch_packs]
    ascending = all(b > a for a, b in zip(oldests, oldests[1:]))
    t.check("[6.11] batch = 1 cuts the pass into one pack per room, seq counting from 0",
            len(batch_packs) == 3 and seqs == [0, 1, 2] and all(int(p.m("bc")) == 1 for p in batch_packs),
            f"packs={len(batch_packs)} seqs={','.join(map(str, seqs))}")
    t.check("[6.11b] r counts down to 0 and the counts only go forwards",
            remainders == [2, 1, 0] and ascending and all(int(p.m("tc")) == 3 for p in batch_packs),
            f"r={','.join(map(str, remainders))} os={','.join(map(str, oldests))}")

    # Unsubscribing stops the pushes; the catch-up is still there for next time.
    invite_unsubscribe(ws_f2, 67)
    room_i7 = create_room(tok_a, "after-unsubscribe")
    invite(room_i7, reg_f["user_id"], tok_a)
    after_unsub = recv_invite(ws_f2, 2000)
    t.check("[6.12] Unsubscribe stops the pushes",
            after_unsub is None, "silence, as designed" if after_unsub is None else describe(after_unsub))
    back_again = invite_rooms(invite_fetch(ws_f2, 70))
    t.check("[6.12b] ... and the invite it missed is still waiting in the next pass",
            room_i7 in back_again, f"rooms={','.join(map(str, back_again))}")

    ws_close(ws_g, ws_f2)
    t.stop_server(server)


t.run(main)
