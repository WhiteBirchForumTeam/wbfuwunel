. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
# The two WebSocket connection limits (docs/design/wbf-pack-pipeline.md §2.1 and §2.2): the per-device one, which
# needs an identity and therefore cannot see a connection that has not logged in, and the per-address one, which is
# checked before the token is read and is the only gate an anonymous connection ever meets.
#
# ⭐ The check that matters most here is that **anonymous connections are counted**: without it the per-address
# limit would still pass a test that only opened authenticated connections, while leaving the hole it exists to
# close wide open.
$OUT = "$S\e2e17-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}

# Opens a connection and says what happened. A refused upgrade throws; the status code is in the innermost
# exception's message (the same read e2e7 does).
function Try-Open($tok, [string]$forwarded) {
  try { $ws = Ws-Open $tok $forwarded; @{ ws = $ws; ok = $true; why = 'open' } }
  catch {
    $error_ = $_.Exception; while ($error_.InnerException) { $error_ = $error_.InnerException }
    @{ ws = $null; ok = $false; why = $error_.Message }
  }
}
function Is-Refused-429($attempt) { -not $attempt.ok -and $attempt.why -match '429' }
function Close-Ws($ws) {
  if ($null -eq $ws) { return }
  try { $ws.CloseAsync([System.Net.WebSockets.WebSocketCloseStatus]::NormalClosure, 'bye', [Threading.CancellationToken]::None).Wait(3000) | Out-Null } catch {}
  try { $ws.Dispose() } catch {}
}

# The refusal's body is a pack, and `ClientWebSocket` throws away the response of a failed handshake — so the only
# way to read the `code_id` is to do the upgrade by hand and look at what comes back. The handshake is deliberately
# well-formed: a request the server would otherwise accept, refused only by the gate under test.
function Refused-Upgrade-Pack([string]$tok, [string]$forwarded) {
  $request = New-Object System.Net.Http.HttpRequestMessage ([System.Net.Http.HttpMethod]::Get, "http://127.0.0.1:8015/_wbf/v1/ws")
  $request.Headers.TryAddWithoutValidation('Connection', 'Upgrade') | Out-Null
  $request.Headers.TryAddWithoutValidation('Upgrade', 'websocket') | Out-Null
  $request.Headers.TryAddWithoutValidation('Sec-WebSocket-Version', '13') | Out-Null
  $request.Headers.TryAddWithoutValidation('Sec-WebSocket-Key', [Convert]::ToBase64String((1..16 | ForEach-Object { [byte](Get-Random -Max 256 ) }))) | Out-Null
  if ($tok) { $request.Headers.TryAddWithoutValidation('Authorization', "Bearer $tok") | Out-Null }
  if ($forwarded) { $request.Headers.TryAddWithoutValidation('X-Forwarded-For', $forwarded) | Out-Null }
  try {
    $response = $script:PackHttpClient.SendAsync($request).Result
    $bytes = $response.Content.ReadAsByteArrayAsync().Result
    $pack = if ($bytes.Length -gt 0) { Read-Pack $bytes } else { $null }
    @{ status = [int]$response.StatusCode; pack = $pack }
  } catch {
    $error_ = $_.Exception; while ($error_.InnerException) { $error_ = $error_.InnerException }
    @{ status = 0; pack = $null; error = $error_.Message }
  }
}

Log '################ Scenario 1: the per-address limit counts anonymous connections ################'
# Three per address, two per device: small enough to reach both gates, far enough apart to tell them apart.
$db1 = "$S\e2e17db1"; Remove-Item -Recurse -Force $db1 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db1 | Out-Null
$cfg1 = "$S\e2e17-1.toml"
@('[global]','server_name = "localhost"',('database_path = "' + ($db1 -replace '\\','/') + '"'),'port = 8015','address = ["127.0.0.1"]',
  'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
  'wbf_ws_idle_timeout = 60','wbf_ws_unauthenticated_timeout = 30',
  'wbf_ws_max_connections_per_address = 3','wbf_ws_max_connections_per_device = 2',
  'login_rc_per_second = 5','login_rc_burst_count = 40','log = "info"') -join "`n" | Set-Content -Path $cfg1 -Encoding ascii
$server = Start-Server $cfg1 's1'
$reg = Api Post '/_matrix/client/v3/register' '{"username":"hana","password":"pw-hana-1","auth":{"type":"m.login.dummy"}}' $null
$tok = $reg.access_token

# ---- anonymous connections are counted: this is the whole reason the limit exists ----
$anon1 = Try-Open $null
$anon2 = Try-Open $null
$anon3 = Try-Open $null
$anon4 = Try-Open $null
Check '[1.1] three anonymous connections open and the fourth is refused: the per-address limit is the only one that sees them' `
  ($anon1.ok -and $anon2.ok -and $anon3.ok -and (Is-Refused-429 $anon4)) `
  "1=$($anon1.why) 2=$($anon2.why) 3=$($anon3.why) 4=$($anon4.why)"

# 🚨 The point of the previous check, stated as its own: the per-device limit is 2, and these are three connections
# from one machine. They got past it because it counts (user, device) and an anonymous connection has neither.
Check '[1.2] those three got past the per-device limit of 2, because that limit cannot see a connection with no identity' `
  ($anon1.ok -and $anon2.ok -and $anon3.ok) "per_device=2 anonymous_open=3"

$refused = Refused-Upgrade-Pack $null
$refusedCode = if ($null -ne $refused.pack) { $refused.pack.meta.code } else { '(no pack)' }
$refusedId = if ($null -ne $refused.pack) { [int]$refused.pack.meta.code_id } else { 0 }
$refusedMax = if ($null -ne $refused.pack) { [int]$refused.pack.meta.max_connections } else { 0 }
Check '[1.3] the refusal is 429 with TooManyConnectionsFromAddress (1403) and the limit it hit, not the per-device 1402' `
  ($refused.status -eq 429 -and $refusedId -eq 1403 -and $refusedCode -eq 'TooManyConnectionsFromAddress' -and $refusedMax -eq 3) `
  "status=$($refused.status) code=$refusedCode code_id=$refusedId max=$refusedMax"

# ⚠️ The message must not tell the caller to close one of their own: at this gate the connections in the way may
# belong to somebody else behind the same address.
$refusedMessage = if ($null -ne $refused.pack) { "$($refused.pack.meta.message)" } else { '' }
Check '[1.4] the refusal does not tell the caller to close one of their own connections' `
  ($refusedMessage -ne '' -and $refusedMessage -notmatch 'close') "message=$refusedMessage"

# ---- a closed connection gives its place back ----
Close-Ws $anon3.ws
Start-Sleep -Milliseconds 800
$anon5 = Try-Open $null
Check '[1.5] closing one connection gives its place back: a new one is accepted again' `
  ($anon5.ok) "reopen=$($anon5.why)"

# ---- HTTP is not counted, even with the address full ----
$httpPing = Send-Pack (Json-Pack 1 4 0 1 @{ nonce = 7 } $null) $tok
Check '[1.6] with the address full, an HTTP pack from the same address still answers: HTTP holds no connection' `
  ($httpPing.subtype -eq 5) "$(Describe $httpPing)"

Log '################ Scenario 2: the two gates are separate ################'
# The address still has room, but the device does not: the refusal must be the device one, with its own code.
Close-Ws $anon1.ws; Close-Ws $anon2.ws; Close-Ws $anon5.ws
Start-Sleep -Milliseconds 800

$bearer1 = Try-Open $tok
$bearer2 = Try-Open $tok
$bearer3 = Try-Open $tok
$deviceRefusal = Refused-Upgrade-Pack $tok
$deviceCode = if ($null -ne $deviceRefusal.pack) { $deviceRefusal.pack.meta.code } else { '(no pack)' }
$deviceId = if ($null -ne $deviceRefusal.pack) { [int]$deviceRefusal.pack.meta.code_id } else { 0 }
Check '[2.1] two connections for one device, then the device gate refuses with TooManyConnections (1402) while the address still has room' `
  ($bearer1.ok -and $bearer2.ok -and (Is-Refused-429 $bearer3) -and $deviceRefusal.status -eq 429 -and $deviceId -eq 1402 -and $deviceCode -eq 'TooManyConnections') `
  "1=$($bearer1.why) 2=$($bearer2.why) 3=$($bearer3.why) code=$deviceCode code_id=$deviceId"

# The two open bearer connections are two of the address's three places, so one anonymous still fits and the next
# does not — and that one is refused by the address gate, not the device gate.
$anonWithBearers = Try-Open $null
$addressRefusal = Refused-Upgrade-Pack $null
$addressId = if ($null -ne $addressRefusal.pack) { [int]$addressRefusal.pack.meta.code_id } else { 0 }
Check '[2.2] the same address, now full of bearer connections, refuses the next one with 1403: the two counts share no state but the same connections' `
  ($anonWithBearers.ok -and $addressRefusal.status -eq 429 -and $addressId -eq 1403) `
  "anon=$($anonWithBearers.why) code_id=$addressId"

$stillAlive = Ws-Call $bearer1.ws (Json-Pack 1 4 0 2 @{ nonce = 9 } $null)
Check '[2.3] the connections already open are untouched by the refusals: the new one is turned away, never an old one' `
  ($stillAlive.subtype -eq 5) "$(Describe $stillAlive)"

$hello = Ws-Call $bearer2.ws (Json-Pack 1 1 0 3 @{ protocol = 1; client = 'e2e17'; features = @() } $null)
Check '[2.4] Hello reports both limits, so a client can tell how many it may open' `
  ([int]$hello.meta.max_connections_per_device -eq 2 -and [int]$hello.meta.max_connections_per_address -eq 3) `
  "per_device=$($hello.meta.max_connections_per_device) per_address=$($hello.meta.max_connections_per_address)"

Close-Ws $bearer1.ws; Close-Ws $bearer2.ws; Close-Ws $anonWithBearers.ws
Stop-Server $server

Log '################ Scenario 3: zero means unlimited ################'
$db2 = "$S\e2e17db2"; Remove-Item -Recurse -Force $db2 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db2 | Out-Null
$cfg2 = "$S\e2e17-2.toml"
@('[global]','server_name = "localhost"',('database_path = "' + ($db2 -replace '\\','/') + '"'),'port = 8015','address = ["127.0.0.1"]',
  'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
  'wbf_ws_idle_timeout = 60','wbf_ws_unauthenticated_timeout = 30',
  'wbf_ws_max_connections_per_address = 0','wbf_ws_max_connections_per_device = 2','log = "info"') -join "`n" | Set-Content -Path $cfg2 -Encoding ascii
$server = Start-Server $cfg2 's3'
$opened = @()
$allOpen = $true
for ($n = 0; $n -lt 6; $n++) {
  $attempt = Try-Open $null
  if (-not $attempt.ok) { $allOpen = $false }
  $opened += $attempt
}
Check '[3.1] with the limit set to 0 the address gate takes no place at all: six anonymous connections open' `
  ($allOpen) "opened=$(@($opened | Where-Object { $_.ok }).Count)/6"
foreach ($attempt in $opened) { Close-Ws $attempt.ws }
Stop-Server $server

Log '################ Scenario 4: a loopback peer names the client (localhost_ip, default) ################'
# ⭐ This is the rule 維護者 2026-09-25 added, and the only e2e that reaches it: the script connects from
# 127.0.0.1, which is in the default localhost_ip, so X-Forwarded-For decides which bucket each connection
# counts against. Without it every same-host-proxy and unix-socket deployment would put every client in one
# bucket — the collapse cirno found in the PR #85 second review.
$db3 = "$S\e2e17db3"; Remove-Item -Recurse -Force $db3 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db3 | Out-Null
$cfg3 = "$S\e2e17-3.toml"
@('[global]','server_name = "localhost"',('database_path = "' + ($db3 -replace '\\','/') + '"'),'port = 8015','address = ["127.0.0.1"]',
  'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
  'wbf_ws_idle_timeout = 60','wbf_ws_unauthenticated_timeout = 30',
  'wbf_ws_max_connections_per_address = 2','wbf_ws_max_connections_per_device = 2','log = "info"') -join "`n" | Set-Content -Path $cfg3 -Encoding ascii
$server = Start-Server $cfg3 's4'

# Three clients behind the same local proxy, three addresses: the limit of 2 is per client, not per proxy.
$separate = @('198.51.100.1','198.51.100.2','198.51.100.3') | ForEach-Object { Try-Open $null $_ }
Check '[4.1] three connections forwarded from three addresses all open: the header, not the loopback peer, is what is counted' `
  (@($separate | Where-Object { $_.ok }).Count -eq 3) "opened=$(@($separate | Where-Object { $_.ok }).Count)/3"
foreach ($attempt in $separate) { Close-Ws $attempt.ws }

# Same client twice over, then a third: the limit still bites, it just bites the right party.
$sameA = Try-Open $null '198.51.100.9'
$sameB = Try-Open $null '198.51.100.9'
$sameC = Try-Open $null '198.51.100.9'
Check '[4.2] two from one forwarded address open and the third is refused: the limit follows the client' `
  ($sameA.ok -and $sameB.ok -and (Is-Refused-429 $sameC)) "1=$($sameA.why) 2=$($sameB.why) 3=$($sameC.why)"

# A fourth client is untouched by the third one's refusal — the proof that these are separate buckets and not
# one shared count that happens to be full.
$other = Try-Open $null '198.51.100.8'
Check '[4.3] another forwarded address still gets in while that one is full: separate buckets, not one shared count' `
  ($other.ok) "other=$($other.why)"

$refused = Refused-Upgrade-Pack $null '198.51.100.9'
Check '[4.4] the refusal is still 1403 with the same remedy: the forwarded address changed who is counted, nothing else' `
  ($refused.status -eq 429 -and $refused.pack -and $refused.pack.meta.code_id -eq 1403) `
  "status=$($refused.status) code=$($refused.pack.meta.code) code_id=$($refused.pack.meta.code_id)"

Close-Ws $sameA.ws; Close-Ws $sameB.ws; Close-Ws $other.ws
Stop-Server $server

Log '################ Scenario 5: localhost_ip = [] turns the header off again ################'
# 🚨 Same script, same headers, opposite outcome — and the only thing that changed is one config line. That is
# what makes this pair worth having: it pins the rule to the setting rather than to the server happening to
# behave a certain way.
$db4 = "$S\e2e17db4"; Remove-Item -Recurse -Force $db4 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db4 | Out-Null
$cfg4 = "$S\e2e17-4.toml"
@('[global]','server_name = "localhost"',('database_path = "' + ($db4 -replace '\\','/') + '"'),'port = 8015','address = ["127.0.0.1"]',
  'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
  'wbf_ws_idle_timeout = 60','wbf_ws_unauthenticated_timeout = 30','localhost_ip = []',
  'wbf_ws_max_connections_per_address = 2','wbf_ws_max_connections_per_device = 2','log = "info"') -join "`n" | Set-Content -Path $cfg4 -Encoding ascii
$server = Start-Server $cfg4 's5'

$ignored = @('198.51.100.1','198.51.100.2','198.51.100.3') | ForEach-Object { Try-Open $null $_ }
Check '[5.1] with localhost_ip emptied the same three headers are ignored: all three count as the loopback peer, so the third is refused' `
  ($ignored[0].ok -and $ignored[1].ok -and (Is-Refused-429 $ignored[2])) `
  "1=$($ignored[0].why) 2=$($ignored[1].why) 3=$($ignored[2].why)"
foreach ($attempt in $ignored) { Close-Ws $attempt.ws }
Stop-Server $server

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
