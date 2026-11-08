. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
# Session tokens (/docs/design/accounts/session-tokens.md): an access token and a refresh token are two
# credentials with two jobs. Scenario 1 is the client-visible half of §2 -- a refresh token authenticates
# nothing, while /refresh and RFC7009 revocation still take it. Scenario 2 is §3: logging in again on the
# same device_id ends that device's previous session, including its ability to keep refreshing itself.
#
# 🚨 Both were real before PR (this branch), measured on a running server: the refresh token answered
# /whoami and wrote account data, and after a re-login the previous refresh token still minted fresh
# pairs for ever. These checks are the reverse of that run.
$OUT = "$S\e2e18-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
function Write-Config18([string]$db) {
  $cfg = "$S\e2e18.toml"
  # ⚠️ refresh_token_reuse_grace = 0 on purpose: the default 15 s window answers a reused token with a
  # 200 (the benign double-submit a flaky network produces), which would hide the replay path this
  # script wants to see. The grace itself is covered by the service's own tests.
  @('[global]','server_name = "localhost"',('database_path = "' + ($db -replace '\\','/') + '"'),'port = 8015','address = ["127.0.0.1"]',
    'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
    'refresh_token_reuse_grace = 0',
    # Pinned, not defaulted: see the note on `Write-Config` in wbf-helpers.ps1.
    'wbf_ws_idle_timeout = 120','log = "info"') -join "`n" | Set-Content -Path $cfg -Encoding ascii
  $cfg
}
function Login([string]$user, [string]$device, [bool]$wantRefresh) {
  $body = @{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = $user }
             password = 'correct-horse-battery'; device_id = $device; refresh_token = $wantRefresh }
  Http Post '/_matrix/client/v3/login' $body $null
}
function Whoami([string]$tok) { (Http Get '/_matrix/client/v3/account/whoami' $null $tok).status }
function Meta-Of($pack) { $m = $null; try { $m = $pack.metaText | ConvertFrom-Json } catch {}; $m }
# Ping (kind 1, subtype 4): the cheapest frame that still goes through `revalidate`, which is what
# asks about the session's token on every frame (/docs/design/wire/session-teardown.md §4.3).
$PING = New-Pack 1 4 0 0 1 @() @()
function Revoke([string]$tok) {
  $req = New-Object System.Net.Http.HttpRequestMessage ([System.Net.Http.HttpMethod]::Post, "$B/_tuwunel/oidc/revoke")
  $req.Content = New-Object System.Net.Http.StringContent ("token=$([uri]::EscapeDataString($tok))&token_type_hint=refresh_token", [Text.Encoding]::UTF8, 'application/x-www-form-urlencoded')
  [int]$script:PackHttpClient.SendAsync($req).Result.StatusCode
}

$db = "$S\e2e18-db"
Remove-Item -Recurse -Force $db -ErrorAction SilentlyContinue
$cfg = Write-Config18 $db

Log '################ Scenario 1: a refresh token is not an access token ################'
$p = Start-Server $cfg 's1'
try {
  $null = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
  $login = (Login 'alice' 'DEVICEONE' $true).json
  Check '[1.0] login with refresh_token:true returns both credentials and an access-token lifetime' `
    ($login.access_token -and $login.refresh_token -and $login.expires_in_ms -gt 0) "expires_in_ms=$($login.expires_in_ms)"
  Check '[1.1] the access token authenticates' ((Whoami $login.access_token) -eq 200) "status=$(Whoami $login.access_token)"

  $asBearer = Whoami $login.refresh_token
  $write = Http Put '/_matrix/client/v3/user/@alice:localhost/account_data/org.e2e18.probe' @{ probe = 1 } $login.refresh_token
  $devices = Http Get '/_matrix/client/v3/devices' $null $login.refresh_token
  Check '[1.2] the refresh token authenticates nothing: a read is refused' ($asBearer -eq 401) "whoami=$asBearer"
  Check '[1.2b] ... a write too (this one changed account data before the gate)' ($write.status -eq 401) "status=$($write.status) body=$($write.text)"
  Check '[1.2c] ... and so is /devices' ($devices.status -eq 401) "status=$($devices.status)"
  Check '[1.2d] the refusal is M_UNKNOWN_TOKEN, the code a client acts on' ($write.json.errcode -eq 'M_UNKNOWN_TOKEN') "errcode=$($write.json.errcode)"

  # ⭐ The other half of the gate: refusing it for authentication must not break what it is for.
  $refreshed = Http Post '/_matrix/client/v3/refresh' @{ refresh_token = $login.refresh_token } $null
  Check '[1.3] /refresh still takes it and returns a rotated pair' `
    ($refreshed.status -eq 200 -and $refreshed.json.access_token -and $refreshed.json.refresh_token -and $refreshed.json.refresh_token -ne $login.refresh_token) `
    "status=$($refreshed.status) rotated=$($refreshed.json.refresh_token -ne $login.refresh_token)"
  Check '[1.3b] the access token it minted authenticates' ((Whoami $refreshed.json.access_token) -eq 200) "status=$(Whoami $refreshed.json.access_token)"
  Check '[1.3c] ... and the rotated refresh token still authenticates nothing' ((Whoami $refreshed.json.refresh_token) -eq 401) "status=$(Whoami $refreshed.json.refresh_token)"

  $replay = Http Post '/_matrix/client/v3/refresh' @{ refresh_token = $login.refresh_token } $null
  Check '[1.4] replaying the spent refresh token is still refused (reuse detection untouched)' ($replay.status -eq 401) "status=$($replay.status) errcode=$($replay.json.errcode)"

  # RFC7009: the revocation endpoint takes either kind, so it has a lookup of its own.
  $revokeStatus = Revoke $refreshed.json.refresh_token
  Check '[1.5] RFC7009 revocation accepts a refresh token' ($revokeStatus -eq 200) "status=$revokeStatus"
  Check '[1.5b] ... and it really revoked the session: the access token of that device is gone' `
    ((Whoami $refreshed.json.access_token) -eq 401) "status=$(Whoami $refreshed.json.access_token)"
  $afterRevoke = Http Post '/_matrix/client/v3/refresh' @{ refresh_token = $refreshed.json.refresh_token } $null
  # ⚠️ 401 or 403: a revoked refresh token lands in the "unrecognized" branch, which answers 403
  # M_FORBIDDEN, while the spec's /refresh describes 401 M_UNKNOWN_TOKEN for a token that is unknown
  # or already used. Which one this server should give is ⏳ open
  # (/docs/design/accounts/session-tokens.md §2.5); the check pins "refused" and logs the code so the
  # answer is one assertion away.
  Check '[1.5c] ... and so is the refresh token itself' ($afterRevoke.status -in 401,403) "status=$($afterRevoke.status) errcode=$($afterRevoke.json.errcode)"

  Log '################ Scenario 2: logging in again on a device ends its previous session ################'
  $one = (Login 'alice' 'DEVICETWO' $true).json
  $elsewhere = (Login 'alice' 'DEVICETHREE' $true).json
  Check '[2.0] both devices are logged in' ((Whoami $one.access_token) -eq 200 -and (Whoami $elsewhere.access_token) -eq 200) `
    "two=$(Whoami $one.access_token) three=$(Whoami $elsewhere.access_token)"

  $two = (Login 'alice' 'DEVICETWO' $true).json
  Check '[2.1] the token from the previous login on that device is refused' ((Whoami $one.access_token) -eq 401) "status=$(Whoami $one.access_token)"
  Check '[2.1b] ... while the new one works' ((Whoami $two.access_token) -eq 200) "status=$(Whoami $two.access_token)"
  $oldRefresh = Http Post '/_matrix/client/v3/refresh' @{ refresh_token = $one.refresh_token } $null
  # Same open question as [1.5c] about which refusal code this should be.
  Check '[2.2] the previous refresh token cannot mint a new pair (it could, for ever, before the gate)' `
    ($oldRefresh.status -in 401,403) "status=$($oldRefresh.status) errcode=$($oldRefresh.json.errcode)"
  Check '[2.3] the other device is untouched: this is not "revoke everything"' ((Whoami $elsewhere.access_token) -eq 200) "status=$(Whoami $elsewhere.access_token)"
  $list = (Http Get '/_matrix/client/v3/devices' $null $two.access_token).json.devices | ForEach-Object { $_.device_id }
  Check '[2.4] the device itself is still there: a login is not a logout' ($list -contains 'DEVICETWO' -and $list -contains 'DEVICETHREE') "devices=$($list -join ',')"

  Log '################ Scenario 3: the wbf channel asks the same question ################'
  # The channel authenticates a Bearer upgrade with the same lookup, so a refresh token must not open one.
  # 🚨 `$elsewhere`'s refresh token, not `$one`'s: the re-login in [2.1] already revoked `$one`'s, so
  # refusing it would prove nothing about the kind of credential it is. This one is current.
  # 📎 A refused upgrade makes ConnectAsync throw rather than return a closed socket, and a throw is
  # the expected outcome here -- so it is caught and read as "did not open".
  $wsRefresh = $null
  $openedWithRefresh = try {
    $wsRefresh = Ws-Open $elsewhere.refresh_token
    $wsRefresh.State -eq [System.Net.WebSockets.WebSocketState]::Open
  } catch { $false }
  Check '[3.0] a refresh token does not open a channel' (-not $openedWithRefresh) "opened=$openedWithRefresh"
  if ($wsRefresh) { try { $wsRefresh.Dispose() } catch {} }

  # A channel opened with a live token keeps working; after a re-login on that device its next frame is
  # refused and the connection ends -- the same bounded teardown a locked account gets
  # (/docs/design/wire/session-teardown.md §4.3).
  $ws = Ws-Open $two.access_token
  $pong = Ws-Call $ws $PING
  Check '[3.1] a channel opened with the current access token answers' ($null -ne $pong) "reply=$(Describe $pong)"
  $three = (Login 'alice' 'DEVICETWO' $true).json
  $afterRelogin = try { Ws-Call $ws $PING } catch { $null }
  Check '[3.2] after a re-login on that device the old connection is refused at its next frame' `
    ($null -eq $afterRelogin -or (Meta-Of $afterRelogin).errcode -eq 'M_UNKNOWN_TOKEN') "reply=$(Describe $afterRelogin)"
  Check '[3.2b] ... and the token from that re-login works over HTTP' ((Whoami $three.access_token) -eq 200) "status=$(Whoami $three.access_token)"
  if ($ws) { try { $ws.Dispose() } catch {} }
}
finally {
  Stop-Server $p
}

Log "################ RESULT: pass=$script:Pass fail=$script:Fail ################"
if ($script:Fail -gt 0) { exit 1 }
