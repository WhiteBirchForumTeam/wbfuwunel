. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
$OUT = "$S\e2e11-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$GSEQKEY = 'org.wbftw.wbfuwunel.g_seq'
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
function Write-Config11([string]$db, [int]$queueLen = 32, [int]$draftMaxMembers = 0) {
  $cfg = "$S\e2e11.toml"
  # ⚠️ A to-device window must fit in the send queue (wbf-to-device.md 7, asserted at startup), so
  # shrinking the queue for the backpressure scenario shrinks the window with it: a queue of four
  # packs takes four packs of a hundred. Without this the server refuses to start, correctly.
  $deviceLimit = $queueLen * 100
  @('[global]','server_name = "localhost"',('database_path = "' + ($db.Replace([string][char]92, '/')) + '"'),'port = 8015','address = ["127.0.0.1"]',
    'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
    ('wbf_ws_send_queue_len = ' + $queueLen),('wbf_device_fetch_max_limit = ' + $deviceLimit),'wbf_ws_idle_timeout = 120','log = "info"') +
    $(if ($draftMaxMembers -gt 0) { @(('wbf_draft_max_room_members = ' + $draftMaxMembers)) } else { @() }) -join "`n" | Set-Content -Path $cfg -Encoding ascii
  $cfg
}
function GSeqOf($ev) { if ($ev.unsigned -and ($ev.unsigned.PSObject.Properties.Name -contains $GSEQKEY)) { [int64]$ev.unsigned.$GSEQKEY } else { $null } }
function Register($name) { Api Post '/_matrix/client/v3/register' ('{"username":"' + $name + '","password":"pw-pw-pw-pw","auth":{"type":"m.login.dummy"}}') $null }
function Create-Room($tok, $name) { (Api Post '/_matrix/client/v3/createRoom' (@{ preset = 'private_chat'; name = $name } | ConvertTo-Json -Compress) $tok).room_id }
function Invite($room, $who, $tok) { $null = Api Post "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/invite" (@{ user_id = $who } | ConvertTo-Json -Compress) $tok }
function Join($room, $tok) { $null = Api Post "/_matrix/client/v3/join/$([uri]::EscapeDataString($room))" '{}' $tok }
function Leave($room, $tok) { $null = Api Post "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/leave" '{}' $tok }
function Kick($room, $who, $tok) { $null = Api Post "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/kick" (@{ user_id = $who } | ConvertTo-Json -Compress) $tok }
# Sends an event of an arbitrary type through the ordinary path; $null when the server refuses.
function Send-Msg-Typed($room, $type, $content, $tok) {
  $txn = [guid]::NewGuid().ToString('N')
  (Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/send/$([uri]::EscapeDataString($type))/$txn" ($content | ConvertTo-Json -Compress) $tok).event_id
}
function Send-Msg($room, $body, $tok) {
  $txn = [guid]::NewGuid().ToString('N')
  (Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/send/m.room.message/$txn" (@{ msgtype = 'm.text'; body = $body } | ConvertTo-Json -Compress) $tok).event_id
}
# Ws-Recv with a deadline: returns $null when nothing arrives within $ms (a quiet channel is a result here, not a hang).
# A ReceiveAsync that timed out is NOT abandoned: .NET keeps it pending and it would eat the next frame, so it is kept
# per socket and waited on again next time (an abandoned receive is how the first run of this script hung).
$script:PendingRecv = @{}; $script:PendingBuf = @{}
function Recv-Or-Null($ws, [int]$ms) {
  $key = $ws.GetHashCode()
  $stream = New-Object System.IO.MemoryStream
  do {
    if ($script:PendingRecv.ContainsKey($key)) { $t = $script:PendingRecv[$key]; $buf = $script:PendingBuf[$key] }
    else { $buf = New-Object byte[] 262144; $t = $ws.ReceiveAsync([ArraySegment[byte]]$buf, [Threading.CancellationToken]::None) }
    if (-not $t.Wait($ms)) { $script:PendingRecv[$key] = $t; $script:PendingBuf[$key] = $buf; return $null }
    $script:PendingRecv.Remove($key); $script:PendingBuf.Remove($key)
    $r = $t.Result
    if ($r.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close) { return @{ closed = $true; code = "$($r.CloseStatus)" } }
    $stream.Write($buf, 0, $r.Count)
  } while (-not $r.EndOfMessage)
  $p = Read-Pack ($stream.ToArray()); $p.http = 'ws'; $p
}
# Send one pack and wait for one frame, at most 10 s; a silent server fails the run instead of hanging it.
# Push packs that were already queued for this connection are skipped: the caller wants the reply.
function Call($ws, [byte[]]$pack) {
  Ws-Send $ws $pack
  for ($i = 0; $i -lt 50; $i++) {
    $p = Recv-Or-Null $ws 10000
    if ($null -eq $p) { throw 'no reply within 10 s' }
    if ($p.closed) { throw "server closed the connection: $($p.code)" }
    if (-not ($p.kind -eq 0x14 -and $p.subtype -eq 6)) { return $p }
  }
  throw 'only pushes, no reply'
}
function Push-Events([byte[]]$data) {
  $events = @(); $at = 0
  while ($at + 4 -le $data.Length) { $len = [int](RdBE32 $data $at); $at += 4; $events += ,([Text.Encoding]::UTF8.GetString($data, $at, $len) | ConvertFrom-Json); $at += $len }
  $events
}
# Collects Push packs until the channel is quiet for $quietMs; returns the packs (each with .events).
function Drain-Pushes($ws, [int]$quietMs = 1500) {
  $packs = @()
  while ($true) {
    $p = Recv-Or-Null $ws $quietMs
    if ($null -eq $p -or $p.closed) { break }
    if ($p.kind -eq 0x14 -and $p.subtype -eq 6) { $p.events = @(Push-Events ([byte[]]$p.data)); $packs += ,$p } else { $p.events = @(); $packs += ,$p }
  }
  # Callers wrap the result in @(): a one-element array is unrolled on return and re-wrapped there, an empty one
  # becomes an empty array, longer ones pass through. (Returning `,$packs` on top of @() double-wraps.)
  $packs
}
function Subscribe($ws, [uint64]$id, $rooms, $cgSeq) {
  $meta = @{}; if ($null -ne $rooms) { $meta.rooms = @($rooms) }; if ($null -ne $cgSeq) { $meta.cg_seq = $cgSeq }
  Call $ws (Json-Pack 0x14 4 (Conv $id) 0 $meta $null)
}
function Unsubscribe($ws, [uint64]$id, $rooms) {
  $meta = @{}; if ($null -ne $rooms) { $meta.rooms = @($rooms) }
  Call $ws (Json-Pack 0x14 5 (Conv $id) 1 $meta $null)
}
function Ids($packs) { @($packs | ForEach-Object { $_.events } | ForEach-Object { $_.event_id }) }
# A Stream pack (0x02): the meta is the room id itself, as UTF-8 text rather than JSON, and the
# header carries which draft (id) and the author's piece counter (seq).
function Stream-Pack([byte]$subtype, [string]$room, [uint64]$draftId, [uint32]$seq, [byte[]]$data) {
  New-Pack 0x02 $subtype 0 $draftId $seq ([Text.Encoding]::UTF8.GetBytes($room)) $data
}
function Draft-Pack([string]$room, [uint32]$seq) { Stream-Pack 0x01 $room 0 $seq $null }
# A piece (Keypoint/Delta/Append): its data starts with the seq it follows, big-endian
# (streaming-messages.md 3.0). Keypoint's prev is 0 — it replaces the whole buffer.
function Piece-Pack([byte]$subtype, [string]$room, [uint64]$draftId, [uint32]$seq, [uint32]$prev, [byte[]]$payload) {
  if ($null -eq $payload) { $payload = @() }
  Stream-Pack $subtype $room $draftId $seq ([byte[]]((BE32 $prev) + $payload))
}

# ================= Scenario 1: subscribe, push, membership hooks, ignore =================
Log '################ Scenario 1: channels and Push ################'
$db1 = "$S\e2e11db-1"; Remove-Item -Recurse -Force $db1 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db1 | Out-Null
$cfg = Write-Config11 $db1
$server = Start-Server $cfg 's1'
$regA = Register 'alice'; $tokA = $regA.access_token
$regB = Register 'bob'; $tokB = $regB.access_token
$regC = Register 'carol'; $tokC = $regC.access_token
$r1 = Create-Room $tokA 'one'
Invite $r1 $regB.user_id $tokA; Join $r1 $tokB
Invite $r1 $regC.user_id $tokA; Join $r1 $tokC

# [1.1] alice: one subscribed connection, one not; bob sends -> only the subscribed one gets a Push
$wsSub = Ws-Open $tokA
$wsNot = Ws-Open $tokA
$hello = Call $wsSub (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e11'; features = @() } $null)
Check '[1.0] Hello reports push and a connection_id' ((@($hello.meta.features) -contains 'push') -and $hello.meta.connection_id -gt 0) "features=$(@($hello.meta.features) -join ',') connection_id=$($hello.meta.connection_id)"
$ack = Subscribe $wsSub 7 $null $null
Check '[1.1a] account-wide Subscribe -> Ack with joined>=1, latest_g_seq, skipped=[]' ($ack.subtype -eq 2 -and $ack.meta.joined -ge 1 -and $ack.meta.latest_g_seq -gt 0 -and @($ack.meta.skipped).Count -eq 0) (Describe $ack)
$m1 = Send-Msg $r1 'hello from bob' $tokB
$pushes = @(Drain-Pushes $wsSub)
Check '[1.1b] bob sends -> the subscribed connection gets one Push with that event, id=7 seq=0, fs=ls=g_seq, gap=false' ($pushes.Count -eq 1 -and $pushes[0].subtype -eq 6 -and $pushes[0].id -eq (Conv 7) -and $pushes[0].seq -eq 0 -and @($pushes[0].events).Count -eq 1 -and $pushes[0].events[0].event_id -eq $m1 -and [int64]$pushes[0].meta.fs -eq (GSeqOf $pushes[0].events[0]) -and $pushes[0].meta.fs -eq $pushes[0].meta.ls -and $pushes[0].meta.gap -eq $false) "$(Describe $pushes[0]) events=$(@($pushes[0].events).Count) first=$($pushes[0].events[0].event_id) m1=$m1 g=$(GSeqOf $pushes[0].events[0])"
$nothing = Recv-Or-Null $wsNot 1000
Check '[1.1c] the unsubscribed connection got nothing' ($null -eq $nothing) ''

# [1.2] alice sends over the WS herself -> Ack (event_id) and a Push of the same event on the same connection
$sendMeta = @{ room_id = $r1; type = 'm.room.message'; txn_id = [guid]::NewGuid().ToString('N') }
Ws-Send $wsSub (Json-Pack 0x14 2 0 5 $sendMeta ([Text.Encoding]::UTF8.GetBytes('{"msgtype":"m.text","body":"from alice over ws"}')))
# The Push is queued inside append, before the handler's Ack: two frames, Push first.
$frames = @(Drain-Pushes $wsSub)
$ackSend = @($frames | Where-Object { $_.kind -eq 1 -and $_.subtype -eq 2 })
$pushes = @($frames | Where-Object { $_.kind -eq 0x14 -and $_.subtype -eq 6 })
Check '[1.2] own Event/Send -> Ack with event_id and a Push of the same event (seq 1), Push first' ($ackSend.Count -eq 1 -and $ackSend[0].meta.event_id -and $pushes.Count -eq 1 -and $pushes[0].seq -eq 1 -and $pushes[0].events[0].event_id -eq $ackSend[0].meta.event_id -and $frames[0].subtype -eq 6) "frames=$(($frames | ForEach-Object { '{0}/{1}' -f $_.kind, $_.subtype }) -join ',') ack=$($ackSend[0].meta.event_id) push=$(Ids $pushes)"
$ackSend = $ackSend[0]

# [1.3] Subscribe with cg_seq: events after the watermark arrive first, then live ones
$mark = GSeqOf $pushes[0].events[0]
$m2 = Send-Msg $r1 'after mark 1' $tokB
$m3 = Send-Msg $r1 'after mark 2' $tokB
$null = @(Drain-Pushes $wsSub 800)   # clear the live pushes of m2/m3 on the first connection
$wsLate = Ws-Open $tokA
$ackLate = Subscribe $wsLate 8 $null $mark
$catch = @(Drain-Pushes $wsLate)
$caught = Ids $catch
Check '[1.3] Subscribe cg_seq=<mark> -> the two newer events are pushed (catch-up), newest first' ($ackLate.subtype -eq 2 -and $caught.Count -eq 2 -and $caught[0] -eq $m3 -and $caught[1] -eq $m2) "caught=$($caught -join ',')"
$m4 = Send-Msg $r1 'live after catch-up' $tokB
$live = @(Drain-Pushes $wsLate)
Check '[1.3b] then live events keep coming on that connection' ($live.Count -eq 1 -and $live[0].events[0].event_id -eq $m4) (Ids $live)

# [1.4] a room alice joins after subscribing is followed (account-wide); a named-room subscription is not
$wsNamed = Ws-Open $tokA
$ackNamed = Subscribe $wsNamed 9 @($r1) $null
$r2 = Create-Room $tokB 'two'
Invite $r2 $regA.user_id $tokB; Join $r2 $tokA
$m5 = Send-Msg $r2 'in room two' $tokB
$inSub = @(Drain-Pushes $wsSub)
$inNamed = @(Drain-Pushes $wsNamed 800)
$namedGot = @($inNamed | Where-Object { (Ids @($_)) -contains $m5 }).Count
Check '[1.4a] account-wide connection gets the new room''s event' ((Ids $inSub) -contains $m5) (Ids $inSub)
Check '[1.4b] named-room connection does not (it only asked for room one)' ($ackNamed.meta.joined -eq 1 -and $namedGot -eq 0) "named joined=$($ackNamed.meta.joined) got=$namedGot"

# [1.5] leaving a room stops its pushes; kick stops them too
Leave $r2 $tokA
$m6 = Send-Msg $r2 'after alice left' $tokB
$afterLeave = @(Drain-Pushes $wsSub 1000)
$leaveEvents = Ids $afterLeave
Check '[1.5a] after leaving room two, its later message is not pushed (the leave itself may be)' (-not ($leaveEvents -contains $m6)) "got=$($leaveEvents -join ',')"
$wsB = Ws-Open $tokB
$null = Subscribe $wsB 10 $null $null
Kick $r1 $regB.user_id $tokA
$m7 = Send-Msg $r1 'after bob was kicked' $tokC
$bobAfter = @(Drain-Pushes $wsB 1000)
Check '[1.5b] after being kicked, bob is not pushed the room''s later message' (-not ((Ids $bobAfter) -contains $m7)) "got=$((Ids $bobAfter) -join ',')"

# [1.6] ignoring carol drops her events for alice
$null = Api Put "/_matrix/client/v3/user/$([uri]::EscapeDataString($regA.user_id))/account_data/m.ignored_user_list" (@{ ignored_users = @{ $regC.user_id = @{} } } | ConvertTo-Json -Compress -Depth 4) $tokA
Start-Sleep -Milliseconds 300
$null = @(Drain-Pushes $wsSub 800)
$m8 = Send-Msg $r1 'carol, ignored' $tokC
$ignored = @(Drain-Pushes $wsSub 1000)
Check '[1.6] an ignored sender''s event is not pushed' (-not ((Ids $ignored) -contains $m8)) "got=$((Ids $ignored) -join ',')"

# [1.7] Unsubscribe: named room, then all; idempotent
$u1 = Unsubscribe $wsSub 7 @($r1)
$u2 = Unsubscribe $wsSub 7 @($r1)
$m9 = Send-Msg $r1 'after unsubscribe' $tokA
$afterUnsub = @(Drain-Pushes $wsSub 1000)
Check '[1.7a] Unsubscribe room one (twice, second is a no-op) -> Ack both, no more pushes for it' ($u1.subtype -eq 2 -and $u2.subtype -eq 2 -and -not ((Ids $afterUnsub) -contains $m9)) "got=$((Ids $afterUnsub) -join ',')"
$u3 = Unsubscribe $wsLate 8 $null
Check '[1.7b] Unsubscribe all -> Ack' ($u3.subtype -eq 2) (Describe $u3)

# [1.8] HTTP: Subscribe is Unsupported
$http = Send-Pack (Json-Pack 0x14 4 (Conv 11) 0 @{} $null) $tokA
Check '[1.8] Subscribe over HTTP -> Error Unsupported' ($http.subtype -eq 3 -and $http.meta.code -eq 'Unsupported') (Describe $http)
foreach ($w in @($wsSub, $wsNot, $wsLate, $wsNamed, $wsB)) { try { $w.Dispose() } catch {} }
Stop-Server $server

# ================= Scenario 2: backpressure: a reader that stops reading gets a gap, never blocks the sender =================
Log '################ Scenario 2: gap under backpressure ################'
$db2 = "$S\e2e11db-2"; Remove-Item -Recurse -Force $db2 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db2 | Out-Null
$cfg2 = Write-Config11 $db2 4
$server = Start-Server $cfg2 's2'
$regA = Register 'alice'; $tokA = $regA.access_token
$regB = Register 'bob'; $tokB = $regB.access_token
$r1 = Create-Room $tokA 'one'
Invite $r1 $regB.user_id $tokA; Join $r1 $tokB
$wsA = Ws-Open $tokA
$null = Subscribe $wsA 1 $null $null
# alice stops reading; bob sends 40 messages: each must be acknowledged promptly (the push never blocks append)
$sw = [Diagnostics.Stopwatch]::StartNew()
# 60 KB bodies, 100 of them: small packs would all sit in the loopback socket buffers and the server queue would
# never fill; a few megabytes do fill it.
$pad = 'x' * 60000
$sent = @(); 1..100 | ForEach-Object { $sent += (Send-Msg $r1 "flood $_ $pad" $tokB) }
$elapsed = $sw.Elapsed.TotalSeconds
Check '[2.1] 100 sends of 60 KB while the subscriber is not reading all succeed quickly' ($sent.Count -eq 100 -and ($sent | Where-Object { -not $_ }).Count -eq 0 -and $elapsed -lt 60) "$([math]::Round($elapsed,1)) s"
# now read: the queue (4) plus the socket buffers held some, the rest were dropped
$got = @(Drain-Pushes $wsA 1500)
$gotIds = Ids $got
Check '[2.2a] fewer than 100 arrived (the rest were dropped, append never waited)' ($gotIds.Count -lt 100 -and $gotIds.Count -gt 0) "arrived=$($gotIds.Count)"
# gap is a flag on the NEXT push that gets through (the client cannot be told about a drop by a pack that was dropped)
$mAfter = Send-Msg $r1 'after the flood' $tokB
$next = @(Drain-Pushes $wsA 1500)
$gapSeen = @($next | Where-Object { $_.meta.gap -eq $true }).Count
Check '[2.2b] the next Push after the drops carries gap=true' ($gapSeen -ge 1 -and ((Ids $next) -contains $mAfter)) "next=$(($next | ForEach-Object { $_.meta.gap }) -join ',')"
$sent += $mAfter
# fill in with Recent from the watermark (the last event before the flood: the join); collect all windows
$recentIds = @()
Ws-Send $wsA (Json-Pack 0x14 1 (Conv 99) 0 @{ limit = 200; batch = 20 } $null)
# ⚠️ `$batch`, never `$b`: PowerShell variable names are case-insensitive, so `$b` is the helpers'
# `$B` — the base URL — and overwriting it makes the next Start-Server probe an address named after
# a hashtable. It stayed invisible while nothing after this line used $B (README, and e2e12 again).
do { $batch = Recv-Or-Null $wsA 5000; if ($null -eq $batch) { break }; if ($batch.kind -eq 0x14 -and $batch.subtype -eq 3) { $recentIds += @(Push-Events ([byte[]]$batch.data) | ForEach-Object { $_.event_id }) } } while ($batch.meta.r -ne 0)
$missing = @($sent | Where-Object { $recentIds -notcontains $_ }).Count
Check '[2.3] Recent returns every flooded event (the truth is in the DB, the push only hinted)' ($missing -eq 0) "missing=$missing recent=$($recentIds.Count)"
$wsA.Dispose()

Log '################ Scenario 3: the connection health counter (wire-format 2.1) ################'
# A pack whose kind byte is unassigned is framed perfectly: decode refuses it, but its sender speaks
# the protocol, so it must not spend the budget. PR #38's first version counted every decode error
# and would close this connection on the eighth one.
$wsH = Ws-Open $tokA
$unknownAnswers = 0
try {
  for ($i = 0; $i -lt 9; $i++) {
    # ⚠️ Not `$p`: at script scope that is the server's process handle, and overwriting it
    # meant the run's last Stop-Server was handed a pack, read `.Id` as 0 and killed nothing.
    $answer = Call $wsH (New-Pack 0x7f 1 0 0 $i @() @())
    if ($answer.subtype -eq 3 -and $answer.meta.code -eq 'UnknownKind' -and $answer.meta.code_id -eq 1101) { $unknownAnswers++ }
  }
} catch { Log "  (scenario 3: $($_.Exception.Message))" }
Check '[3.1] nine packs with an unassigned kind -> UnknownKind each, the connection stays open' ($unknownAnswers -eq 9 -and $wsH.State -eq 'Open') "answered=$unknownAnswers state=$($wsH.State)"
$hello = $null
try { $hello = Call $wsH (Json-Pack 1 1 0 90 @{ protocol = 1; client = 'e2e11.ps1'; features = @() } $null) } catch { Log "  (scenario 3 Hello: $($_.Exception.Message))" }
Check '[3.2] ... and the connection still answers a real request' ($null -ne $hello -and $hello.subtype -eq 2) $(if ($hello) { Describe $hello } else { 'no reply' })
# Eight frames that really do not decode (the meta is damaged, so the CRC fails): the budget runs out.
$corrupt = New-Pack 1 4 0 0 91 ([Text.Encoding]::UTF8.GetBytes('{"nonce":1}')) @()
$corrupt[20] = [byte](($corrupt[20] -bxor 0xff))
$errors = 0; $closed = $null
# A connection the earlier checks already lost cannot be tested further, and sending into it is how
# a failed run turns into a hung one: stop instead.
for ($i = 0; $i -lt 8 -and $null -eq $closed -and $wsH.State -eq 'Open'; $i++) {
  try { Ws-Send $wsH $corrupt } catch { Log "  (scenario 3 send: $($_.Exception.Message))"; break }
  $f = Recv-Or-Null $wsH 5000
  if ($null -eq $f) { continue }
  if ($f.closed) { $closed = $f } elseif ($f.subtype -eq 3 -and $f.meta.code_id -eq 1002) { $errors++ }
}
if ($null -eq $closed) { $f = Recv-Or-Null $wsH 5000; if ($f -and $f.closed) { $closed = $f } }
Check '[3.3] eight frames that do not decode -> Corrupt each, then the server closes the connection' ($null -ne $closed -and $errors -ge 7) "errors=$errors close=$($closed.code)"
$wsH.Dispose()
Stop-Server $server

# ================= Scenario 4: drafts (streaming-messages.md, 0x02 Stream) =================
Log '################ Scenario 4: drafts over the channel ################'
$db4 = "$S\e2e11db-4"; Remove-Item -Recurse -Force $db4 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db4 | Out-Null
# The member cap is set to 2 rather than filling a room with eleven accounts: the rule is
# "more members than the cap refuses a Draft", and two sides of that are two sides of it.
$cfg4 = Write-Config11 $db4 32 2
$server = Start-Server $cfg4 's4'
$regA = Register 'alice'; $tokA = $regA.access_token
$regB = Register 'bob'; $tokB = $regB.access_token
$regC = Register 'carol'; $tokC = $regC.access_token
$room = Create-Room $tokA 'drafts'
Invite $room $regB.user_id $tokA; Join $room $tokB

$wsA = Ws-Open $tokA; $null = Call $wsA (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e11-a'; features = @() } $null)
$wsB = Ws-Open $tokB; $null = Call $wsB (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e11-b'; features = @() } $null)
$null = Subscribe $wsA 40 $null $null
$null = Subscribe $wsB 41 $null $null

# [4.1] Draft writes a real, pushed anchor event
$draft = Call $wsA (Draft-Pack $room 1)
$draftId = [uint64]$draft.meta.id   # already carries its type byte (wire-format 2.2)
$anchorPushes = @(Drain-Pushes $wsB 1500)
$anchorTypes = @($anchorPushes | ForEach-Object { $_.events } | ForEach-Object { $_.type })
Check '[4.1a] Draft -> Ack with event_id and g_seq' ($draft.subtype -eq 2 -and $draft.meta.event_id -and $draftId -gt 0) (Describe $draft)
Check '[4.1b] the anchor is a real event: bob is pushed it, type org.wbftw.wbfuwunel.draft' ($anchorTypes -contains 'org.wbftw.wbfuwunel.draft') "types=$($anchorTypes -join ',')"

# The id's type byte, on this kind specifically (wire-format 2.2, PR #47). The unit
# tests cover the gate; these two cover the two mistakes a client actually makes:
# composing the id itself before the server has minted one, and sending a piece with
# the bare g_seq it read out of the Ack's meta instead of the composed id beside it.
$earlyType = Call $wsA (Stream-Pack 0x01 $room (Anchor 7) 2 $null)
Check '[4.1c] a Draft naming an anchor it does not have yet -> InvalidRequest' `
  ($earlyType.subtype -eq 3 -and $earlyType.meta.code_id -eq 1201) (Describe $earlyType)
$bareId = Call $wsA (Piece-Pack 0x05 $room ([uint64]$draft.meta.g_seq) 1 0 ([Text.Encoding]::UTF8.GetBytes('x')))
Check '[4.1d] a piece carrying the bare g_seq instead of the composed id -> InvalidRequest' `
  ($bareId.subtype -eq 3 -and $bareId.meta.code_id -eq 1201) (Describe $bareId)
$null = Drain-Pushes $wsA 800

# [4.2] the three piece subtypes reach the room unchanged — and the author's own connection too
$appendData = [Text.Encoding]::UTF8.GetBytes('piece one')
$deltaData = [Text.Encoding]::UTF8.GetBytes('piece two')
$keyData = [Text.Encoding]::UTF8.GetBytes('the whole draft so far')
Ws-Send $wsA (Piece-Pack 0x05 $room $draftId 1 0 $appendData)
Ws-Send $wsA (Piece-Pack 0x04 $room $draftId 2 1 $deltaData)
Ws-Send $wsA (Piece-Pack 0x03 $room $draftId 3 0 $keyData)
$atB = @(Drain-Pushes $wsB 1500 | Where-Object { $_.kind -eq 0x02 })
$atA = @(Drain-Pushes $wsA 1500 | Where-Object { $_.kind -eq 0x02 })
$subtypesB = @($atB | ForEach-Object { $_.subtype })
$bodiesB = @($atB | ForEach-Object { [Text.Encoding]::UTF8.GetString(([byte[]]$_.data), 4, $_.data.Length - 4) })
$prevsB = @($atB | ForEach-Object { RdBE32 ([byte[]]$_.data) 0 })
$metaB = @($atB | ForEach-Object { $_.metaText })
Check '[4.2a] bob receives Append, Delta and Keypoint in order, data byte for byte' `
  (($subtypesB -join ',') -eq '5,4,3' -and ($bodiesB -join '|') -eq 'piece one|piece two|the whole draft so far') `
  "subtypes=$($subtypesB -join ',') bodies=$($bodiesB -join '|')"
Check '[4.2d] each piece names the one it follows; the Keypoint names none' (($prevsB -join ',') -eq '0,1,0') "prevs=$($prevsB -join ',')"
Check '[4.2b] the meta is the room id itself and the header carries the draft id and the piece counter' `
  (($metaB | Select-Object -Unique) -eq $room -and (@($atB | ForEach-Object { $_.id }) -join ',') -eq "$draftId,$draftId,$draftId" -and (@($atB | ForEach-Object { $_.seq }) -join ',') -eq '1,2,3') `
  "meta=$($metaB[0]) ids=$(@($atB | ForEach-Object { $_.id }) -join ',') seqs=$(@($atB | ForEach-Object { $_.seq }) -join ',')"
Check '[4.2c] the author gets its own pieces back (one broadcast, no special case)' (@($atA).Count -eq 3) "at the author=$(@($atA).Count)"

# [4.3] the size limit, both sides of it
$tooBig = [byte[]]::new(10241)
$justFits = [byte[]]::new(10236)   # plus the 4-byte prev is exactly the limit
$big = Call $wsA (Stream-Pack 0x03 $room $draftId 4 $tooBig)
Check '[4.3a] a piece over wbf_draft_max_piece_bytes -> TooLarge' ($big.subtype -eq 3 -and $big.meta.code_id -eq 1103) (Describe $big)
Ws-Send $wsA (Piece-Pack 0x03 $room $draftId 5 0 $justFits)
$fitted = @(Drain-Pushes $wsB 1500 | Where-Object { $_.kind -eq 0x02 })
Check '[4.3b] exactly the limit passes' (@($fitted).Count -eq 1 -and @($fitted)[0].data.Length -eq 10240) "packs=$(@($fitted).Count) bytes=$(@($fitted)[0].data.Length)"
$null = Drain-Pushes $wsA 800

# [4.4] the meta is a room id, not JSON
$badMeta = Call $wsA (New-Pack 0x02 0x05 0 $draftId 6 ([Text.Encoding]::UTF8.GetBytes('{"room_id":"!x:localhost"}')) ([byte[]]((BE32 0) + $appendData)))
Check '[4.4] meta that is not a room id -> InvalidRequest' ($badMeta.subtype -eq 3 -and $badMeta.meta.code_id -eq 1201) (Describe $badMeta)

# [4.4b] the chain itself: the three rules the server enforces without reading the content
$noPrev = Call $wsA (Stream-Pack 0x05 $room $draftId 7 ([Text.Encoding]::UTF8.GetBytes('ab')))
Check '[4.4b] a piece whose data is too short to carry prev -> InvalidRequest' ($noPrev.subtype -eq 3 -and $noPrev.meta.code_id -eq 1201) (Describe $noPrev)
$zeroSeq = Call $wsA (Piece-Pack 0x05 $room $draftId 0 0 $appendData)
Check '[4.4c] a piece numbered 0 -> InvalidRequest (0 is reserved for "no base")' ($zeroSeq.subtype -eq 3 -and $zeroSeq.meta.code_id -eq 1201) (Describe $zeroSeq)
$basedKeypoint = Call $wsA (Piece-Pack 0x03 $room $draftId 7 6 $keyData)
Check '[4.4d] a Keypoint that names a base -> InvalidRequest (it replaces the whole draft)' ($basedKeypoint.subtype -eq 3 -and $basedKeypoint.meta.code_id -eq 1201) (Describe $basedKeypoint)

# [4.5] Demand is broadcast like any other piece; a connection that did not subscribe gets nothing
$wsC = Ws-Open $tokC; $null = Call $wsC (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e11-c'; features = @() } $null)
Invite $room $regC.user_id $tokA; Join $room $tokC
$null = Subscribe $wsC 42 $null $null
$null = Drain-Pushes $wsA 800; $null = Drain-Pushes $wsB 800; $null = Drain-Pushes $wsC 800
Ws-Send $wsC (Stream-Pack 0x10 $room $draftId 1 $null)
$demandA = @(Drain-Pushes $wsA 1500 | Where-Object { $_.kind -eq 0x02 -and $_.subtype -eq 0x10 })
$demandC = @(Drain-Pushes $wsC 1500 | Where-Object { $_.kind -eq 0x02 -and $_.subtype -eq 0x10 })
Check '[4.5] a Demand reaches the author and the asker alike (no special routing)' (@($demandA).Count -eq 1 -and @($demandC).Count -eq 1) "author=$(@($demandA).Count) asker=$(@($demandC).Count)"
$null = Drain-Pushes $wsB 800

# [4.6] the piece counter is the client's; the server carries it and does not renumber
Ws-Send $wsA (Piece-Pack 0x05 $room $draftId 5 4 $appendData)
Ws-Send $wsA (Piece-Pack 0x05 $room $draftId 6 5 $appendData)
Ws-Send $wsA (Piece-Pack 0x05 $room $draftId 8 7 $appendData)
$counted = @(Drain-Pushes $wsB 1500 | Where-Object { $_.kind -eq 0x02 })
Check '[4.6] 5, 6, 8 arrive as 5, 6, 8 (the gap is the client''s to notice)' ((@($counted | ForEach-Object { $_.seq }) -join ',') -eq '5,6,8') "seqs=$(@($counted | ForEach-Object { $_.seq }) -join ',')"
$null = Drain-Pushes $wsA 800

# [4.7] who may write, and to what
$notAuthor = Call $wsB (Piece-Pack 0x05 $room $draftId 9 8 $appendData)
Check '[4.7a] a piece from somebody who is not the author -> Forbidden' ($notAuthor.subtype -eq 3 -and $notAuthor.meta.code_id -eq 1302) (Describe $notAuthor)
$noSuchDraft = Call $wsA (Piece-Pack 0x05 $room (Anchor 999999) 9 8 $appendData)
Check '[4.7b] a draft id nothing in the room has -> NotFound' ($noSuchDraft.subtype -eq 3 -and $noSuchDraft.meta.code_id -eq 1501) (Describe $noSuchDraft)
$notADraft = Call $wsA (Piece-Pack 0x05 $room (Anchor ([uint64](GSeqOf (Api Get "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/event/$(Send-Msg $room 'an ordinary message' $tokA)" $null $tokA)))) 9 8 $appendData)
Check '[4.7c] an event that is not a draft -> Conflict' ($notADraft.subtype -eq 3 -and $notADraft.meta.code_id -eq 1502) (Describe $notADraft)
$httpDraft = Send-Pack (Draft-Pack $room 9) $tokA
Check '[4.7d] Stream over HTTP -> Unsupported' ($httpDraft.subtype -eq 3 -and $httpDraft.meta.code_id -eq 1102) (Describe $httpDraft)
$null = Drain-Pushes $wsA 800; $null = Drain-Pushes $wsB 800; $null = Drain-Pushes $wsC 800

# [4.8] the room is now three people, and this server's cap is two
$tooBusy = Call $wsA (Draft-Pack $room 10)
Check '[4.8] a room past wbf_draft_max_room_members -> Conflict' ($tooBusy.subtype -eq 3 -and $tooBusy.meta.code_id -eq 1502) (Describe $tooBusy)

# [4.9] the throttle: 100 pieces at once is more than 30/s with a burst of 60
$refused = 0
for ($i = 0; $i -lt 100; $i++) { Ws-Send $wsA (Piece-Pack 0x05 $room $draftId ([uint32](100 + $i)) ([uint32](99 + $i)) $appendData) }
$answers = @(Drain-Pushes $wsA 2500)
$refused = @($answers | Where-Object { $_.kind -eq 0x01 -and $_.subtype -eq 3 -and $_.meta.code_id -eq 1401 }).Count
$relayed = @($answers | Where-Object { $_.kind -eq 0x02 }).Count
Check '[4.9] a hundred pieces at once: some are relayed, the rest are RateLimited' ($refused -gt 0 -and $relayed -gt 0 -and ($refused + $relayed) -eq 100) "relayed=$relayed rateLimited=$refused"
$null = Drain-Pushes $wsB 1500; $null = Drain-Pushes $wsC 1500

# [4.10] the client's own ending: Abandon redacts the anchor, and the draft is closed for good
$bye = Call $wsA (Stream-Pack 0x02 $room $draftId 201 $null)
$redactions = @(Drain-Pushes $wsB 2000 | ForEach-Object { $_.events } | Where-Object { $_.type -eq 'm.room.redaction' })
$afterAbandon = Call $wsA (Piece-Pack 0x05 $room $draftId 202 201 $appendData)
Check '[4.10a] Abandon -> Ack with the redaction event id' ($bye.subtype -eq 2 -and $bye.meta.redaction_event_id) (Describe $bye)
Check '[4.10b] subscribers see the redaction as an ordinary push' (@($redactions).Count -ge 1) "redactions=$(@($redactions).Count)"
Check '[4.10c] a piece for an abandoned draft -> Conflict' ($afterAbandon.subtype -eq 3 -and $afterAbandon.meta.code_id -eq 1502) (Describe $afterAbandon)

# [4.11] the seven holes the external review of 2026-09-12 found, each refused where it was let through.
# A second room, with two members: the first one is at the member cap, so no new draft can be
# opened there — and a draft that was refused would make every check below pass for the wrong
# reason.
$room2 = Create-Room $tokA 'drafts two'
Invite $room2 $regB.user_id $tokA; Join $room2 $tokB
$wsB2 = Ws-Open $tokB; $null = Call $wsB2 (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e11-b2'; features = @() } $null)
$null = Subscribe $wsB2 45 $null $null
$null = Drain-Pushes $wsA 800; $null = Drain-Pushes $wsB2 800
$draft2 = Call $wsA (Draft-Pack $room2 300)
$id2 = [uint64]$draft2.meta.id
Check '[4.11pre] the draft these checks need is open' ($draft2.subtype -eq 2 -and $draft2.meta.g_seq -gt 0) (Describe $draft2)
$null = Drain-Pushes $wsA 800; $null = Drain-Pushes $wsB2 800

# R7: a stranger must not learn, from the difference between the refusals, whether an event exists
$regD = Register 'dave'; $tokD = $regD.access_token
$wsD = Ws-Open $tokD; $null = Call $wsD (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e11-d'; features = @() } $null)
$probeOpen = Call $wsD (Stream-Pack 0x10 $room2 $id2 1 $null)
$probeMissing = Call $wsD (Stream-Pack 0x10 $room2 (Anchor 999999) 1 $null)
Check '[4.11a] a non-member probing an open draft and a missing one gets the same refusal' `
  ($probeOpen.subtype -eq 3 -and $probeMissing.subtype -eq 3 -and $probeOpen.meta.code_id -eq 1302 -and $probeMissing.meta.code_id -eq 1302) `
  "open=$($probeOpen.meta.code_id) missing=$($probeMissing.meta.code_id)"
$wsD.Dispose()

# R3: a Demand declares no data; a large one is refused and the connection closed, a small one is stripped
Ws-Send $wsB2 (Stream-Pack 0x10 $room2 $id2 1 ([Text.Encoding]::UTF8.GetBytes('ignored payload')))
$strippedAtA = @(Drain-Pushes $wsA 1500 | Where-Object { $_.kind -eq 0x02 -and $_.subtype -eq 0x10 })
Check '[4.11b] a Demand is relayed with its data stripped' (@($strippedAtA).Count -eq 1 -and @($strippedAtA)[0].data.Length -eq 0) `
  "packs=$(@($strippedAtA).Count) bytes=$(@($strippedAtA)[0].data.Length)"
$wsFat = Ws-Open $tokB; $null = Call $wsFat (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e11-fat'; features = @() } $null)
$fat = Call $wsFat (Stream-Pack 0x10 $room2 $id2 1 ([byte[]]::new(2048)))
$fatClose = Recv-Or-Null $wsFat 5000
Check '[4.11c] a Demand carrying 2 KiB -> InvalidRequest and the connection is closed' `
  ($fat.subtype -eq 3 -and $fat.meta.code_id -eq 1201 -and $null -ne $fatClose -and $fatClose.closed) `
  "code=$($fat.meta.code_id) closed=$($fatClose.closed) reason=$($fatClose.code)"
$wsFat.Dispose()

# flags: a Stream pack sets none of them
$flagged = Call $wsA (New-Pack 0x02 0x05 8 $id2 2 ([Text.Encoding]::UTF8.GetBytes($room2)) ([byte[]]((BE32 1) + $appendData)))
Check '[4.11d] a Stream pack with a flag set -> InvalidRequest' ($flagged.subtype -eq 3 -and $flagged.meta.code_id -eq 1201) (Describe $flagged)

# R6: the anchor's event type is the server's to write
$forged = Send-Msg-Typed $room 'org.wbftw.wbfuwunel.draft' @{ msgtype = 'org.wbftw.wbfuwunel.draft'; body = '(draft)' } $tokA
Check '[4.11e] an ordinary send of the anchor type -> refused, so the room cap cannot be walked around' ($null -eq $forged) "event_id=$forged"

# R5: ignoring somebody hides their draft pieces, as it hides their messages
$null = Api Put "/_matrix/client/v3/user/$([uri]::EscapeDataString($regB.user_id))/account_data/m.ignored_user_list" (@{ ignored_users = @{ $regA.user_id = @{} } } | ConvertTo-Json -Compress -Depth 4) $tokB
Start-Sleep -Seconds 1
$null = Drain-Pushes $wsA 800; $null = Drain-Pushes $wsB2 800
Ws-Send $wsA (Piece-Pack 0x05 $room2 $id2 2 1 $appendData)
$atIgnorer = @(Drain-Pushes $wsB2 1500 | Where-Object { $_.kind -eq 0x02 })
$atOther = @(Drain-Pushes $wsA 1500 | Where-Object { $_.kind -eq 0x02 })
Check '[4.11f] a piece from an ignored author does not reach the ignorer, and still reaches everyone else' `
  (@($atIgnorer).Count -eq 0 -and @($atOther).Count -eq 1) "ignorer=$(@($atIgnorer).Count) other=$(@($atOther).Count)"
$null = Api Put "/_matrix/client/v3/user/$([uri]::EscapeDataString($regB.user_id))/account_data/m.ignored_user_list" (@{ ignored_users = @{} } | ConvertTo-Json -Compress -Depth 4) $tokB

# R4: the room can take a voice away, and an open draft must not be a way around that.
$bobDraft = Call $wsB2 (Draft-Pack $room2 1)
$bobId = [uint64]$bobDraft.meta.id
$null = Drain-Pushes $wsB2 800
Ws-Send $wsB2 (Piece-Pack 0x05 $room2 $bobId 1 0 $appendData)
$beforeMute = @(Drain-Pushes $wsB2 1500 | Where-Object { $_.kind -eq 0x02 })
$null = Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room2))/state/m.room.power_levels/" (@{ users = @{ $regA.user_id = 100; $regB.user_id = -1 }; events_default = 0 } | ConvertTo-Json -Compress -Depth 4) $tokA
Start-Sleep -Seconds 1
$muted = Call $wsB2 (Piece-Pack 0x05 $room2 $bobId 2 1 $appendData)
Check '[4.11g] a piece before the room takes the author''s voice away is relayed' (@($beforeMute).Count -eq 1) "packs=$(@($beforeMute).Count)"
Check '[4.11h] and one after it -> Forbidden: an open draft is not a way around a mute' ($muted.subtype -eq 3 -and $muted.meta.code_id -eq 1302) (Describe $muted)
# Bob's voice goes back: the next check is about suspension, and a muted user cannot redact
# either (a redaction is an event too), which would make it pass for the wrong reason.
$null = Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room2))/state/m.room.power_levels/" (@{ users = @{ $regA.user_id = 100 }; events_default = 0 } | ConvertTo-Json -Compress -Depth 4) $tokA
Start-Sleep -Seconds 1

foreach ($w in @($wsA, $wsB, $wsC, $wsB2)) { try { $w.Dispose() } catch {} }

# R2 and R1 both need a restart: suspension is an admin endpoint (so somebody has to be made an
# admin, which is a command that runs with the server stopped), and the transaction id can only
# be shown to survive a restart by restarting.
Stop-Server $server
$null = Exec $cfg4 @("user make-user-admin $($regA.user_id)") 'admin'
$server = Start-Server $cfg4 's4b'

# R1: this is the first connection of a new run, so its number is 1 — the same number alice's
# first connection had. With the same request seq, the transaction id would be the one from the
# first run, and the Ack would carry that run's anchor (in the other room).
$wsA3 = Ws-Open $tokA; $null = Call $wsA3 (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e11-a3'; features = @() } $null)
$afterRestart = Call $wsA3 (Draft-Pack $room2 1)
Check '[4.11i] a Draft after a restart writes a new anchor instead of replaying the first run''s' `
  ($afterRestart.subtype -eq 2 -and [uint64]$afterRestart.meta.id -ne $draftId) `
  "g_seq=$($afterRestart.meta.g_seq) first run=$draftId"

# R2: a suspended account may abandon its draft but not write to it
$suspend = Api Put "/_matrix/client/v1/admin/suspend/$([uri]::EscapeDataString($regB.user_id))" (@{ suspended = $true } | ConvertTo-Json -Compress) $tokA
$wsB3 = Ws-Open $tokB; $null = Call $wsB3 (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e11-b3'; features = @() } $null)
$null = Subscribe $wsB3 46 $null $null
$suspendedPiece = Call $wsB3 (Piece-Pack 0x05 $room2 $bobId 3 1 $appendData)
Check '[4.11j] a suspended author cannot broadcast a piece' ($null -ne $suspend -and $suspendedPiece.subtype -eq 3 -and $suspendedPiece.meta.code_id -eq 1302) "suspend=$($suspend.suspended) $(Describe $suspendedPiece)"
$byeSuspended = Call $wsB3 (Stream-Pack 0x02 $room2 $bobId 301 $null)
Check '[4.11k] ... but may still abandon its own draft' ($byeSuspended.subtype -eq 2 -and $byeSuspended.meta.redaction_event_id) (Describe $byeSuspended)

$wsA3.Dispose(); $wsB3.Dispose()
Stop-Server $server

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"

# A pending ReceiveAsync or an undisposed socket can keep this process alive long after the
# last line is written — every batch run this session looked like a hang for that reason, with
# the results already on disk. Leave on purpose, and say in the exit code whether it passed:
# a FAIL used to be invisible to anything that only looked at the exit status.
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
