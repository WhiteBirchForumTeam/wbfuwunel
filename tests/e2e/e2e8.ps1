. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
# Scenario 2 needs a binary from before the r_seq migration; point E2E_OLD_EXE at one, or the scenario is skipped.
$OLDEXE = $env:E2E_OLD_EXE
$SEQKEY = 'org.wbftw.wbfuwunel.r_seq'
$GSEQKEY = 'org.wbftw.wbfuwunel.g_seq'
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
function GSeqOf($ev) { if ($ev.unsigned -and ($ev.unsigned.PSObject.Properties.Name -contains $GSEQKEY)) { [int64]$ev.unsigned.$GSEQKEY } else { $null } }
function SeqOf($ev) { if ($ev.unsigned -and ($ev.unsigned.PSObject.Properties.Name -contains $SEQKEY)) { [int64]$ev.unsigned.$SEQKEY } else { $null } }
function Send-Msg($room, $body, $tok) {
  $txn = [guid]::NewGuid().ToString('N')
  $r = Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/send/m.room.message/$txn" (@{ msgtype = 'm.text'; body = $body } | ConvertTo-Json -Compress) $tok
  $r.event_id
}
function Room-Messages($room, $tok, $dir = 'b', $limit = 100) {
  Api Get "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/messages?dir=$dir&limit=$limit" $null $tok
}
function Get-Event($room, $eid, $tok) {
  Api Get "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/event/$([uri]::EscapeDataString($eid))" $null $tok
}
# Ws-Recv with a deadline: a server that never answers fails the run instead of hanging it. A close frame throws too.
function Ws-Recv-Bounded($ws, [int]$ms) {
  $stream = New-Object System.IO.MemoryStream; $buf = New-Object byte[] 262144
  do {
    $t = $ws.ReceiveAsync([ArraySegment[byte]]$buf, [Threading.CancellationToken]::None)
    if (-not $t.Wait($ms)) { throw "no WebSocket frame within $ms ms (state=$($ws.State))" }
    $r = $t.Result
    if ($r.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close) { throw "server closed the connection: $($r.CloseStatus) $($r.CloseStatusDescription)" }
    $stream.Write($buf, 0, $r.Count)
  } while (-not $r.EndOfMessage)
  $p = Read-Pack ($stream.ToArray()); $p.http = 'ws'; $p
}
# Splits a Batch's data (u32 big-endian length + JSON, repeated) into events.
function Batch-Events([byte[]]$data) {
  $events = @(); $at = 0
  while ($at + 4 -le $data.Length) {
    $len = [int](RdBE32 $data $at); $at += 4
    $events += ,([Text.Encoding]::UTF8.GetString($data, $at, $len) | ConvertFrom-Json); $at += $len
  }
  if ($at -ne $data.Length) { throw "Batch data has $($data.Length - $at) trailing bytes" }
  $events
}
# One Recent window over WS: sends the request with `id` = $id and collects Batch packs until r = 0
# (pipeline 6.1). Returns the last pack, plus:
#   .events   every event of the window, newest first
#   .batches  the Batch packs in order
#   .meta     the last Batch's meta (tc, bc, fs, ls, r) plus, derived the way a client would:
#             returned = tc; complete = (tc < limit asked); next = ls of the last batch when not complete, else $null
# An Error pack ends the collection at once and comes back as-is (subtype 3, .events empty).
function Recent-Ws($ws, [uint32]$id, $limit, $after, $before, $batch) {
  $meta = @{}; if ($null -ne $limit) { $meta.limit = $limit }; if ($null -ne $after) { $meta.cg_seq = $after }; if ($null -ne $before) { $meta.before = $before }; if ($null -ne $batch) { $meta.batch = $batch }
  Ws-Send $ws (Json-Pack 0x14 1 $id 0 $meta $null)
  $batches = @(); $events = @()
  do {
    $p = Ws-Recv-Bounded $ws 15000
    if ($p.subtype -ne 3 -or $p.kind -ne 0x14) { $p.events = @(); $p.batches = @(); return $p }
    $batches += ,$p
    $events += @(Batch-Events ([byte[]]$p.data))
  } while ($p.meta.r -ne 0)
  $askedLimit = if ($null -ne $limit) { [int]$limit } else { 320 }
  $last = $batches[-1]
  $last.events = $events; $last.batches = $batches
  $last.meta | Add-Member -NotePropertyName returned -NotePropertyValue ([int]$last.meta.tc) -Force
  $last.meta | Add-Member -NotePropertyName complete -NotePropertyValue ([int]$last.meta.tc -lt $askedLimit) -Force
  $last.meta | Add-Member -NotePropertyName next -NotePropertyValue $(if ([int]$last.meta.tc -lt $askedLimit) { $null } else { [int64]$last.meta.ls }) -Force
  $last
}
function Recent-Http($tok, [uint32]$id, $limit, $after, $before) {
  $meta = @{}; if ($null -ne $limit) { $meta.limit = $limit }; if ($null -ne $after) { $meta.cg_seq = $after }; if ($null -ne $before) { $meta.before = $before }
  $p = Send-Pack (Json-Pack 0x14 1 $id 0 $meta $null) $tok
  $p.events = @()
  $p
}
function Is-DescendingTs($events) {
  for ($i = 1; $i -lt $events.Count; $i++) { if ($events[$i].origin_server_ts -gt $events[$i-1].origin_server_ts) { return $false } }
  $true
}

# ================= Scenario 1: seq on every path, Event/Recent over WS and HTTP =================
$db1 = "$S\e2e8db-1"; Remove-Item -Recurse -Force $db1 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db1 | Out-Null
$cfg = Write-Config $db1 86400
Log '################ Scenario 1: fresh database ################'
$p = Start-Server $cfg 's1'
$regA = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$regB = Api Post '/_matrix/client/v3/register' '{"username":"bob","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokA = $regA.access_token; $tokB = $regB.access_token
Log "users = $($regA.user_id) $($regB.user_id)"

$r1 = (Api Post '/_matrix/client/v3/createRoom' '{"preset":"public_chat","name":"one"}' $tokA).room_id
$r2 = (Api Post '/_matrix/client/v3/createRoom' '{"preset":"private_chat","name":"two"}' $tokA).room_id
Log "rooms = $r1 $r2"
$null = Api Post "/_matrix/client/v3/join/$([uri]::EscapeDataString($r1))" '{}' $tokB

$m1 = Send-Msg $r1 'r1 m1' $tokA
$m2 = Send-Msg $r2 'r2 m2' $tokA
$m3 = Send-Msg $r1 'r1 m3' $tokA
$mB = Send-Msg $r1 'bob says hi' $tokB
$m4 = Send-Msg $r2 'r2 m4' $tokA
$m5 = Send-Msg $r1 'r1 m5' $tokA

# [1.1] /messages: every event carries a seq, forward order is 1..n with no gaps
$msgs1 = Room-Messages $r1 $tokA 'f' 100
$seqs1 = @($msgs1.chunk | ForEach-Object { SeqOf $_ })
$missing = @($seqs1 | Where-Object { $null -eq $_ }).Count
$expected = 1..$seqs1.Count
$contiguous = ($seqs1.Count -gt 0) -and (($seqs1 -join ',') -eq ($expected -join ','))
Check '[1.1] /messages seq 1..n contiguous' ($missing -eq 0 -and $contiguous) "n=$($seqs1.Count) seqs=$($seqs1 -join ',')"
$createSeq = SeqOf ($msgs1.chunk | Where-Object { $_.type -eq 'm.room.create' })
Check '[1.1b] m.room.create is seq 1' ($createSeq -eq 1) "create=$createSeq"

# [1.2] the same event has the same seq on /event, /context, /messages, /sync
$evM3 = Get-Event $r1 $m3 $tokA
$seqM3 = SeqOf $evM3
$fromMsgs = SeqOf ($msgs1.chunk | Where-Object { $_.event_id -eq $m3 })
$ctx = Api Get "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($r1))/context/$([uri]::EscapeDataString($m3))?limit=2" $null $tokA
$fromCtx = SeqOf $ctx.event
$sync = Api Get '/_matrix/client/v3/sync?timeout=0' $null $tokA
$syncEvents = @($sync.rooms.join.$r1.timeline.events)
$fromSync = SeqOf ($syncEvents | Where-Object { $_.event_id -eq $m3 })
Check '[1.2] same seq on /event /messages /context /sync' ($null -ne $seqM3 -and $seqM3 -eq $fromMsgs -and $seqM3 -eq $fromCtx -and $seqM3 -eq $fromSync) "event=$seqM3 messages=$fromMsgs context=$fromCtx sync=$fromSync"
$syncMissing = @($syncEvents | Where-Object { $null -eq (SeqOf $_) }).Count
Check '[1.2b] every /sync timeline event carries seq' ($syncEvents.Count -gt 0 -and $syncMissing -eq 0) "events=$($syncEvents.Count) missing=$syncMissing"

# [1.3] a second room counts independently; a newer message is larger than every older one in its room
$msgs2 = Room-Messages $r2 $tokA 'f' 100
$seqs2 = @($msgs2.chunk | ForEach-Object { SeqOf $_ })
$contiguous2 = (($seqs2 -join ',') -eq ((1..$seqs2.Count) -join ','))
Check '[1.3] room two counts 1..n on its own' $contiguous2 "seqs=$($seqs2 -join ',')"
$seqM1 = SeqOf ($msgs1.chunk | Where-Object { $_.event_id -eq $m1 }); $seqM5 = SeqOf ($msgs1.chunk | Where-Object { $_.event_id -eq $m5 })
Check '[1.3b] later message has larger seq' ($seqM1 -lt $seqM3 -and $seqM3 -lt $seqM5) "m1=$seqM1 m3=$seqM3 m5=$seqM5"

# [1.4] redaction keeps the seq
$null = Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($r1))/redact/$([uri]::EscapeDataString($m3))/$([guid]::NewGuid().ToString('N'))" '{"reason":"test"}' $tokA
Start-Sleep -Milliseconds 500
$evM3r = Get-Event $r1 $m3 $tokA
$pruned = -not ($evM3r.content.PSObject.Properties.Name -contains 'body')
Check '[1.4] redacted event keeps its seq' ((SeqOf $evM3r) -eq $seqM3 -and $pruned -and $evM3r.unsigned.redacted_because) "before=$seqM3 after=$(SeqOf $evM3r) pruned=$pruned"
$msgs1r = Room-Messages $r1 $tokA 'f' 100
$seqsAfter = @($msgs1r.chunk | ForEach-Object { SeqOf $_ })
Check '[1.4b] redaction event itself got the next seq' ($seqsAfter.Count -eq $seqs1.Count + 1 -and $seqsAfter[-1] -eq $seqs1.Count + 1) "seqs=$($seqsAfter -join ',')"

# [1.5] Hello advertises the features
$ws = Ws-Open $tokA
$hello = Ws-Call $ws (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e8'; features = @() } $null)
$feat = @($hello.meta.features)
Check '[1.5] Hello features include recent, batch and seq' (($feat -contains 'recent') -and ($feat -contains 'batch') -and ($feat -contains 'seq')) "features=$($feat -join ',')"

# [1.6] Event/Recent over WS: newest first, paged by cursor to exhaustion, union equals both rooms
# alice is the first user, so she is also in the admin room: expect the union over every joined room
$joined = @((Api Get '/_matrix/client/v3/joined_rooms' $null $tokA).joined_rooms)
$allAlice = @(); foreach ($jr in $joined) { $allAlice += @((Room-Messages $jr $tokA 'f' 500).chunk) }
Log "  alice joined rooms = $($joined.Count) events = $($allAlice.Count)"
# limit 3 with batch 1: three Batch packs (r = 2, 1, 0), tc = 3 on every one, fs/ls are the events' own g_seq
$page1 = Recent-Ws $ws 2 3 $null $null 1
Check '[1.6] first window: 3 Batch packs of 1, tc=3 on each, r counts 2,1,0, seq 0,1,2' ($page1.subtype -eq 3 -and $page1.batches.Count -eq 3 -and $page1.events.Count -eq 3 -and (($page1.batches | ForEach-Object { $_.meta.tc }) -join ',') -eq '3,3,3' -and (($page1.batches | ForEach-Object { $_.meta.r }) -join ',') -eq '2,1,0' -and (($page1.batches | ForEach-Object { $_.seq }) -join ',') -eq '0,1,2' -and (($page1.batches | ForEach-Object { $_.id }) -join ',') -eq '2,2,2') (Describe $page1)
$latest1 = GSeqOf $page1.events[0]
Check '[1.6a] g_seq descends across the window' (((GSeqOf $page1.events[0]) -gt (GSeqOf $page1.events[1])) -and ((GSeqOf $page1.events[1]) -gt (GSeqOf $page1.events[2]))) "g=$(($page1.events | ForEach-Object { GSeqOf $_ }) -join ',')"
Check '[1.6g] each batch fs/ls equal its one event g_seq; the last ls is the next cursor' ((0..2 | ForEach-Object { ([int64]$page1.batches[$_].meta.fs -eq (GSeqOf $page1.events[$_])) -and ([int64]$page1.batches[$_].meta.ls -eq (GSeqOf $page1.events[$_])) }) -notcontains $false -and [int64]$page1.meta.next -eq (GSeqOf $page1.events[2])) "next=$($page1.meta.next)"
Check '[1.6b] first page newest first' ((Is-DescendingTs $page1.events) -and $page1.events[0].event_id -ne $m1) "ids=$(($page1.events | ForEach-Object { $_.event_id.Substring(0,8) }) -join ' ')"
Check '[1.6c] events carry room_id, r_seq and g_seq' (@($page1.events | Where-Object { $_.room_id -and (SeqOf $_) -ne $null -and (GSeqOf $_) -ne $null }).Count -eq 3) "rooms=$(($page1.events | ForEach-Object { $_.room_id }) -join ' ')"
$collected = @($page1.events); $cursor = $page1.meta.next; $pages = 1
while ($cursor) {
  $pg = Recent-Ws $ws (10 + $pages) 4 $null $cursor; $pages++
  $collected += @($pg.events); $cursor = $pg.meta.next
  if ($null -eq $cursor) { $lastComplete = $pg.meta.complete }
  if ($pages -gt 50) { break }
}
$collectedIds = @($collected | ForEach-Object { $_.event_id } | Sort-Object)
$expectedIds = @($allAlice | ForEach-Object { $_.event_id } | Sort-Object)
$dupes = $collectedIds.Count - @($collectedIds | Select-Object -Unique).Count
Check '[1.6d] paging to exhaustion returns exactly the union of both rooms' (($collectedIds -join ',') -eq ($expectedIds -join ',') -and $dupes -eq 0) "got=$($collectedIds.Count) expected=$($expectedIds.Count) pages=$pages dupes=$dupes"
Check '[1.6e] whole sequence is newest first' (Is-DescendingTs $collected) ''
Check '[1.6f] last page has next=null and complete=true' ($null -eq $cursor -and $lastComplete -eq $true) ''

# [1.6h] cg_seq = cached watermark: only the difference comes back, complete=true, no next
$mark = GSeqOf $collected[2]
$diff = Recent-Ws $ws 30 100 $mark $null
$expectDiff = @($collected | Where-Object { (GSeqOf $_) -gt $mark }).Count
Check '[1.6h] cg_seq=<g_seq of 3rd newest> returns exactly the 2 newer in one batch, complete' ($diff.meta.returned -eq 2 -and $diff.events.Count -eq $expectDiff -and $diff.batches.Count -eq 1 -and $diff.meta.complete -eq $true -and $null -eq $diff.meta.next) (Describe $diff)
$none = Recent-Ws $ws 31 100 $latest1 $null
Check '[1.6i] cg_seq=newest returns one empty Batch: tc=0 bc=0 r=0 fs=ls=0' ($none.subtype -eq 3 -and $none.batches.Count -eq 1 -and $none.meta.tc -eq 0 -and $none.meta.bc -eq 0 -and $none.meta.fs -eq 0 -and $none.meta.ls -eq 0 -and $none.events.Count -eq 0) (Describe $none)
$zero = Recent-Ws $ws 34 3 0 $null
Check '[1.6k] cg_seq=0 behaves like no cache' ($zero.meta.returned -eq 3 -and $zero.events[0].event_id -eq $page1.events[0].event_id) (Describe $zero)
# [1.6j] a hole: cg_seq=mark but limit=1 -> newest only, complete=false, next; then before=next fills the hole
$hole = Recent-Ws $ws 32 1 $mark $null
$fill = Recent-Ws $ws 33 100 $mark $hole.meta.next
Check '[1.6j] limit smaller than the gap: complete=false; cg_seq+before fills the rest to complete' ($hole.meta.returned -eq 1 -and $hole.meta.complete -eq $false -and $fill.meta.returned -eq 1 -and $fill.meta.complete -eq $true -and (GSeqOf $fill.events[0]) -lt (GSeqOf $hole.events[0]) -and (GSeqOf $fill.events[0]) -gt $mark) "hole=$(Describe $hole) fill=$(Describe $fill)"

# [1.7] bob sees only room one
$wsB = Ws-Open $tokB
$pageB = Recent-Ws $wsB 3 100 $null $null
$roomsB = @($pageB.events | ForEach-Object { $_.room_id } | Select-Object -Unique)
Check '[1.7] bob gets room one only, next=null' ($roomsB.Count -eq 1 -and $roomsB[0] -eq $r1 -and $null -eq $pageB.meta.next -and $pageB.meta.complete -eq $true -and $pageB.events.Count -gt 0) "rooms=$($roomsB -join ' ') returned=$($pageB.meta.returned)"

# [1.8] ignoring bob removes his message for alice, in Recent as in /messages
$null = Api Put "/_matrix/client/v3/user/$([uri]::EscapeDataString($regA.user_id))/account_data/m.ignored_user_list" (@{ ignored_users = @{ $regB.user_id = @{} } } | ConvertTo-Json -Compress -Depth 4) $tokA
Start-Sleep -Milliseconds 300
$pageIgn = Recent-Ws $ws 4 100 $null $null
$hasBob = @($pageIgn.events | Where-Object { $_.event_id -eq $mB }).Count
$msgsIgn = Room-Messages $r1 $tokA 'f' 100
$hasBobMsgs = @($msgsIgn.chunk | Where-Object { $_.event_id -eq $mB }).Count
Check '[1.8] ignored sender absent from Recent and /messages alike' ($hasBob -eq 0 -and $hasBobMsgs -eq 0) "recent=$hasBob messages=$hasBobMsgs"

# [1.9] HTTP is not a transport for Recent (its reply streams); bad cursor; defaults; batch clamping
$http1 = Recent-Http $tokA 5 2 $null $null
Check '[1.9] Recent over HTTP -> Error Unsupported, id copied' ($http1.subtype -eq 3 -and $http1.kind -eq 1 -and $http1.meta.code -eq 'Unsupported' -and $http1.id -eq 5) (Describe $http1)
$bad = Recent-Ws $ws 6 2 'not-a-number' $null
Check '[1.9b] non-integer after -> Error Conflict' ($bad.subtype -eq 3 -and $bad.kind -eq 1 -and $bad.meta.code -eq 'Conflict') (Describe $bad)
$noMeta = Recent-Ws $ws 7 $null $null $null
Check '[1.9c] empty meta = defaults (window 320, batch 10): one window, batches of at most 10' ($noMeta.subtype -eq 3 -and $noMeta.meta.tc -gt 0 -and $noMeta.meta.tc -lt 320 -and (@($noMeta.batches | Where-Object { $_.meta.bc -gt 10 }).Count -eq 0) -and $noMeta.batches.Count -eq [math]::Ceiling($noMeta.meta.tc / 10)) (Describe $noMeta)
$big = Recent-Ws $ws 9 500 $null $null 1000
Check '[1.9e] batch=1000 is clamped to 100: everything in one batch when tc <= 100' ($big.subtype -eq 3 -and $big.meta.tc -eq $noMeta.meta.tc -and $big.batches.Count -eq [math]::Ceiling($big.meta.tc / 100)) (Describe $big)
$clamp = Recent-Ws $ws 10 10000 $null $null 100
Check '[1.9f] limit=10000 is accepted and clamped (Hello says recent_max_limit=500); the window still ends' ($clamp.subtype -eq 3 -and $clamp.meta.tc -eq $noMeta.meta.tc -and $clamp.meta.r -eq 0 -and $hello.meta.recent_max_limit -eq 500 -and $hello.meta.recent_default_limit -eq 320) (Describe $clamp)
# two windows leave the connection free in between: a Ping between them is answered at once
$pong = Ws-Call $ws (New-Pack 1 4 0 0 99 ([Text.Encoding]::UTF8.GetBytes('{"nonce":7}')) @())
Check '[1.9g] Ping between two windows -> Pong' ($pong.subtype -eq 5) (Describe $pong)
try {
  $unknown = Ws-Call $ws (New-Pack 0x14 0x7f 0 0 8 @() @())
  Check '[1.9d] unknown Event subtype -> UnknownKind' ($unknown.subtype -eq 3 -and $unknown.meta.code -eq 'UnknownKind') (Describe $unknown)
} catch { Check '[1.9d] unknown Event subtype -> UnknownKind' $false ("exception: " + $_.Exception.Message + " " + $_.InvocationInfo.PositionMessage) }

$ws.Dispose(); $wsB.Dispose()
Stop-Server $p

# ================= Scenario 2: a database from before seq gets numbered once =================
if (-not $OLDEXE -or -not (Test-Path $OLDEXE)) { Log '################ Scenario 2 skipped: set E2E_OLD_EXE to a pre-migration binary ################' }
else {
Log '################ Scenario 2: migration of an old database ################'
$db2 = "$S\e2e8db-2"; Remove-Item -Recurse -Force $db2 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db2 | Out-Null
$cfg2 = Write-Config $db2 86400
$saveExe = $EXE; $EXE = $OLDEXE
$p = Start-Server $cfg2 's2-old'
$regO = Api Post '/_matrix/client/v3/register' '{"username":"old","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokO = $regO.access_token
$rO = (Api Post '/_matrix/client/v3/createRoom' '{"preset":"private_chat","name":"old room"}' $tokO).room_id
$o1 = Send-Msg $rO 'old 1' $tokO; $o2 = Send-Msg $rO 'old 2' $tokO; $o3 = Send-Msg $rO 'old 3' $tokO
$before = Room-Messages $rO $tokO 'f' 100
$noneHad = @($before.chunk | Where-Object { $null -ne (SeqOf $_) }).Count
Check '[2.0] old binary stores no seq' ($noneHad -eq 0 -and $before.chunk.Count -gt 0) "events=$($before.chunk.Count) with_seq=$noneHad"
Stop-Server $p
$EXE = $saveExe

$p = Start-Server $cfg2 's2-new'
$after = Room-Messages $rO $tokO 'f' 100
$seqsO = @($after.chunk | ForEach-Object { SeqOf $_ })
$gMissing = @($after.chunk | Where-Object { $null -eq (GSeqOf $_) }).Count
Check '[2.1] migration numbered every stored event 1..n with g_seq' (($seqsO -join ',') -eq ((1..$seqsO.Count) -join ',') -and $gMissing -eq 0) "seqs=$($seqsO -join ',')"
$o4 = Send-Msg $rO 'new 4' $tokO
$ev4 = Get-Event $rO $o4 $tokO
Check '[2.2] the first new event continues at n+1' ((SeqOf $ev4) -eq $seqsO.Count + 1) "seq=$(SeqOf $ev4) n=$($seqsO.Count)"
$log = (Get-Content "$OUT\s2-new.out" -Raw) -replace "`e\[[0-9;]*m", ''
Check '[2.3] migration logged once' ($log -match 'Numbered stored events') ''
Stop-Server $p

$p = Start-Server $cfg2 's2-again'
$log2 = (Get-Content "$OUT\s2-again.out" -Raw) -replace "`e\[[0-9;]*m", ''
Check '[2.4] second start does not renumber' (-not ($log2 -match 'Numbered stored events')) ''
$again = Room-Messages $rO $tokO 'f' 100
$seqsAgain = @($again.chunk | ForEach-Object { SeqOf $_ })
Check '[2.5] numbers unchanged after restart' (($seqsAgain -join ',') -eq ((1..$seqsAgain.Count) -join ',') -and $seqsAgain.Count -eq $seqsO.Count + 1) "seqs=$($seqsAgain -join ',')"
Stop-Server $p
}


# ================= Scenario 3: the pack's byte budget cuts a page; the cursor resumes without loss or repeat =================
Log '################ Scenario 3: wbf_data_max_bytes small ################'
$db3 = "$S\e2e8db-3"; Remove-Item -Recurse -Force $db3 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db3 | Out-Null
$cfg3 = Write-Config $db3 86400 0 1500
$p = Start-Server $cfg3 's3'
$regC = Api Post '/_matrix/client/v3/register' '{"username":"carol","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokC = $regC.access_token
$rC = (Api Post '/_matrix/client/v3/createRoom' '{"preset":"private_chat","name":"budget"}' $tokC).room_id
1..6 | ForEach-Object { $null = Send-Msg $rC "budget message number $_ with some padding text to make it a few hundred bytes long" $tokC }
$joinedC = @((Api Get '/_matrix/client/v3/joined_rooms' $null $tokC).joined_rooms)
$allC = @(); foreach ($jr in $joinedC) { $allC += @((Room-Messages $jr $tokC 'f' 500).chunk) }
$wsC = Ws-Open $tokC
# The byte budget cuts batches, not the window: one window holds every event (each fits alone), in more
# batches than `batch` alone would make, none over 1500 B of data.
$pg = Recent-Ws $wsC 1 100 $null $null 100
Check '[3.1] budget of 1500 B: one window with every event, cut into several batches, each data <= 1500 B' ($pg.subtype -eq 3 -and $pg.meta.complete -eq $true -and $pg.meta.tc -eq $allC.Count -and $pg.batches.Count -gt 1 -and (@($pg.batches | Where-Object { $_.data.Length -gt 1500 }).Count -eq 0)) "batches=$($pg.batches.Count) tc=$($pg.meta.tc) want=$($allC.Count) sizes=$(($pg.batches | ForEach-Object { $_.data.Length }) -join ',')"
$got = @($pg.events)
$gotIds = @($got | ForEach-Object { $_.event_id } | Sort-Object); $wantIds = @($allC | ForEach-Object { $_.event_id } | Sort-Object)
$dup = $gotIds.Count - @($gotIds | Select-Object -Unique).Count
Check '[3.2] the batches together hold every event exactly once' (($gotIds -join ',') -eq ($wantIds -join ',') -and $dup -eq 0) "got=$($gotIds.Count) want=$($wantIds.Count) dupes=$dup"
Check '[3.3] batches are contiguous: newest first across batch boundaries, fs/ls match the data' ((Is-DescendingTs $got) -and ((0..($pg.batches.Count - 1) | ForEach-Object { $bt = $pg.batches[$_]; $evs = @(Batch-Events ([byte[]]$bt.data)); ([int64]$bt.meta.fs -eq (GSeqOf $evs[0])) -and ([int64]$bt.meta.ls -eq (GSeqOf $evs[-1])) }) -notcontains $false)) ''
$wsC.Dispose()
Stop-Server $p

# a budget no single event fits in: every event is skipped, the reply is empty but complete
$cfg3b = Write-Config $db3 86400 0 200
$p = Start-Server $cfg3b 's3b'
$wsC = Ws-Open $tokC
$empty = Recent-Ws $wsC 1 100 $null $null
Check '[3.4] budget smaller than any event: every event skipped, one empty Batch (tc=0)' ($empty.subtype -eq 3 -and $empty.batches.Count -eq 1 -and $empty.meta.tc -eq 0 -and $empty.meta.complete -eq $true -and $null -eq $empty.meta.next) (Describe $empty)
$wsC.Dispose()
Stop-Server $p

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"
