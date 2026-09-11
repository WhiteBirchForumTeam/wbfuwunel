. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
$OUT = "$S\e2e12-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
function Write-Config12([string]$db) {
  $cfg = "$S\e2e12.toml"
  @('[global]','server_name = "localhost"',('database_path = "' + ($db.Replace([string][char]92, '/')) + '"'),'port = 8015','address = ["127.0.0.1"]',
    'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
    'wbf_ws_idle_timeout = 120','log = "info"') -join "`n" | Set-Content -Path $cfg -Encoding ascii
  $cfg
}
function Register($name) { Api Post '/_matrix/client/v3/register' ('{"username":"' + $name + '","password":"pw-pw-pw-pw","auth":{"type":"m.login.dummy"}}') $null }
# Sends one to-device message from $tok to (user, device).
function Send-ToDevice($tok, $user, $device, $body) {
  $txn = [guid]::NewGuid().ToString('N')
  $messages = @{ messages = @{ $user = @{ $device = @{ msgtype = 'm.test'; body = $body } } } }
  Api Put "/_matrix/client/v3/sendToDevice/m.room.message/$txn" ($messages | ConvertTo-Json -Compress -Depth 6) $tok
}
# Ws-Recv with a deadline; an unfinished ReceiveAsync is kept per socket and waited on again
# (tests/e2e/README.md: an abandoned one eats the next frame).
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
function Call($ws, [byte[]]$pack) {
  Ws-Send $ws $pack
  for ($i = 0; $i -lt 50; $i++) {
    $p = Recv-Or-Null $ws 10000
    if ($null -eq $p) { throw 'no reply within 10 s' }
    if ($p.closed) { throw "server closed the connection: $($p.code)" }
    # A Device/Push may arrive first; the caller wants the reply to its request.
    if (-not ($p.kind -eq 0x16 -and $p.subtype -eq 6)) { return $p }
  }
  throw 'only pushes, no reply'
}
function Drain($ws, [int]$quietMs = 1500) {
  $packs = @()
  while ($true) {
    $p = Recv-Or-Null $ws $quietMs
    if ($null -eq $p -or $p.closed) { break }
    $packs += ,$p
  }
  @($packs)
}
# The counts a Device/Batch or Device/Push meta carries.
function Counts($pack) { @($pack.meta.counts | ForEach-Object { [uint64]$_ }) }
# The items in a pack's data (u32 length prefix each).
function Items([byte[]]$data) {
  $items = @(); $at = 0
  while ($at + 4 -le $data.Length) { $len = [int](RdBE32 $data $at); $at += 4; $items += ,([Text.Encoding]::UTF8.GetString($data, $at, $len) | ConvertFrom-Json); $at += $len }
  @($items)
}
# data for ItemsDestroy: each count as 8 big-endian bytes, no separator.
function Count-Bytes($counts) {
  $bytes = New-Object byte[] (8 * @($counts).Count)
  $at = 0
  foreach ($c in @($counts)) { (BE64 ([uint64]$c)).CopyTo($bytes, $at); $at += 8 }
  ,$bytes
}
function Destroy($ws, [uint64]$id, $counts) {
  $data = Count-Bytes $counts
  Ws-Send $ws (New-Pack 0x16 3 0 $id 0 ([Text.Encoding]::UTF8.GetBytes((@{ tc = @($counts).Count } | ConvertTo-Json -Compress))) $data)
  $ack = $null; $result = $null
  for ($i = 0; $i -lt 20 -and ($null -eq $ack -or $null -eq $result); $i++) {
    $p = Recv-Or-Null $ws 10000
    if ($null -eq $p) { break }
    if ($p.closed) { throw "server closed the connection: $($p.code)" }
    if ($p.kind -eq 1 -and $p.subtype -eq 2) { $ack = $p }
    elseif ($p.kind -eq 0x16 -and $p.subtype -eq 7) { $result = $p }
  }
  @{ ack = $ack; result = $result }
}

Log '################ Scenario 1: the to-device queue over the channel ################'
$db = "$S\e2e12db"; Remove-Item -Recurse -Force $db -EA SilentlyContinue; New-Item -ItemType Directory -Force $db | Out-Null
$cfg = Write-Config12 $db
$p = Start-Server $cfg 's1'
$regA = Register 'alice'; $tokA = $regA.access_token; $devA = $regA.device_id
$regB = Register 'bob'; $tokB = $regB.access_token

$ws = Ws-Open $tokA
$hello = Call $ws (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e12'; features = @() } $null)
Check '[1.0] Hello answers on a fresh connection' ($hello.subtype -eq 2) "connection_id=$($hello.meta.connection_id)"

# [1.1] the device_id must be the session's
$wrong = Call $ws (Json-Pack 0x16 4 10 0 @{ device_id = 'NOTMYDEVICE' } $null)
Check '[1.1] Subscribe naming another device -> Forbidden' ($wrong.subtype -eq 3 -and $wrong.meta.code_id -eq 1302) (Describe $wrong)

# [1.2] the right one is accepted
$ok = Call $ws (Json-Pack 0x16 4 10 0 @{ device_id = $devA } $null)
Check '[1.2] Subscribe with the session device -> Ack latest_cd_seq' ($ok.subtype -eq 2 -and $ok.meta.latest_cd_seq -gt 0) (Describe $ok)

# [1.3] a second connection of the same device is refused, and the first keeps it
$ws2 = Ws-Open $tokA
$taken = Call $ws2 (Json-Pack 0x16 4 11 0 @{ device_id = $devA } $null)
Check '[1.3] a second connection -> Conflict, the holder does not change' ($taken.subtype -eq 3 -and $taken.meta.code_id -eq 1502) (Describe $taken)

# [1.4] bob sends to alice's device -> pushed to the holder, with its count
$null = Send-ToDevice $tokB $regA.user_id $devA 'first'
$null = Send-ToDevice $tokB $regA.user_id $devA 'second'
$pushes = @(Drain $ws 2000 | Where-Object { $_.kind -eq 0x16 -and $_.subtype -eq 6 })
$pushedCounts = @($pushes | ForEach-Object { Counts $_ })
$pushedBodies = @($pushes | ForEach-Object { Items ([byte[]]$_.data) } | ForEach-Object { $_.content.body })
Check '[1.4] both messages are pushed, each with its count' ($pushedCounts.Count -eq 2 -and $pushedBodies -contains 'first' -and $pushedBodies -contains 'second') "counts=$($pushedCounts -join ',') bodies=$($pushedBodies -join ',')"
Check '[1.4b] the pushes carry the subscription id, not the request id' (@($pushes | Where-Object { $_.id -eq 10 }).Count -eq $pushes.Count) "ids=$(@($pushes | ForEach-Object { $_.id }) -join ',')"

# [1.5] nothing was destroyed yet, so a Fetch from zero returns them
$fetch = @()
Ws-Send $ws (Json-Pack 0x16 1 12 0 @{ cd_seq = 0 } $null)
do { $batch = Recv-Or-Null $ws 5000; if ($null -eq $batch) { break }; if ($batch.kind -eq 0x16 -and $batch.subtype -eq 2) { $fetch += ,$batch } } while ($batch.meta.r -ne 0)
$fetched = @($fetch | ForEach-Object { Counts $_ })
Check '[1.5] Fetch returns the queue oldest first, r=0 on the last pack' ($fetched.Count -eq 2 -and $fetched[0] -lt $fetched[1] -and $fetch[-1].meta.r -eq 0) "counts=$($fetched -join ',')"

# [1.6] destroy: an Ack that the command arrived, then the result
$destroy = Destroy $ws 13 $fetched
$destroyedCounts = @()
if ($destroy.result) { $at = 0; $data = [byte[]]$destroy.result.data; while ($at + 8 -le $data.Length) { $destroyedCounts += ,(RdBE64 $data $at); $at += 8 } }
Check '[1.6] ItemsDestroy -> Ack (received) then ItemsDestroyed (result)' ($null -ne $destroy.ack -and $null -ne $destroy.result) "ack=$($null -ne $destroy.ack) result=$($null -ne $destroy.result)"
Check '[1.6b] every count asked for is reported gone' (@($destroyedCounts).Count -eq 2 -and $destroy.result.meta.tc -eq 2 -and $destroy.result.meta.bc -eq 2) "tc=$($destroy.result.meta.tc) bc=$($destroy.result.meta.bc) counts=$($destroyedCounts -join ',')"

# [1.7] and they really are gone
$after = @()
Ws-Send $ws (Json-Pack 0x16 1 14 0 @{ cd_seq = 0 } $null)
do { $batch = Recv-Or-Null $ws 5000; if ($null -eq $batch) { break }; if ($batch.kind -eq 0x16 -and $batch.subtype -eq 2) { $after += ,$batch } } while ($batch.meta.r -ne 0)
Check '[1.7] the queue is empty afterwards (one empty Batch, r=0)' ($after.Count -eq 1 -and $after[0].meta.bc -eq 0 -and $after[0].meta.tc -eq 0 -and $after[0].meta.r -eq 0) (Describe $after[0])

# [1.8] destroying again is not an error: the client asked for a state, not an event
$null = Send-ToDevice $tokB $regA.user_id $devA 'third'
$third = @(Drain $ws 2000 | Where-Object { $_.kind -eq 0x16 -and $_.subtype -eq 6 } | ForEach-Object { Counts $_ })
$again = Destroy $ws 15 $third
$twice = Destroy $ws 16 $third
Check '[1.8] destroying an item that is already gone still reports it destroyed' ($again.result.meta.bc -eq 1 -and $twice.result.meta.bc -eq 1) "first=$($again.result.meta.bc) second=$($twice.result.meta.bc)"

# [1.9] tc and the data must agree, or nothing is destroyed
$null = Send-ToDevice $tokB $regA.user_id $devA 'fourth'
$fourth = @(Drain $ws 2000 | Where-Object { $_.kind -eq 0x16 -and $_.subtype -eq 6 } | ForEach-Object { Counts $_ })
$lying = Call $ws (New-Pack 0x16 3 0 17 0 ([Text.Encoding]::UTF8.GetBytes('{"tc":5}')) (Count-Bytes $fourth))
Check '[1.9] tc that disagrees with the data -> InvalidRequest' ($lying.subtype -eq 3 -and $lying.meta.code_id -eq 1201) (Describe $lying)
$stillThere = @()
Ws-Send $ws (Json-Pack 0x16 1 18 0 @{ cd_seq = 0 } $null)
do { $batch = Recv-Or-Null $ws 5000; if ($null -eq $batch) { break }; if ($batch.kind -eq 0x16 -and $batch.subtype -eq 2) { $stillThere += ,$batch } } while ($batch.meta.r -ne 0)
Check '[1.9b] and nothing was destroyed' (@($stillThere | ForEach-Object { Counts $_ }).Count -eq 1) "left=$(@($stillThere | ForEach-Object { Counts $_ }) -join ',')"

# [1.10] only the holder may destroy
$notHolder = Call $ws2 (New-Pack 0x16 3 0 19 0 ([Text.Encoding]::UTF8.GetBytes('{"tc":1}')) (Count-Bytes $fourth))
Check '[1.10] ItemsDestroy from a connection that does not hold the queue -> Forbidden' ($notHolder.subtype -eq 3 -and $notHolder.meta.code_id -eq 1302) (Describe $notHolder)

# [1.11] Unsubscribe hands the queue back
$bye = Call $ws (Json-Pack 0x16 5 20 0 @{} $null)
$nowFree = Call $ws2 (Json-Pack 0x16 4 21 0 @{ device_id = $devA } $null)
Check '[1.11] Unsubscribe -> Ack, and the next connection can take the queue' ($bye.subtype -eq 2 -and $nowFree.subtype -eq 2) "unsubscribe=$($bye.subtype) subscribe=$($nowFree.subtype)"

# [1.12] HTTP is not a place to hold a queue
$http = Send-Pack (Json-Pack 0x16 4 22 0 @{ device_id = $devA } $null) $tokA
Check '[1.12] Device/Subscribe over HTTP -> Unsupported' ($http.subtype -eq 3 -and $http.meta.code_id -eq 1102) (Describe $http)

$ws.Dispose(); $ws2.Dispose()
Stop-Server $p

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"
