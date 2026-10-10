"""Shared helpers for the wbf end-to-end scripts: pack codec, HTTP and WebSocket transport, server start/stop.

Imported by e2e*.py; not a test by itself. See README.md. The Python port of wbf-helpers.ps1 (issue #114):
the same wire format, the same `ok`/`FAIL`/`RESULT` lines, the same output directory, on Windows and Linux.
"""

from __future__ import annotations

import json
import os
import re
import signal
import subprocess
import sys
import time
import traceback
import urllib.error
import urllib.request
from pathlib import Path

from websockets.exceptions import ConnectionClosed
from websockets.protocol import State
from websockets.sync.client import connect as _ws_connect

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")

IS_WINDOWS = os.name == "nt"
REPO = Path(__file__).resolve().parent.parent.parent
# Everything a run writes (configs, databases, server logs, results) goes under target/, never next to the scripts.
S = REPO / "target" / "e2e-runs"
B = "http://127.0.0.1:8015"
WS_URL = "ws://127.0.0.1:8015/_wbf/v1/ws"


def default_exe() -> Path:
    if os.environ.get("E2E_EXE"):
        return Path(os.environ["E2E_EXE"])
    return REPO / "target" / "e2e" / ("tuwunel.exe" if IS_WINDOWS else "tuwunel")


# ---------- CRC-32C (Castagnoli, reflected 0x82F63B78) ----------
def _crc_table() -> list[int]:
    table = []
    for i in range(256):
        c = i
        for _ in range(8):
            c = (c >> 1) ^ 0x82F63B78 if c & 1 else c >> 1
        table.append(c)
    return table


_CRC_TABLE = _crc_table()


def crc32c(data: bytes) -> int:
    c = 0xFFFFFFFF
    table = _CRC_TABLE
    for byte in data:
        c = table[(c ^ byte) & 0xFF] ^ (c >> 8)
    return c ^ 0xFFFFFFFF


def be32(v: int) -> bytes:
    return (v & 0xFFFFFFFF).to_bytes(4, "big")


def be64(v: int) -> bytes:
    return (v & 0xFFFFFFFFFFFFFFFF).to_bytes(8, "big")


def rd_be32(b: bytes, at: int) -> int:
    return int.from_bytes(b[at:at + 4], "big")


def rd_be64(b: bytes, at: int) -> int:
    return int.from_bytes(b[at:at + 8], "big")


def utf8(s: str) -> bytes:
    return s.encode("utf-8")


def to_json(obj) -> str:
    return json.dumps(obj, separators=(",", ":"), ensure_ascii=False)


# ---------- pack encode / decode ----------
# kinds: Control=1 Upload=3 Download=4 ; flags: META_ENCRYPTED=1 WANT_ACK=2 IS_RESPONSE=4
def new_pack(kind: int, subtype: int, flags: int, id: int, seq: int, meta: bytes | None, data: bytes | None) -> bytes:
    meta = meta or b""
    data = data or b""
    head = bytes([1, kind, subtype, flags]) + be64(id) + be32(seq) + be32(len(meta)) + meta
    return head + be32(crc32c(head)) + be32(len(data)) + data + be32(crc32c(data))


# The header's id carries its own type in the first byte (wire-format 2.2), so a client cannot
# put an upload id where a subscription's number goes without the server saying so.
def wbf_id(type: int, value: int) -> int:
    return (type << 56) + value


# 0x01: a conversation number this client picked (Recent, Subscribe, Device/*).
def conv(n: int) -> int:
    return wbf_id(1, n)


# 0x02: an event's position — how a draft names its anchor.
def anchor(g: int) -> int:
    return wbf_id(2, g)


def json_pack(kind: int, subtype: int, id: int, seq: int, obj, data: bytes | None = None) -> bytes:
    meta = utf8(to_json(obj)) if obj is not None else b""
    return new_pack(kind, subtype, 0, id, seq, meta, data)


def hello_pack(client: str, seq: int = 1) -> bytes:
    return json_pack(1, 1, 0, seq, {"protocol": 1, "client": client, "features": []})


class Pack:
    """A decoded pack. `meta` is the parsed JSON (or None); `m(key)` reads one field and is None when absent."""

    closed = False

    def __init__(self, raw: bytes):
        self.error = None
        self.http = None
        self.events: list = []
        if len(raw) < 32:
            self.error = f"too short ({len(raw)})"
            self.version = self.kind = self.subtype = self.flags = self.id = self.seq = None
            self.meta, self.meta_text, self.data = None, "", b""
            self.meta_crc_ok = self.data_crc_ok = False
            return
        mlen = rd_be32(raw, 16)
        meta = raw[20:20 + mlen]
        dlen_at = 20 + mlen + 4
        dlen = rd_be32(raw, dlen_at)
        dstart = dlen_at + 4
        self.version, self.kind, self.subtype, self.flags = raw[0], raw[1], raw[2], raw[3]
        self.id = rd_be64(raw, 4)
        self.seq = rd_be32(raw, 12)
        self.data = raw[dstart:dstart + dlen]
        self.meta_text = meta.decode("utf-8", errors="replace") if mlen else ""
        self.meta = None
        if self.meta_text:
            try:
                self.meta = json.loads(self.meta_text)
            except ValueError:
                pass
        self.meta_crc_ok = rd_be32(raw, 20 + mlen) == crc32c(raw[:20 + mlen])
        self.data_crc_ok = rd_be32(raw, dstart + dlen) == crc32c(self.data)

    def m(self, key: str):
        return self.meta.get(key) if isinstance(self.meta, dict) else None


class Closed:
    """What a receive returns when the server closed the connection; `code` is the close code (1006: no close frame)."""

    closed = True
    kind = subtype = id = seq = None
    meta = None
    meta_text = ""
    data = b""
    events: list = []

    def __init__(self, code: int, reason: str = ""):
        self.code = code
        self.reason = reason

    def m(self, key: str):
        return None

    def __str__(self):
        return f"{self.code}{' ' + self.reason if self.reason else ''}"


def describe(p) -> str:
    if p is None:
        return "nothing"
    if p.closed:
        return f"closed code={p}"
    k = {2: "Ack", 3: "Error", 5: "Pong"}.get(p.subtype, f"sub{p.subtype}")
    return f"{k} http={p.http} id={p.id} seq={p.seq} meta={p.meta_text} data={len(p.data)}B"


def push_events(data: bytes) -> list:
    """The events of a Push or Batch pack: each one length-prefixed JSON."""
    events, at = [], 0
    while at + 4 <= len(data):
        n = rd_be32(data, at)
        at += 4
        events.append(json.loads(data[at:at + n].decode("utf-8")))
        at += n
    return events


# ---------- WebSocket ----------
def ws_open(tok: str | None = None, forwarded: str | None = None):
    headers = {}
    if tok:
        headers["Authorization"] = f"Bearer {tok}"
    # Only e2e17 passes this: it is how a same-host proxy names the real client,
    # and whether the server believes it is the whole of the localhost_ip rule.
    if forwarded:
        headers["X-Forwarded-For"] = forwarded
    # ⚠️ No keepalive pings and no compression: the scripts decide every frame the server sees.
    # No size cap: a Recent batch of 60 KB events is well over the library's 1 MiB default.
    return _ws_connect(WS_URL, additional_headers=headers, compression=None, proxy=None,
                       ping_interval=None, max_size=None, open_timeout=10, close_timeout=2)


def ws_is_open(ws) -> bool:
    return ws.state is State.OPEN


def ws_send(ws, pack: bytes) -> None:
    ws.send(pack)


def ws_send_text(ws, text: str) -> None:
    ws.send(text)


def ws_close(*sockets) -> None:
    for ws in sockets:
        if ws is None:
            continue
        try:
            ws.close()
        except Exception:
            pass


def recv_or_none(ws, ms: int):
    """One frame within `ms`, decoded; None when nothing arrives (a quiet channel is a result here, not a hang),
    a Closed when the server closed the connection.

    Unlike .NET's ClientWebSocket, a receive that times out here loses nothing: the library reads into its
    own queue and the next call gets the frame (the PowerShell scripts had to keep the pending task by hand).
    """
    try:
        raw = ws.recv(timeout=ms / 1000)
    except TimeoutError:
        return None
    except ConnectionClosed as e:
        if e.rcvd is not None:
            return Closed(e.rcvd.code, e.rcvd.reason)
        return Closed(1006, "no close frame")
    if isinstance(raw, str):
        raw = raw.encode("utf-8")
    p = Pack(raw)
    p.http = "ws"
    return p


def ws_recv(ws, ms: int = 30000):
    p = recv_or_none(ws, ms)
    if p is None:
        raise TimeoutError(f"no frame within {ms} ms")
    return p


# ---------- HTTP ----------
def _request(method: str, path: str, body: bytes | None, tok: str | None, content_type: str | None, timeout: float):
    req = urllib.request.Request(B + path, data=body, method=method.upper())
    if tok:
        req.add_header("Authorization", f"Bearer {tok}")
    if content_type:
        req.add_header("Content-Type", content_type)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def http(method: str, path: str, body=None, tok: str | None = None) -> dict:
    """Status and body of every response (no exception on a non-2xx) -- for a check that expects a 404 or an errcode."""
    payload = utf8(to_json(body)) if body is not None else None
    status, raw = _request(method, path, payload, tok, "application/json" if payload is not None else None, 15)
    text = raw.decode("utf-8", errors="replace")
    try:
        parsed = json.loads(text) if text else None
    except ValueError:
        parsed = None
    return {"status": status, "text": text, "json": parsed}


def send_pack(pack: bytes, tok: str | None = None) -> Pack:
    status, raw = _request("POST", "/_wbf/v1/pack", pack, tok, "application/octet-stream", 30)
    p = Pack(raw)
    p.http = status
    return p


def get_bytes(mxc: str, tok: str) -> dict:
    media_id = re.sub(r"^mxc://localhost/", "", mxc)
    status, raw = _request("GET", f"/_matrix/client/v1/media/download/localhost/{media_id}", None, tok, None, 30)
    return {"status": status, "bytes": raw}


def _probe(timeout: float = 2) -> bool:
    try:
        with urllib.request.urlopen(B + "/_matrix/client/versions", timeout=timeout):
            return True
    except urllib.error.HTTPError:
        return True
    except OSError:
        return False


def _strip_colour(text: str) -> str:
    return re.sub(r"\x1b\[[0-9;]*m", "", text)


class Suite:
    """One script's run: its output directory, results.txt, the pass/fail counters and the server it started."""

    def __init__(self, name: str):
        self.name = name
        self.exe = default_exe()
        self.out = S / f"{name}-out"
        self.out.mkdir(parents=True, exist_ok=True)
        self.result = self.out / "results.txt"
        self.result.write_text("", encoding="utf-8")
        self.passed = 0
        self.failed = 0
        self._servers: list[subprocess.Popen] = []

    # ---------- results ----------
    def log(self, m: str) -> None:
        with self.result.open("a", encoding="utf-8") as f:
            f.write(m + "\n")
        print(m, flush=True)

    def check(self, name: str, ok, detail="") -> bool:
        """`ok` and `detail` may be callables. Only then is a condition that raises (an index into an empty list,
        a field of a reply that never came) a FAIL with the exception in the detail: a plain value is computed
        before `check` is called, so if *that* raises, the run ends as ABORTED. ⇒ Pass a lambda whenever the
        condition reaches into something that may not be there."""
        if callable(ok):
            try:
                ok = ok()
            except Exception as e:
                ok, detail = False, f"(check raised {type(e).__name__}: {e}) " + (self._detail(detail))
        if callable(detail):
            detail = self._detail(detail)
        ok = bool(ok)
        if ok:
            self.passed += 1
            self.log(f"  ok   {name}  {detail}")
        else:
            self.failed += 1
            self.log(f"  FAIL {name}  {detail}")
        return ok

    @staticmethod
    def _detail(detail) -> str:
        if not callable(detail):
            return str(detail)
        try:
            return str(detail())
        except Exception as e:
            return f"(detail raised {type(e).__name__}: {e})"

    def run(self, body) -> None:
        """Runs the script body, always stops every server it started, prints RESULT and exits non-zero on a FAIL
        (or on an exception, which the PowerShell scripts left as a hang with a live server)."""
        aborted = False
        try:
            body()
        except Exception:
            aborted = True
            self.log("  !! aborted: " + traceback.format_exc().rstrip().replace("\n", "\n  !! "))
        finally:
            for p in list(self._servers):
                self.stop_server(p, settle=False)
        self.log(f"################ RESULT: pass={self.passed} fail={self.failed}{' ABORTED' if aborted else ''} ################")
        sys.exit(1 if self.failed or aborted else 0)

    # ---------- HTTP with logging ----------
    def api(self, method: str, path: str, body=None, tok: str | None = None):
        """The parsed JSON of a 2xx; None (and a `!! FAILED` line) for anything else. `body` is JSON text or an object."""
        if body is not None and not isinstance(body, (str, bytes)):
            body = to_json(body)
        if isinstance(body, str):
            body = utf8(body)
        try:
            status, raw = _request(method, path, body, tok, "application/json" if body is not None else None, 15)
        except OSError as e:
            self.log(f"  !! FAILED {method} {path} : {e}")
            return None
        if not 200 <= status < 300:
            self.log(f"  !! FAILED {method} {path} : HTTP {status} {raw[:300].decode('utf-8', errors='replace')}")
            return None
        try:
            return json.loads(raw) if raw else {}
        except ValueError:
            return None

    # ---------- server ----------
    def config_path(self, stem: str) -> Path:
        S.mkdir(parents=True, exist_ok=True)
        return S / f"{stem}.toml"

    def fresh_db(self, stem: str) -> Path:
        import shutil
        db = S / stem
        shutil.rmtree(db, ignore_errors=True)
        db.mkdir(parents=True)
        return db

    def read_server_log(self, tag: str) -> str:
        parts = []
        for ext in ("out", "err"):
            f = self.out / f"{tag}.{ext}"
            if f.exists():
                parts.append(f.read_text(encoding="utf-8", errors="replace"))
        return _strip_colour("\n".join(parts))

    def _spawn(self, args: list[str], tag: str) -> subprocess.Popen:
        out = open(self.out / f"{tag}.out", "wb")
        err = open(self.out / f"{tag}.err", "wb")
        kwargs = {}
        if not IS_WINDOWS:
            kwargs["start_new_session"] = True
        p = subprocess.Popen([str(self.exe)] + args, stdout=out, stderr=err, stdin=subprocess.DEVNULL, **kwargs)
        out.close()
        err.close()
        self._servers.append(p)
        return p

    def start_server(self, cfg: Path, tag: str) -> subprocess.Popen:
        # 🚨 Refuse to start when something already answers on the port: a run that died leaves its server
        # behind, the new one cannot bind and exits, and the probe below happily gets an answer from the old
        # one -- every check then runs against the wrong server and database (2026-09-13, e2e11).
        if _probe():
            self.log(f"  !! a server already answers on {B} before {tag} started ({self._stale()}) -- a previous run did not stop it")
            raise RuntimeError(f"port already in use before starting {tag}")
        p = self._spawn(["-c", str(cfg)], tag)
        for _ in range(40):
            time.sleep(0.5)
            if p.poll() is not None:
                raise RuntimeError(f"{tag} exited (code {p.returncode}) before answering")
            if _probe():
                return p
        self.log(f"  !! server probe failed 40 times ({tag}); process exited={p.poll() is not None}")
        raise RuntimeError(f"server did not come up ({tag})")

    def stop_server(self, p: subprocess.Popen | None, settle: bool = True) -> None:
        """Stops the server and does not return until no server of this binary is left (README: a live server
        holds the port and the database and makes a finished run look like a hang)."""
        if settle:
            time.sleep(3)
        if p is not None and p.poll() is None:
            p.kill()
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                pass
        if p in self._servers:
            self._servers.remove(p)
        for _ in range(20):
            alive = self._alive_servers()
            if not alive:
                break
            for pid in alive:
                self._kill_pid(pid)
            time.sleep(0.5)
        if settle:
            time.sleep(1)

    def exec_commands(self, cfg: Path, cmds: list[str], tag: str) -> list[str]:
        """Runs admin commands with the server's --execute (it starts, runs them, shuts down); returns the
        interesting lines of its output."""
        args = ["-c", str(cfg)]
        for c in cmds + ["server shutdown"]:
            args += ["--execute", c]
        p = self._spawn(args, tag)
        try:
            p.wait(timeout=120)
        except subprocess.TimeoutExpired:
            pass
        self.stop_server(p)
        lines = self.read_server_log(tag).split("\n")
        return [re.sub(r"^\S+\s+", "", l) for l in lines if re.search(r"reference|deleted|rror|panicked|Sweeping", l)]

    # Sweeping is by the binary under test, not by any process called tuwunel: only one script runs at a
    # time (README), so one still up after its script asked it to stop is that script's.
    def _alive_servers(self) -> list[int]:
        if IS_WINDOWS:
            name = self.exe.name
            out = subprocess.run(["tasklist", "/FI", f"IMAGENAME eq {name}", "/FO", "CSV", "/NH"],
                                 capture_output=True, text=True).stdout
            return [int(row.split('","')[1]) for row in out.splitlines() if row.startswith(f'"{name}"')]
        want = os.path.realpath(self.exe)
        pids = []
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            try:
                if os.path.realpath(f"/proc/{entry}/exe") == want:
                    pids.append(int(entry))
            except OSError:
                continue
        return pids

    def _stale(self) -> str:
        return "; ".join(f"pid {pid}" for pid in self._alive_servers()) or "not this binary"

    @staticmethod
    def _kill_pid(pid: int) -> None:
        try:
            if IS_WINDOWS:
                subprocess.run(["taskkill", "/F", "/PID", str(pid)], capture_output=True)
            else:
                os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
