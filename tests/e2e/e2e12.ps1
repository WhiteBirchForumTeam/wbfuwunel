. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
$OUT = "$S\e2e12-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
function Write-Config12([string]$db, [string[]]$extra = @()) {
  $cfg = "$S\e2e12.toml"
  (@('[global]','server_name = "localhost"',('database_path = "' + ($db.Replace([string][char]92, '/')) + '"'),'port = 8015','address = ["127.0.0.1"]',
    'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
    'wbf_ws_idle_timeout = 120','log = "info"') + $extra) -join "`n" | Set-Content -Path $cfg -Encoding ascii
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
    # A Device/Push or Device/CryptoState may arrive first (a Subscribe is always followed by a CryptoState,
    # wbf-e2ee.md 3.4); the caller wants the reply to its request.
    if (-not ($p.kind -eq 0x16 -and ($p.subtype -eq 6 -or $p.subtype -eq 8))) { return $p }
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
  Ws-Send $ws (New-Pack 0x16 3 0 (Conv $id) 0 ([Text.Encoding]::UTF8.GetBytes((@{ tc = @($counts).Count } | ConvertTo-Json -Compress))) $data)
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
$server = Start-Server $cfg 's1'
$regA = Register 'alice'; $tokA = $regA.access_token; $devA = $regA.device_id
$regB = Register 'bob'; $tokB = $regB.access_token

$ws = Ws-Open $tokA
$hello = Call $ws (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e12'; features = @() } $null)
Check '[1.0] Hello answers on a fresh connection' ($hello.subtype -eq 2) "connection_id=$($hello.meta.connection_id)"

# [1.1] the device_id must be the session's
$wrong = Call $ws (Json-Pack 0x16 4 (Conv 10) 0 @{ device_id = 'NOTMYDEVICE' } $null)
Check '[1.1] Subscribe naming another device -> Forbidden' ($wrong.subtype -eq 3 -and $wrong.meta.code_id -eq 1302) (Describe $wrong)

# [1.2] the right one is accepted
$ok = Call $ws (Json-Pack 0x16 4 (Conv 10) 0 @{ device_id = $devA } $null)
Check '[1.2] Subscribe with the session device -> Ack latest_cd_seq' ($ok.subtype -eq 2 -and $ok.meta.latest_cd_seq -gt 0) (Describe $ok)

# [1.3] a later connection of the same device takes the queue over, and the one it displaced is told.
# Refusing the later one instead would mean a device whose last connection died silently cannot
# subscribe until the idle timeout — and if that connection is wedged, not ever (wbf-to-device.md 4).
$ws2 = Ws-Open $tokA
$took = Call $ws2 (Json-Pack 0x16 4 (Conv 11) 0 @{ device_id = $devA } $null)
Check '[1.3a] a later connection of the same device -> Ack, it takes the queue over' ($took.subtype -eq 2 -and $took.meta.latest_cd_seq -gt 0) (Describe $took)
$notice = Recv-Or-Null $ws 5000
while ($null -ne $notice -and $notice.kind -eq 0x16 -and $notice.subtype -eq 8) { $notice = Recv-Or-Null $ws 5000 }
Check '[1.3b] the displaced connection is told: Superseded, carrying its own subscription id, IS_LAST' `
  ($null -ne $notice -and $notice.subtype -eq 3 -and $notice.meta.code_id -eq 1505 -and $notice.meta.code -eq 'Superseded' -and $notice.id -eq (Conv 10) -and ($notice.flags -band 8) -eq 8) `
  "id=$($notice.id) flags=$($notice.flags) code=$($notice.meta.code_id)"
# Taking it back, so the rest of this script speaks through $ws — and the same notice goes the other way.
$back = Call $ws (Json-Pack 0x16 4 (Conv 10) 0 @{ device_id = $devA } $null)
$notice2 = Recv-Or-Null $ws2 5000
while ($null -ne $notice2 -and $notice2.kind -eq 0x16 -and $notice2.subtype -eq 8) { $notice2 = Recv-Or-Null $ws2 5000 }
Check '[1.3c] it works in both directions: the second connection is displaced by its own id' `
  ($back.subtype -eq 2 -and $null -ne $notice2 -and $notice2.meta.code_id -eq 1505 -and $notice2.id -eq (Conv 11)) `
  "id=$($notice2.id) code=$($notice2.meta.code_id)"

# [1.4] bob sends to alice's device -> pushed to the holder, with its count
$null = Send-ToDevice $tokB $regA.user_id $devA 'first'
$null = Send-ToDevice $tokB $regA.user_id $devA 'second'
$pushes = @(Drain $ws 2000 | Where-Object { $_.kind -eq 0x16 -and $_.subtype -eq 6 })
$pushedCounts = @($pushes | ForEach-Object { Counts $_ })
$pushedBodies = @($pushes | ForEach-Object { Items ([byte[]]$_.data) } | ForEach-Object { $_.content.body })
Check '[1.4] both messages are pushed, each with its count' ($pushedCounts.Count -eq 2 -and $pushedBodies -contains 'first' -and $pushedBodies -contains 'second') "counts=$($pushedCounts -join ',') bodies=$($pushedBodies -join ',')"
Check '[1.4b] the pushes carry the subscription id, not the request id' (@($pushes | Where-Object { $_.id -eq (Conv 10) }).Count -eq $pushes.Count) "ids=$(@($pushes | ForEach-Object { $_.id }) -join ',')"

# [1.5] nothing was destroyed yet, so a Fetch from zero returns them
$fetch = @()
Ws-Send $ws (Json-Pack 0x16 1 (Conv 12) 0 @{ cd_seq = 0 } $null)
do { $batch = Recv-Or-Null $ws 5000; if ($null -eq $batch) { break }; if ($batch.kind -eq 0x16 -and $batch.subtype -eq 2) { $fetch += ,$batch } } while ($batch.meta.r -ne 0)
$fetched = @($fetch | ForEach-Object { Counts $_ })
Check '[1.5] Fetch returns the queue oldest first, r=0 on the last pack' ($fetched.Count -eq 2 -and $fetched[0] -lt $fetched[1] -and $fetch[-1].meta.r -eq 0) "counts=$($fetched -join ',')"
$zero = @()
Ws-Send $ws (Json-Pack 0x16 1 (Conv 23) 0 @{ cd_seq = 0; limit = 0 } $null)
do { $batch = Recv-Or-Null $ws 5000; if ($null -eq $batch) { break }; if ($batch.kind -eq 0x16 -and $batch.subtype -eq 2) { $zero += ,$batch } } while ($batch.meta.r -ne 0)
Check '[1.5c] limit=0 asks for none: one empty Batch with more=false (PR #53 review, rumia)' ($zero.Count -eq 1 -and $zero[0].meta.bc -eq 0 -and $zero[0].meta.more -eq $false) "batches=$($zero.Count) more=$($zero[0].meta.more)"
Check '[1.5b] two items under a limit of 1000 and the window budget: more=false, nothing behind them' ($fetch.Count -gt 0 -and @($fetch | Where-Object { $_.meta.more -ne $false }).Count -eq 0) "more=$(@($fetch | ForEach-Object { $_.meta.more }) -join ',')"

# [1.6] destroy: an Ack that the command arrived, then the result
$destroy = Destroy $ws 13 $fetched
$destroyedCounts = @()
if ($destroy.result) { $at = 0; $data = [byte[]]$destroy.result.data; while ($at + 8 -le $data.Length) { $destroyedCounts += ,(RdBE64 $data $at); $at += 8 } }
Check '[1.6] ItemsDestroy -> Ack (received) then ItemsDestroyed (result)' ($null -ne $destroy.ack -and $null -ne $destroy.result) "ack=$($null -ne $destroy.ack) result=$($null -ne $destroy.result)"
Check '[1.6b] every count asked for is reported gone' (@($destroyedCounts).Count -eq 2 -and $destroy.result.meta.tc -eq 2 -and $destroy.result.meta.bc -eq 2) "tc=$($destroy.result.meta.tc) bc=$($destroy.result.meta.bc) counts=$($destroyedCounts -join ',')"

# [1.7] and they really are gone
$after = @()
Ws-Send $ws (Json-Pack 0x16 1 (Conv 14) 0 @{ cd_seq = 0 } $null)
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
$lying = Call $ws (New-Pack 0x16 3 0 (Conv 17) 0 ([Text.Encoding]::UTF8.GetBytes('{"tc":5}')) (Count-Bytes $fourth))
Check '[1.9] tc that disagrees with the data -> InvalidRequest' ($lying.subtype -eq 3 -and $lying.meta.code_id -eq 1201) (Describe $lying)
$stillThere = @()
Ws-Send $ws (Json-Pack 0x16 1 (Conv 18) 0 @{ cd_seq = 0 } $null)
do { $batch = Recv-Or-Null $ws 5000; if ($null -eq $batch) { break }; if ($batch.kind -eq 0x16 -and $batch.subtype -eq 2) { $stillThere += ,$batch } } while ($batch.meta.r -ne 0)
Check '[1.9b] and nothing was destroyed' (@($stillThere | ForEach-Object { Counts $_ }).Count -eq 1) "left=$(@($stillThere | ForEach-Object { Counts $_ }) -join ',')"

# [1.10] only the holder may destroy
$notHolder = Call $ws2 (New-Pack 0x16 3 0 (Conv 19) 0 ([Text.Encoding]::UTF8.GetBytes('{"tc":1}')) (Count-Bytes $fourth))
Check '[1.10] ItemsDestroy from a connection that does not hold the queue -> Forbidden' ($notHolder.subtype -eq 3 -and $notHolder.meta.code_id -eq 1302) (Describe $notHolder)

# [1.11] Unsubscribe hands the queue back
$bye = Call $ws (Json-Pack 0x16 5 (Conv 20) 0 @{} $null)
$nowFree = Call $ws2 (Json-Pack 0x16 4 (Conv 21) 0 @{ device_id = $devA } $null)
Check '[1.11] Unsubscribe -> Ack, and the next connection can take the queue' ($bye.subtype -eq 2 -and $nowFree.subtype -eq 2) "unsubscribe=$($bye.subtype) subscribe=$($nowFree.subtype)"

# [1.12] HTTP is not a place to hold a queue
$http = Send-Pack (Json-Pack 0x16 4 (Conv 22) 0 @{ device_id = $devA } $null) $tokA
Check '[1.12] Device/Subscribe over HTTP -> Unsupported' ($http.subtype -eq 3 -and $http.meta.code_id -eq 1102) (Describe $http)

# [1.13] logging in as somebody else on a live connection lets go of the old identity's queue.
# Without this, alice's to-device items — her Megolm keys — are pushed into a connection that is
# now bob's, and the queue stays held by a session that may no longer destroy from it.
$ws3 = Ws-Open $tokA
$heldA = Call $ws3 (Json-Pack 0x16 4 (Conv 30) 0 @{ device_id = $devA } $null)
$swap = Call $ws3 (Json-Pack 16 1 0 31 @{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = 'bob' }; password = 'pw-pw-pw-pw'; initial_device_display_name = 'e2e12 swap' } $null)
$null = Send-ToDevice $tokB $regA.user_id $devA 'after the identity swap'
$leaked = @(Drain $ws3 2000 | Where-Object { $_.kind -eq 0x16 -and $_.subtype -eq 6 })
Check '[1.13] after a Login as another user, the old device queue is let go and nothing is pushed to it' `
  ($heldA.subtype -eq 2 -and $swap.subtype -eq 2 -and $leaked.Count -eq 0) `
  "subscribe=$($heldA.subtype) login=$($swap.subtype) leaked=$($leaked.Count)"
$ws3.Dispose()

$ws.Dispose(); $ws2.Dispose()
Stop-Server $server

# ================= Scenario 2: a catch-up configured to zero leaves no gap behind =================
# wbf_device_fetch_default_limit = 0 makes Device/Subscribe{cd_seq} catch up nothing. Nothing was cut short, so
# the next live Push -- about an item that arrived afterwards -- must say gap=false (PR #53 review, rumia).
Log '################ Scenario 2: zero catch-up ################'
$cfg2 = Write-Config12 $db @('wbf_device_fetch_default_limit = 0')
$server = Start-Server $cfg2 's2'
$wsZ = Ws-Open $tokA
$heldZ = Call $wsZ (Json-Pack 0x16 4 (Conv 40) 0 @{ device_id = $devA; cd_seq = 0 } $null)
$null = Send-ToDevice $tokB $regA.user_id $devA 'live after a zero catch-up'
$livePushes = @(Drain $wsZ 2000 | Where-Object { $_.kind -eq 0x16 -and $_.subtype -eq 6 })
Check '[2.1] catch-up limit of 0: the first live Push says gap=false' ($heldZ.subtype -eq 2 -and $livePushes.Count -ge 1 -and $livePushes[0].meta.gap -eq $false) "subscribe=$($heldZ.subtype) pushes=$($livePushes.Count) gap=$($livePushes[0].meta.gap)"
$wsZ.Dispose()
Stop-Server $server

# ================= Scenario 3: Fetch without cd_seq, paging by destroying =================
# ⭐ The rule 維護者 2026-09-26 定的 (issue #87, wbf-to-device.md §3.1.2): the correct call carries no
# cd_seq, and it means "from the oldest item that has not been destroyed". The queue head is the
# waterline; the client stores no number. Paging is: destroy the window, then ask again.
#
# 🚨 This is already the behaviour -- the point of the scenario is that it becomes a promise. Nothing
# else stops someone giving cd_seq a default waterline later, and that filter is exactly what made
# three key-losing paths possible on the client side (client PR #60, three reviewers).
Log '################ Scenario 3: Fetch without cd_seq ################'
# 🚨 A fresh database on purpose: the queue has to hold exactly the two items this scenario sends.
# Scenario 1 leaves items behind by design ([1.9] refuses to destroy `fourth` because tc lies) and
# scenario 2 never destroys its own -- reusing $db here would make [3.1] count those too.
$db3 = "$S\e2e12db3"; Remove-Item -Recurse -Force $db3 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db3 | Out-Null
$cfg3 = Write-Config12 $db3 @()
$server = Start-Server $cfg3 's3'
$reg3A = Register 'alice'; $tok3A = $reg3A.access_token; $dev3A = $reg3A.device_id
$reg3B = Register 'bob'; $tok3B = $reg3B.access_token
$ws3 = Ws-Open $tok3A
$null = Call $ws3 (Json-Pack 0x16 4 (Conv 50) 0 @{ device_id = $dev3A } $null)
$null = Drain $ws3 1500
$null = Send-ToDevice $tok3B $reg3A.user_id $dev3A 'oldest'
$null = Send-ToDevice $tok3B $reg3A.user_id $dev3A 'newest'
$null = Drain $ws3 2000

function Fetch-Oldest($ws, [uint64]$id, $meta) {
  $packs = @()
  Ws-Send $ws (Json-Pack 0x16 1 (Conv $id) 0 $meta $null)
  do { $batch = Recv-Or-Null $ws 5000; if ($null -eq $batch) { break }; if ($batch.kind -eq 0x16 -and $batch.subtype -eq 2) { $packs += ,$batch } } while ($batch.meta.r -ne 0)
  $packs
}

$both = Fetch-Oldest $ws3 51 @{}
$bothBodies = @($both | ForEach-Object { Items ([byte[]]$_.data) } | ForEach-Object { $_.content.body })
Check '[3.1] Fetch with no cd_seq at all returns the whole queue, oldest first' `
  ($bothBodies.Count -eq 2 -and $bothBodies[0] -eq 'oldest' -and $bothBodies[1] -eq 'newest') "bodies=$($bothBodies -join ',')"

# limit=1 stops the window after the oldest; `more` says there is another behind it.
$firstWindow = Fetch-Oldest $ws3 52 @{ limit = 1 }
$firstCounts = @($firstWindow | ForEach-Object { Counts $_ })
$firstBodies = @($firstWindow | ForEach-Object { Items ([byte[]]$_.data) } | ForEach-Object { $_.content.body })
Check '[3.2] limit=1 gives the oldest one and says more=true: the head is the waterline' `
  ($firstBodies.Count -eq 1 -and $firstBodies[0] -eq 'oldest' -and $firstWindow[-1].meta.more -eq $true) `
  "bodies=$($firstBodies -join ',') more=$($firstWindow[-1].meta.more)"

# 🚨 The paging promise: destroy that window, ask again with no cd_seq, get the NEXT one. If a
# default waterline ever crept in, this second call would answer empty and the item would be
# unreachable -- which is the failure this whole rule exists to make impossible.
$destroyed3 = Destroy $ws3 53 $firstCounts
$secondWindow = Fetch-Oldest $ws3 54 @{ limit = 1 }
$secondBodies = @($secondWindow | ForEach-Object { Items ([byte[]]$_.data) } | ForEach-Object { $_.content.body })
Check '[3.3] after destroying that window, Fetch with no cd_seq gives the next one: destroying is the paging handle' `
  ($null -ne $destroyed3.result -and $secondBodies.Count -eq 1 -and $secondBodies[0] -eq 'newest' -and $secondWindow[-1].meta.more -eq $false) `
  "destroyed=$($destroyed3.result.meta.bc) bodies=$($secondBodies -join ',') more=$($secondWindow[-1].meta.more)"

# And a destroyed count never comes back, so the walk always terminates.
$null = Destroy $ws3 55 (@($secondWindow | ForEach-Object { Counts $_ }))
$emptied = Fetch-Oldest $ws3 56 @{}
Check '[3.4] with everything destroyed the queue is empty: destroyed counts never come back' `
  ($emptied.Count -eq 1 -and $emptied[0].meta.tc -eq 0 -and $emptied[0].meta.r -eq 0) (Describe $emptied[0])
$ws3.Dispose()
Stop-Server $server

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"

# A pending ReceiveAsync or an undisposed socket can keep this process alive long after the
# last line is written — every batch run this session looked like a hang for that reason, with
# the results already on disk. Leave on purpose, and say in the exit code whether it passed:
# a FAIL used to be invisible to anything that only looked at the exit status.
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
