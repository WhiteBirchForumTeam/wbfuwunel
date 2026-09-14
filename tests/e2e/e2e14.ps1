. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
# The server user: @system, displayname "[SYS] <server_name>", and the startup gate that refuses an @system account
# this server did not create (docs/design/server-user.md).
# Scenario 2 needs a binary from before the rename (its server user is @conduit, so "system" is a free username
# there); point E2E_OLD_EXE at one, or the scenario is skipped.
$OLDEXE = $env:E2E_OLD_EXE
$OUT = "$S\e2e14-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
function Enc([string]$text) { [uri]::EscapeDataString($text) }
function Register([string]$name) { Http POST '/_matrix/client/v3/register' @{ username = $name; password = 'correct-horse-battery'; auth = @{ type = 'm.login.dummy' } } $null }

$SYSTEM = '@system:localhost'
$DISPLAYNAME = '[SYS] localhost'

Log '################ Scenario 0: a fresh server names its own account @system and shows it as [SYS] localhost ################'
$db0 = "$S\e2e14-db0"; Remove-Item -Recurse -Force $db0 -ErrorAction SilentlyContinue
$cfg0 = Write-Config $db0 60
$server = Start-Server $cfg0 's0'

$alice = Register 'alice'
$tok = $alice.json.access_token

$profile = Http GET "/_matrix/client/v3/profile/$(Enc $SYSTEM)/displayname" $null $tok
Check '[0.1] the server user is @system and its profile displayname is [SYS] localhost' `
  ($profile.status -eq 200 -and $profile.json.displayname -eq $DISPLAYNAME) "status=$($profile.status) $($profile.text)"

$conduit = Http GET "/_matrix/client/v3/profile/$(Enc '@conduit:localhost')" $null $tok
Check '[0.2] there is no @conduit account' ($conduit.status -eq 404) "status=$($conduit.status) $($conduit.text)"

$taken = Register 'system'
Check '[0.3] nobody can register "system": the name is taken from the first moment' `
  ($taken.status -eq 400 -and $taken.json.errcode -eq 'M_USER_IN_USE') "status=$($taken.status) $($taken.text)"

# The first user is made an admin: invited to the admin room by the server user.
$alias = Http GET "/_matrix/client/v3/directory/room/$(Enc '#admins:localhost')" $null $tok
$adminRoom = $alias.json.room_id
$join = Http POST "/_matrix/client/v3/join/$(Enc $adminRoom)" @{} $tok
$member = Http GET "/_matrix/client/v3/rooms/$(Enc $adminRoom)/state/m.room.member/$(Enc $SYSTEM)" $null $tok
Check '[0.4] in the admin room, the server user''s own join event carries the displayname a client shows' `
  ($alias.status -eq 200 -and $join.status -eq 200 -and $member.status -eq 200 -and $member.json.membership -eq 'join' -and $member.json.displayname -eq $DISPLAYNAME) `
  "alias=$($alias.status) join=$($join.status) member=$($member.status) $($member.text)"

Stop-Server $server

Log '################ Scenario 1: restarting on the same database passes the gate ################'
$server = Start-Server $cfg0 's1'
$login = Http POST '/_matrix/client/v3/login' @{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = 'alice' }; password = 'correct-horse-battery' } $null
$again = Http GET "/_matrix/client/v3/profile/$(Enc $SYSTEM)/displayname" $null $login.json.access_token
Check '[1.1] the server user this server created is recognized on the next start, displayname unchanged' `
  ($login.status -eq 200 -and $again.status -eq 200 -and $again.json.displayname -eq $DISPLAYNAME) "login=$($login.status) profile=$($again.status) $($again.text)"
Stop-Server $server

Log '################ Scenario 2: an @system account this server did not create stops the server from starting ################'
if (-not $OLDEXE -or -not (Test-Path $OLDEXE)) { Log '################ Scenario 2 skipped: set E2E_OLD_EXE to a binary from before the rename ################' }
else {
  $db2 = "$S\e2e14-db2"; Remove-Item -Recurse -Force $db2 -ErrorAction SilentlyContinue
  $cfg2 = Write-Config $db2 60
  $saveExe = $EXE; $EXE = $OLDEXE
  $server = Start-Server $cfg2 's2-old'
  $human = Register 'system'
  Check '[2.1] on the old binary (server user @conduit), a person registers "system"' `
    ($human.status -eq 200 -and $human.json.user_id -eq $SYSTEM) "status=$($human.status) $($human.text)"
  Stop-Server $server
  $EXE = $saveExe

  $refused = $false
  try { $server = Start-Server $cfg2 's2-new' } catch { $refused = $true; Log "  (start refused as expected: $($_.Exception.Message))" }
  Stop-Server $server
  $text = Read-ServerLog 's2-new'
  Check '[2.2] the new binary refuses to start: that @system would be made an admin' `
    ($refused -and $text.Contains('is an account this server did not create')) `
    "refused=$refused log=$((($text -split "`n") | Where-Object { $_ -match 'system' } | Select-Object -First 3) -join ' | ')"
}

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
