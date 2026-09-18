. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
# E2EE (B): Device/CryptoState (docs/design/wbf-e2ee.md §3) — a device's own key supply, pushed to the connection
# holding its to-device queue: once after Subscribe, and whenever one-time or fallback keys are added or taken.
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
function Subscribe-Device($ws, [uint64]$conversation, $device) {
  Ws-Send $ws (Json-Pack 0x16 4 (Conv $conversation) 0 @{ device_id = $device } $null)
  $ack = Recv-Or-Null $ws 5000
  @{ ack = $ack; packs = (Drain $ws) }
}
function Register($name) { Api Post '/_matrix/client/v3/register' ('{"username":"' + $name + '","password":"pw-pw-pw-pw","auth":{"type":"m.login.dummy"}}') $null }
function Claim($tok, $user, $device) { Http POST '/_matrix/client/v3/keys/claim' @{ one_time_keys = @{ $user = @{ $device = 'signed_curve25519' } } } $tok }

Log '################ Scenario 1: CryptoState, a device''s own key supply ################'
$db = "$S\e2e15db"; Remove-Item -Recurse -Force $db -EA SilentlyContinue; New-Item -ItemType Directory -Force $db | Out-Null
$cfg = Write-Config $db 86400
$server = Start-Server $cfg 's1'
$regA = Register 'alice'; $regB = Register 'bob'
$alice = $regA.user_id; $devA = $regA.device_id; $tokA = $regA.access_token; $tokB = $regB.access_token

$wsA = Ws-Open $tokA
$first = Subscribe-Device $wsA 10 $devA
$firstCrypto = Only-Crypto $first.packs
$m = if ($firstCrypto.Count -gt 0) { $firstCrypto[0].meta } else { $null }
Check '[1.1] Subscribe is followed by one CryptoState: every field present (unused_fallback_key_types as []), the Subscribe''s id, nothing else' `
  ($first.ack.subtype -eq 2 -and $firstCrypto.Count -eq 1 -and $null -ne $m.otk_counts -and $firstCrypto[0].metaText.Contains('"unused_fallback_key_types":[]') -and $m.gap -eq $false -and $firstCrypto[0].id -eq (Conv 10) -and $null -eq $m.device_lists -and $null -eq $m.dl_seq) `
  "ack=$($first.ack.metaText) crypto=$(if ($m) { $firstCrypto[0].metaText } else { 'none' })"

$upload = Http POST '/_matrix/client/v3/keys/upload' @{ one_time_keys = @{ 'signed_curve25519:AAAA1' = @{ key = 'a2V5MQ'; signatures = @{} }; 'signed_curve25519:AAAA2' = @{ key = 'a2V5Mg'; signatures = @{} } }; fallback_keys = @{ 'signed_curve25519:FALL1' = @{ key = 'ZmFsbA'; fallback = $true; signatures = @{} } } } $tokA
$afterUpload = Only-Crypto (Drain $wsA)
$last = if ($afterUpload.Count -gt 0) { $afterUpload[$afterUpload.Count - 1].meta } else { $null }
Check '[1.2] uploading OTKs and a fallback key pushes the new counts, and the fallback key as unused' `
  ($upload.status -eq 200 -and $afterUpload.Count -ge 1 -and $last.otk_counts.signed_curve25519 -eq 2 -and @($last.unused_fallback_key_types) -contains 'signed_curve25519') `
  "pushes=$($afterUpload.Count) last=$(if ($last) { $afterUpload[$afterUpload.Count - 1].metaText } else { 'none' })"

# An upload that changes nothing: the same OTK again and no fallback key. Both add_* run on every upload.
$again = Http POST '/_matrix/client/v3/keys/upload' @{ one_time_keys = @{ 'signed_curve25519:AAAA1' = @{ key = 'a2V5MQ'; signatures = @{} } } } $tokA
$afterNoChange = Only-Crypto (Drain $wsA)
Check '[1.2b] an upload that adds no key (a repeated OTK, no fallback key) pushes nothing' `
  ($again.status -eq 200 -and $again.json.one_time_key_counts.signed_curve25519 -eq 2 -and $afterNoChange.Count -eq 0) `
  "upload=$($again.text) pushes=$(@($afterNoChange | ForEach-Object { $_.metaText }) -join ' | ')"

$claim = Claim $tokB $alice $devA
$afterClaim = Only-Crypto (Drain $wsA)
Check '[1.3] another user claiming one of the keys pushes the count down' `
  ($claim.status -eq 200 -and $afterClaim.Count -eq 1 -and $afterClaim[0].meta.otk_counts.signed_curve25519 -eq 1) `
  "claim=$($claim.status) pushes=$(@($afterClaim | ForEach-Object { $_.metaText }) -join ' | ')"

# Two more claims: the last OTK, then the fallback key, which is marked used.
$null = Claim $tokB $alice $devA
$fallbackClaim = Claim $tokB $alice $devA
$afterFallback = Only-Crypto (Drain $wsA)
$lastFallback = if ($afterFallback.Count -gt 0) { $afterFallback[$afterFallback.Count - 1] } else { $null }
$fallbackKeyIds = @($fallbackClaim.json.one_time_keys.$alice.$devA.PSObject.Properties.Name)
Check '[1.4] once the OTKs run out a claim takes the fallback key, and the pushed state says it is used: []' `
  ($fallbackKeyIds -contains 'signed_curve25519:FALL1' -and $null -ne $lastFallback -and $lastFallback.metaText.Contains('"unused_fallback_key_types":[]')) `
  "claimed=$($fallbackKeyIds -join ',') pushes=$(@($afterFallback | ForEach-Object { $_.metaText }) -join ' | ')"

$wsA2 = Ws-Open $tokA
$taken = Subscribe-Device $wsA2 12 $devA
$superseded = Drain $wsA 1500
$null = Http POST '/_matrix/client/v3/keys/upload' @{ one_time_keys = @{ 'signed_curve25519:AAAA9' = @{ key = 'a2V5OQ'; signatures = @{} } } } $tokA
$txn = [guid]::NewGuid().ToString('N')
$null = Http PUT "/_matrix/client/v3/sendToDevice/m.e2e15/$txn" @{ messages = @{ $alice = @{ $devA = @{ body = 'x' } } } } $tokB
$onNew = Drain $wsA2
$onOld = Drain $wsA 1500
$ours = @($taken.packs + $onNew | Where-Object { $_.kind -eq 0x16 -and ($_.subtype -eq $CRYPTO_STATE -or $_.subtype -eq $PUSH) -and $_.id -eq (Conv 12) })
$seqs = @($ours | ForEach-Object { [int]$_.seq })
$isContiguous = $seqs.Count -ge 3; for ($n = 1; $n -lt $seqs.Count; $n++) { if ($seqs[$n] -ne $seqs[$n - 1] + 1) { $isContiguous = $false } }
Check '[1.5] a later connection takes the subscription: it alone gets the next CryptoState and Push, one seq across both kinds' `
  ($isContiguous -and @($ours | Where-Object { $_.subtype -eq $PUSH }).Count -ge 1 -and @($onOld | Where-Object { $_.kind -eq 0x16 }).Count -eq 0) `
  "seqs=$($seqs -join ',') kinds=$(@($ours | ForEach-Object { $_.subtype }) -join ',') old=$(@($onOld | ForEach-Object { "$($_.kind)/$($_.subtype)" }) -join ',') superseded=$(@($superseded | ForEach-Object { $_.metaText }) -join ' | ')"

$changes = Http GET '/_matrix/client/v3/keys/changes?from=0&to=999999999999' $null $tokA
Check '[1.6] Matrix''s own key distribution is untouched: /keys/changes still answers left as upstream does (empty)' `
  ($changes.status -eq 200 -and @($changes.json.left).Count -eq 0) "keys/changes=$($changes.text)"

$wsA.Dispose(); $wsA2.Dispose()
Stop-Server $server

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
