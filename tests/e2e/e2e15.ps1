. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
# E2EE (B): Device/CryptoState (docs/design/wbf-e2ee.md §3). One key-count push per change to a device's own keys,
# one device-list push per change in who shares an encrypted room, a catch-up from dl_seq on Subscribe, and the same
# answer from /sync and /keys/changes for the same position.
$OUT = "$S\e2e15-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
$CRYPTO_STATE = 8
$PUSH = 6

# A reply within $ms, or $null. A pending receive is kept and awaited next time: an abandoned ReceiveAsync
# eats the next frame (tests/e2e/README.md).
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
    if ($r.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close) { return $null }
    $stream.Write($buf, 0, $r.Count)
  } while (-not $r.EndOfMessage)
  $p = Read-Pack ($stream.ToArray()); $p.http = 'ws'; $p
}
# Every pack that arrives until $quietMs pass with nothing.
function Drain($ws, [int]$quietMs = 2500) {
  $packs = @()
  while ($true) { $p = Recv-Or-Null $ws $quietMs; if ($null -eq $p) { break }; $packs += ,$p }
  ,$packs
}
function Only-Crypto($packs) { ,@($packs | Where-Object { $_.kind -eq 0x16 -and $_.subtype -eq $CRYPTO_STATE }) }
function Changed-Of($packs) { ,@($packs | ForEach-Object { @($_.meta.device_lists.changed) } | Where-Object { $_ } | Sort-Object -Unique) }
function Left-Of($packs) { ,@($packs | ForEach-Object { @($_.meta.device_lists.left) } | Where-Object { $_ } | Sort-Object -Unique) }
function Set-Text($items) { (@($items) | Sort-Object -Unique) -join ',' }
function Subscribe-Device($ws, [uint64]$conversation, $device, $dlSeq) {
  $meta = @{ device_id = $device }; if ($null -ne $dlSeq) { $meta.dl_seq = $dlSeq }
  Ws-Send $ws (Json-Pack 0x16 4 (Conv $conversation) 0 $meta $null)
  $ack = Recv-Or-Null $ws 5000
  @{ ack = $ack; packs = (Drain $ws) }
}
function Register($name) { Api Post '/_matrix/client/v3/register' ('{"username":"' + $name + '","password":"pw-pw-pw-pw","auth":{"type":"m.login.dummy"}}') $null }
function Create-Encrypted-Room($tok) {
  (Http POST '/_matrix/client/v3/createRoom' @{ preset = 'public_chat'; initial_state = @(@{ type = 'm.room.encryption'; state_key = ''; content = @{ algorithm = 'm.megolm.v1.aes-sha2' } }) } $tok).json.room_id
}
function Enc([string]$text) { [uri]::EscapeDataString($text) }
function Device-Keys($user, $device) {
  @{ user_id = $user; device_id = $device; algorithms = @('m.olm.v1.curve25519-aes-sha2', 'm.megolm.v1.aes-sha2')
     keys = @{ "curve25519:$device" = 'Y3VydmUyNTUxOWtleWZvcmUyZTE1Y3J5cHRvc3RhdGU'; "ed25519:$device" = 'ZWQyNTUxOWtleWZvcmUyZTE1Y3J5cHRvc3RhdGV4eA' }
     signatures = @{ $user = @{ "ed25519:$device" = 'c2lnbmF0dXJlZm9yZTJlMTU' } } }
}

Log '################ Scenario 1: CryptoState pushes, catch-up, and the same answer as /sync and /keys/changes ################'
$db = "$S\e2e15db"; Remove-Item -Recurse -Force $db -EA SilentlyContinue; New-Item -ItemType Directory -Force $db | Out-Null
$cfg = Write-Config $db 86400
$server = Start-Server $cfg 's1'
$regA = Register 'alice'; $regB = Register 'bob'; $regC = Register 'carol'; $regD = Register 'dave'
$alice = $regA.user_id; $bob = $regB.user_id; $carol = $regC.user_id; $dave = $regD.user_id
$tokA = $regA.access_token; $tokB = $regB.access_token; $tokC = $regC.access_token; $tokD = $regD.access_token

# Before the subscription: alice and bob share encrypted room 1; alice and dave share encrypted room 2.
$room1 = Create-Encrypted-Room $tokA
$null = Http POST "/_matrix/client/v3/join/$(Enc $room1)" @{} $tokB
$room2 = Create-Encrypted-Room $tokD
$null = Http POST "/_matrix/client/v3/join/$(Enc $room2)" @{} $tokA

$wsA = Ws-Open $tokA
$first = Subscribe-Device $wsA 10 $regA.device_id $null
$firstCrypto = Only-Crypto $first.packs
$s1 = [uint64]$first.ack.meta.latest_cd_seq
$m = if ($firstCrypto.Count -gt 0) { $firstCrypto[0].meta } else { $null }
Check '[1.1] Subscribe without dl_seq: one CryptoState, every field present, empty lists, dl_seq = latest_cd_seq, the Subscribe''s id' `
  ($firstCrypto.Count -eq 1 -and $null -ne $m.otk_counts -and $null -ne $m.unused_fallback_key_types -and @($m.device_lists.changed).Count -eq 0 -and @($m.device_lists.left).Count -eq 0 -and [uint64]$m.dl_seq -eq $s1 -and $m.gap -eq $false -and $firstCrypto[0].id -eq (Conv 10) -and $firstCrypto[0].metaText.Contains('"unused_fallback_key_types":[]')) `
  "ack=$($first.ack.metaText) crypto=$(if ($m) { $firstCrypto[0].metaText } else { 'none' })"

$upload = Http POST '/_matrix/client/v3/keys/upload' @{ one_time_keys = @{ 'signed_curve25519:AAAA1' = @{ key = 'a2V5MQ'; signatures = @{} }; 'signed_curve25519:AAAA2' = @{ key = 'a2V5Mg'; signatures = @{} }; 'signed_curve25519:AAAA3' = @{ key = 'a2V5Mw'; signatures = @{} } }; fallback_keys = @{ 'signed_curve25519:FALL1' = @{ key = 'ZmFsbA'; fallback = $true; signatures = @{} } } } $tokA
$afterUpload = Only-Crypto (Drain $wsA)
$last = if ($afterUpload.Count -gt 0) { $afterUpload[$afterUpload.Count - 1].meta } else { $null }
Check '[1.2] uploading OTKs and a fallback key pushes the new counts, and the fallback key as unused' `
  ($upload.status -eq 200 -and $afterUpload.Count -ge 1 -and $last.otk_counts.signed_curve25519 -eq 3 -and @($last.unused_fallback_key_types) -contains 'signed_curve25519') `
  "pushes=$($afterUpload.Count) last=$(if ($last) { $afterUpload[$afterUpload.Count - 1].metaText } else { 'none' })"

$claim = Http POST '/_matrix/client/v3/keys/claim' @{ one_time_keys = @{ $alice = @{ $regA.device_id = 'signed_curve25519' } } } $tokB
$afterClaim = Only-Crypto (Drain $wsA)
Check '[1.3] another user claiming one of alice''s keys pushes the count down, with no user in the lists' `
  ($claim.status -eq 200 -and $afterClaim.Count -eq 1 -and $afterClaim[0].meta.otk_counts.signed_curve25519 -eq 2 -and @($afterClaim[0].meta.device_lists.changed).Count -eq 0 -and [uint64]$afterClaim[0].meta.dl_seq -eq $s1) `
  "claim=$($claim.status) pushes=$(@($afterClaim | ForEach-Object { $_.metaText }) -join ' | ')"

# ---- device lists, live ----
$bobKeys = Http POST '/_matrix/client/v3/keys/upload' @{ device_keys = (Device-Keys $bob $regB.device_id) } $tokB
$afterBobKeys = Only-Crypto (Drain $wsA)
Check '[1.4] bob (sharing an encrypted room) uploads device keys: alice is pushed changed [bob]' `
  ($bobKeys.status -eq 200 -and (Set-Text (Changed-Of $afterBobKeys)) -eq $bob) "pushes=$(@($afterBobKeys | ForEach-Object { $_.metaText }) -join ' | ')"

$wsC = Ws-Open $tokC
$cSub = Subscribe-Device $wsC 20 $regC.device_id $null
$null = Http POST "/_matrix/client/v3/join/$(Enc $room1)" @{} $tokC
$aliceSeesCarolJoin = Only-Crypto (Drain $wsA)
$carolSeesJoin = Only-Crypto (Drain $wsC)
Check '[1.5] carol joins room 1: alice is pushed changed [carol]; carol''s own device is pushed changed [alice, bob]' `
  ((Set-Text (Changed-Of $aliceSeesCarolJoin)) -eq $carol -and (Set-Text (Changed-Of $carolSeesJoin)) -eq (Set-Text @($alice, $bob))) `
  "alice=$(@($aliceSeesCarolJoin | ForEach-Object { $_.metaText }) -join ' | ') carol=$(@($carolSeesJoin | ForEach-Object { $_.metaText }) -join ' | ')"

$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room1)/leave" @{} $tokC
$aliceSeesCarolLeave = Only-Crypto (Drain $wsA)
$carolSeesLeave = Only-Crypto (Drain $wsC)
Check '[1.6] carol leaves: alice is pushed left [carol]; carol, who left herself, is pushed left [alice, bob] (decision 7)' `
  ((Set-Text (Left-Of $aliceSeesCarolLeave)) -eq $carol -and (Set-Text (Left-Of $carolSeesLeave)) -eq (Set-Text @($alice, $bob))) `
  "alice=$(@($aliceSeesCarolLeave | ForEach-Object { $_.metaText }) -join ' | ') carol=$(@($carolSeesLeave | ForEach-Object { $_.metaText }) -join ' | ')"
$wsC.Dispose()

$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room2)/leave" @{} $tokA
$aliceLeftRoom2 = Only-Crypto (Drain $wsA)
Check '[1.7] alice leaves room 2 herself: pushed left [dave], the only one she stopped sharing with (decision 7)' `
  ((Set-Text (Left-Of $aliceLeftRoom2)) -eq $dave) "pushes=$(@($aliceLeftRoom2 | ForEach-Object { $_.metaText }) -join ' | ')"
$wsA.Dispose()

# ---- catch-up from s1, against /sync and /keys/changes ----
$wsA2 = Ws-Open $tokA
$again = Subscribe-Device $wsA2 11 $regA.device_id $s1
$caught = Only-Crypto $again.packs
$caughtChanged = Changed-Of $caught; $caughtLeft = Left-Of $caught
$sync = Http GET "/_matrix/client/v3/sync?since=$s1&timeout=0" $null $tokA
$syncChanged = @($sync.json.device_lists.changed); $syncLeft = @($sync.json.device_lists.left)
# A user who joined and left inside the window: /sync may list them in both, the catch-up only in left (their join
# row is gone). Both say "no longer shares"; compare changed without the users in left.
$syncChangedOnly = @($syncChanged | Where-Object { $syncLeft -notcontains $_ })
$caughtChangedOnly = @($caughtChanged | Where-Object { $caughtLeft -notcontains $_ })
Check '[1.8] Subscribe with dl_seq = s1 catches up what /sync since s1 reports: bob changed; carol and dave left' `
  ($caught.Count -ge 1 -and (Set-Text $caughtLeft) -eq (Set-Text @($carol, $dave)) -and (Set-Text $caughtLeft) -eq (Set-Text $syncLeft) -and (Set-Text $caughtChangedOnly) -eq (Set-Text $syncChangedOnly) -and $caughtChangedOnly -contains $bob) `
  "catch-up changed=$(Set-Text $caughtChanged) left=$(Set-Text $caughtLeft) | sync changed=$(Set-Text $syncChanged) left=$(Set-Text $syncLeft) sync=$($sync.status)"

$changes = Http GET "/_matrix/client/v3/keys/changes?from=$s1&to=999999999999" $null $tokA
Check '[1.9] /keys/changes from s1: left is no longer empty, and it is the catch-up''s answer' `
  ($changes.status -eq 200 -and (Set-Text $changes.json.left) -eq (Set-Text $caughtLeft) -and (Set-Text $changes.json.changed) -eq (Set-Text $caughtChanged)) `
  "keys/changes=$($changes.text)"

# ---- one subscription: seq shared with Push, and the holder only ----
$wsA3 = Ws-Open $tokA
$taken = Subscribe-Device $wsA3 12 $regA.device_id $null
$superseded = Drain $wsA2 1500
$null = Http POST '/_matrix/client/v3/keys/claim' @{ one_time_keys = @{ $alice = @{ $regA.device_id = 'signed_curve25519' } } } $tokB
$txn = [guid]::NewGuid().ToString('N')
$null = Http PUT "/_matrix/client/v3/sendToDevice/m.e2e15/$txn" @{ messages = @{ $alice = @{ $regA.device_id = @{ body = 'x' } } } } $tokB
$onNew = Drain $wsA3
$onOld = Drain $wsA2 1500
$ours = @($taken.packs + $onNew | Where-Object { $_.kind -eq 0x16 -and ($_.subtype -eq $CRYPTO_STATE -or $_.subtype -eq $PUSH) -and $_.id -eq (Conv 12) })
$seqs = @($ours | ForEach-Object { [int]$_.seq })
$isContiguous = $seqs.Count -ge 3; for ($n = 1; $n -lt $seqs.Count; $n++) { if ($seqs[$n] -ne $seqs[$n - 1] + 1) { $isContiguous = $false } }
Check '[1.10] a later connection takes the subscription: it alone gets the next CryptoState and Push, one seq across both kinds' `
  ($isContiguous -and @($ours | Where-Object { $_.subtype -eq $PUSH }).Count -ge 1 -and @($onOld | Where-Object { $_.kind -eq 0x16 }).Count -eq 0) `
  "seqs=$($seqs -join ',') kinds=$(@($ours | ForEach-Object { $_.subtype }) -join ',') old=$(@($onOld | ForEach-Object { "$($_.kind)/$($_.subtype)" }) -join ',') superseded=$(@($superseded | ForEach-Object { $_.metaText }) -join ' | ')"

$wsA2.Dispose(); $wsA3.Dispose()
Stop-Server $server

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
