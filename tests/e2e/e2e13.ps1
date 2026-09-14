. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
# The bridge: a pack flagged IS_BRIDGED (bit4) is a Matrix endpoint call (docs/design/wbf-api-bridge.md,
# numbers in docs/bridge-specs/index.md). Scenario 0 pins the layer itself: the split on bit4, each road looking
# only at its own table, and every reply to a bridge call saying so. The endpoint scenarios come with the batches.
$OUT = "$S\e2e13-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
$IS_RESPONSE = 0x04
$IS_BRIDGED = 0x10
$UNKNOWN_KIND = 1101
$CORRUPT = 1002
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
    if ($r.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close -or ($r.Count -eq 0 -and -not $r.EndOfMessage)) { return $null }
    $stream.Write($buf, 0, $r.Count)
  } while (-not $r.EndOfMessage)
  $p = Read-Pack ($stream.ToArray()); $p.http = 'ws'; $p
}
function Call($ws, [byte[]]$pack) { Ws-Send $ws $pack; Recv-Or-Null $ws 5000 }
function Bridged-Pack([byte]$kind, [byte]$subtype, [uint32]$seq, $variables, [byte[]]$body) {
  $meta = if ($null -ne $variables) { [Text.Encoding]::UTF8.GetBytes(($variables | ConvertTo-Json -Compress)) } else { @() }
  New-Pack $kind $subtype $IS_BRIDGED 0 $seq $meta $body
}
function Is-BridgedReply($p) { $null -ne $p -and ($p.flags -band ($IS_RESPONSE -bor $IS_BRIDGED)) -eq ($IS_RESPONSE -bor $IS_BRIDGED) }

Log '################ Scenario 0: the split on bit4 ################'
$db = "$S\e2e13db"; Remove-Item -Recurse -Force $db -EA SilentlyContinue; New-Item -ItemType Directory -Force $db | Out-Null
$cfg = Write-Config $db 86400
$server = Start-Server $cfg 's0'
$reg = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"pw-pw-pw-pw","auth":{"type":"m.login.dummy"}}' $null
$tok = $reg.access_token

$ws = Ws-Open $tok
# 0x11/0x20 is WhoAmI in the specs index; this layer has an empty table, so the bridge road does not know it.
$unmapped = Call $ws (Bridged-Pack 0x11 0x20 41 $null $null)
Check '[0.1] a bridge call for a pair the bridge table does not have -> UnknownKind, and the reply carries IS_BRIDGED' `
  ($null -ne $unmapped -and $unmapped.subtype -eq 3 -and $unmapped.meta.code_id -eq $UNKNOWN_KIND -and (Is-BridgedReply $unmapped) -and $unmapped.seq -eq 41) `
  "flags=0x$('{0:X2}' -f $unmapped.flags) $(Describe $unmapped)"

$httpUnmapped = Send-Pack (Bridged-Pack 0x11 0x20 42 $null $null) $tok
Check '[0.2] the same over POST /_wbf/v1/pack: the bridge does not care about the transport' `
  ($httpUnmapped.subtype -eq 3 -and $httpUnmapped.meta.code_id -eq $UNKNOWN_KIND -and (Is-BridgedReply $httpUnmapped) -and $httpUnmapped.seq -eq 42) `
  "flags=0x$('{0:X2}' -f $httpUnmapped.flags) $(Describe $httpUnmapped)"

$native = Call $ws (New-Pack 0x11 0x20 0 0 43 @() @())
Check '[0.3] the same pair without bit4 goes the native road: UnknownKind, and the reply does not claim IS_BRIDGED' `
  ($null -ne $native -and $native.meta.code_id -eq $UNKNOWN_KIND -and ($native.flags -band $IS_BRIDGED) -eq 0) `
  "flags=0x$('{0:X2}' -f $native.flags) $(Describe $native)"

$pong = Call $ws (Json-Pack 1 4 0 44 @{ nonce = 7 } $null)
Check '[0.4] native packs are untouched: Ping still answers Pong, without IS_BRIDGED' `
  ($null -ne $pong -and $pong.subtype -eq 5 -and ($pong.flags -band $IS_BRIDGED) -eq 0) "flags=0x$('{0:X2}' -f $pong.flags)"

$bridgedPing = Call $ws (New-Pack 1 4 $IS_BRIDGED 0 45 ([Text.Encoding]::UTF8.GetBytes('{"nonce":8}')) @())
Check '[0.5] a native pair sent with bit4 is not handed to the native handler: UnknownKind on the bridge road' `
  ($null -ne $bridgedPing -and $bridgedPing.subtype -eq 3 -and $bridgedPing.meta.code_id -eq $UNKNOWN_KIND -and (Is-BridgedReply $bridgedPing)) `
  "$(Describe $bridgedPing)"
$ws.Dispose()

$reserved = Send-Pack (New-Pack 0x11 0x20 0x20 0 46 @() @()) $tok
Check '[0.6] bit5 is still reserved: Corrupt' ($reserved.subtype -eq 3 -and $reserved.meta.code_id -eq $CORRUPT) "$(Describe $reserved)"

$anonymous = Ws-Open $null
$anon = Call $anonymous (Bridged-Pack 0x11 0x20 47 $null $null)
Check '[0.7] a connection that has not logged in gets the same UnknownKind: the table is asked before anything about the session' `
  ($null -ne $anon -and $anon.meta.code_id -eq $UNKNOWN_KIND -and (Is-BridgedReply $anon)) "$(Describe $anon)"
$anonymous.Dispose()

Stop-Server $server

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
