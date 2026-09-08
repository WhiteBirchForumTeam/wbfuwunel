. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
$OUT = "$S\e2e11-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$GSEQKEY = 'org.wbftw.wbfuwunel.g_seq'
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
function Write-Config11([string]$db, [int]$queueLen = 32) {
  $cfg = "$S\e2e11.toml"
  @('[global]','server_name = "localhost"',('database_path = "' + ($db.Replace([string][char]92, '/')) + '"'),'port = 8015','address = ["127.0.0.1"]',
    'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
    ('wbf_ws_send_queue_len = ' + $queueLen),'wbf_ws_idle_timeout = 120','log = "info"') -join "`n" | Set-Content -Path $cfg -Encoding ascii
  $cfg
}
function GSeqOf($ev) { if ($ev.unsigned -and ($ev.unsigned.PSObject.Properties.Name -contains $GSEQKEY)) { [int64]$ev.unsigned.$GSEQKEY } else { $null } }
function Register($name) { Api Post '/_matrix/client/v3/register' ('{"username":"' + $name + '","password":"pw-pw-pw-pw","auth":{"type":"m.login.dummy"}}') $null }
function Create-Room($tok, $name) { (Api Post '/_matrix/client/v3/createRoom' (@{ preset = 'private_chat'; name = $name } | ConvertTo-Json -Compress) $tok).room_id }
function Invite($room, $who, $tok) { $null = Api Post "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/invite" (@{ user_id = $who } | ConvertTo-Json -Compress) $tok }
function Join($room, $tok) { $null = Api Post "/_matrix/client/v3/join/$([uri]::EscapeDataString($room))" '{}' $tok }
function Leave($room, $tok) { $null = Api Post "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/leave" '{}' $tok }
function Kick($room, $who, $tok) { $null = Api Post "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/kick" (@{ user_id = $who } | ConvertTo-Json -Compress) $tok }
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
  Call $ws (Json-Pack 0x14 4 $id 0 $meta $null)
}
function Unsubscribe($ws, [uint64]$id, $rooms) {
  $meta = @{}; if ($null -ne $rooms) { $meta.rooms = @($rooms) }
  Call $ws (Json-Pack 0x14 5 $id 1 $meta $null)
}
function Ids($packs) { @($packs | ForEach-Object { $_.events } | ForEach-Object { $_.event_id }) }

# ================= Scenario 1: subscribe, push, membership hooks, ignore =================
Log '################ Scenario 1: channels and Push ################'
$db1 = "$S\e2e11db-1"; Remove-Item -Recurse -Force $db1 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db1 | Out-Null
$cfg = Write-Config11 $db1
$p = Start-Server $cfg 's1'
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
Check '[1.1b] bob sends -> the subscribed connection gets one Push with that event, id=7 seq=0, fs=ls=g_seq, gap=false' ($pushes.Count -eq 1 -and $pushes[0].subtype -eq 6 -and $pushes[0].id -eq 7 -and $pushes[0].seq -eq 0 -and @($pushes[0].events).Count -eq 1 -and $pushes[0].events[0].event_id -eq $m1 -and [int64]$pushes[0].meta.fs -eq (GSeqOf $pushes[0].events[0]) -and $pushes[0].meta.fs -eq $pushes[0].meta.ls -and $pushes[0].meta.gap -eq $false) "$(Describe $pushes[0]) events=$(@($pushes[0].events).Count) first=$($pushes[0].events[0].event_id) m1=$m1 g=$(GSeqOf $pushes[0].events[0])"
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
$http = Send-Pack (Json-Pack 0x14 4 11 0 @{} $null) $tokA
Check '[1.8] Subscribe over HTTP -> Error Unsupported' ($http.subtype -eq 3 -and $http.meta.code -eq 'Unsupported') (Describe $http)
foreach ($w in @($wsSub, $wsNot, $wsLate, $wsNamed, $wsB)) { try { $w.Dispose() } catch {} }
Stop-Server $p

# ================= Scenario 2: backpressure: a reader that stops reading gets a gap, never blocks the sender =================
Log '################ Scenario 2: gap under backpressure ################'
$db2 = "$S\e2e11db-2"; Remove-Item -Recurse -Force $db2 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db2 | Out-Null
$cfg2 = Write-Config11 $db2 4
$p = Start-Server $cfg2 's2'
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
Ws-Send $wsA (Json-Pack 0x14 1 99 0 @{ limit = 200; batch = 20 } $null)
do { $b = Recv-Or-Null $wsA 5000; if ($null -eq $b) { break }; if ($b.kind -eq 0x14 -and $b.subtype -eq 3) { $recentIds += @(Push-Events ([byte[]]$b.data) | ForEach-Object { $_.event_id }) } } while ($b.meta.r -ne 0)
$missing = @($sent | Where-Object { $recentIds -notcontains $_ }).Count
Check '[2.3] Recent returns every flooded event (the truth is in the DB, the push only hinted)' ($missing -eq 0) "missing=$missing recent=$($recentIds.Count)"
$wsA.Dispose()
Stop-Server $p

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"
