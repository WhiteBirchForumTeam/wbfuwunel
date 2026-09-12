. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
$OUT = "$S\e2e10-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
function Write-Config10([string]$db) {
  $cfg = "$S\e2e10.toml"
  @('[global]','server_name = "localhost"',('database_path = "' + ($db.Replace([string][char]92, '/')) + '"'),'port = 8015','address = ["127.0.0.1"]',
    'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
    'save_unredacted_events = true','redaction_retention_seconds = 1','media_gc_sweep_interval = 2','log = "info"') -join "`n" | Set-Content -Path $cfg -Encoding ascii
  $cfg
}
function Room-Messages($room, $tok, $dir = 'b', $limit = 100) {
  Api Get "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/messages?dir=$dir&limit=$limit" $null $tok
}
function Upload-Legacy($tok, [byte[]]$bytes, $name) {
  $req = New-Object System.Net.Http.HttpRequestMessage ([System.Net.Http.HttpMethod]::Post, "$B/_matrix/media/v3/upload?filename=$name")
  $req.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $tok)
  $req.Content = New-Object System.Net.Http.ByteArrayContent (,$bytes)
  $req.Content.Headers.ContentType = New-Object System.Net.Http.Headers.MediaTypeHeaderValue('application/octet-stream')
  $resp = $script:PackHttpClient.SendAsync($req).Result
  $json = $resp.Content.ReadAsStringAsync().Result | ConvertFrom-Json
  @{ status = [int]$resp.StatusCode; mxc = $json.content_uri }
}
function Send-Image($room, $mxc, $body, $tok) {
  $txn = [guid]::NewGuid().ToString('N')
  (Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/send/m.room.message/$txn" (@{ msgtype = 'm.image'; body = $body; url = $mxc } | ConvertTo-Json -Compress) $tok).event_id
}
function Send-Text($room, $body, $tok) {
  $txn = [guid]::NewGuid().ToString('N')
  (Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/send/m.room.message/$txn" (@{ msgtype = 'm.text'; body = $body } | ConvertTo-Json -Compress) $tok).event_id
}
function Redact($room, $eid, $tok) { Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/redact/$([uri]::EscapeDataString($eid))/$([guid]::NewGuid().ToString('N'))" '{"reason":"e2e"}' $tok }
function Download-Status($mxc, $tok) { Start-Sleep -Milliseconds 800; (Get-Bytes $mxc $tok).status }
function Create-Room($tok, $name) { (Api Post '/_matrix/client/v3/createRoom' (@{ preset = 'private_chat'; name = $name } | ConvertTo-Json -Compress) $tok).room_id }
function Set-Avatar($user, $mxc, $tok) { Api Put "/_matrix/client/v3/profile/$([uri]::EscapeDataString($user))/avatar_url" (@{ avatar_url = $mxc } | ConvertTo-Json -Compress) $tok }
function Delete-Room($room, $tok) {
  $req = New-Object System.Net.Http.HttpRequestMessage ([System.Net.Http.HttpMethod]::Delete, "$B/_synapse/admin/v1/rooms/$([uri]::EscapeDataString($room))")
  $req.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $tok)
  $req.Content = New-Object System.Net.Http.StringContent ('{"purge":true}', [Text.Encoding]::UTF8, 'application/json')
  $resp = $script:PackHttpClient.SendAsync($req).Result
  @{ status = [int]$resp.StatusCode; text = $resp.Content.ReadAsStringAsync().Result }
}
function Purge-Before($room, $eid, $tok) {
  $r = Api Post "/_synapse/admin/v1/purge_history/$([uri]::EscapeDataString($room))" (@{ purge_up_to_event_id = $eid; delete_local_events = $true } | ConvertTo-Json -Compress) $tok
  Start-Sleep -Seconds 3
  $r
}
function Restart-Server([string]$cfg, [string]$tag) { Stop-Server $script:p; $script:p = Start-Server $cfg $tag; Start-Sleep -Seconds 3 }
function Login-Alice() { (Api Post '/_matrix/client/v3/login' '{"type":"m.login.password","identifier":{"type":"m.id.user","user":"alice"},"password":"correct-horse-battery"}' $null).access_token }
$payload = [byte[]](1..700 | ForEach-Object { $_ % 251 })

# ================= Scenario 1: the maintainer's example (media-holders.md §2.3) =================
Log '################ Scenario 1: holders come and go; media lives while any remains ################'
$db1 = "$S\e2e10db-1"; Remove-Item -Recurse -Force $db1 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db1 | Out-Null
$cfg = Write-Config10 $db1
$script:p = Start-Server $cfg 's1a'
$regA = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokA = $regA.access_token
$rA = Create-Room $tokA 'room a'
$rB = Create-Room $tokA 'room b'
$M = (Upload-Legacy $tokA $payload 'm.bin').mxc
$a1 = Send-Image $rA $M 'a1' $tokA; $a2 = Send-Image $rA $M 'a2' $tokA; $a3 = Send-Image $rA $M 'a3' $tokA
$b1 = Send-Image $rB $M 'b1' $tokA; $b2 = Send-Image $rB $M 'b2' $tokA
Check '[1.0] five events in two rooms hold M: served' ((Download-Status $M $tokA) -eq 200) ''

$del = Delete-Room $rA $tokA
Check '[1.1] delete room a: M still held by room b (200)' ($del.status -eq 200 -and (Download-Status $M $tokA) -eq 200) "delete=$($del.status)"

$null = Redact $rB $b1 $tokA
Check '[1.2] redact b1 with the original retained: still held (200)' ((Download-Status $M $tokA) -eq 200) ''

Restart-Server $cfg 's1b'
$tokA = Login-Alice
Check '[1.3] retention reaped b1 original: still held by b2 (200)' ((Download-Status $M $tokA) -eq 200) ''

$null = Set-Avatar $regA.user_id $M $tokA
$null = Redact $rB $b2 $tokA
Restart-Server $cfg 's1c'
$tokA = Login-Alice
Check '[1.4] b2 redacted and reaped; the avatar alone holds M (200)' ((Download-Status $M $tokA) -eq 200) ''

$null = Set-Avatar $regA.user_id '' $tokA
Check '[1.5] avatar cleared: nothing holds M -> 410' ((Download-Status $M $tokA) -eq 410) ''

# idempotency: doing the removals again changes nothing and logs no error
$null = Redact $rB $b2 $tokA
$again = Delete-Room $rA $tokA
$log = (Get-Content "$OUT\s1a.out","$OUT\s1b.out","$OUT\s1c.out" -Raw) -replace "`e\[[0-9;]*m", ''
Check '[1.6] repeating a redaction and a room deletion is harmless; no media errors logged' (-not ($log -match 'ERROR.*media')) "delete_again=$($again.status)"

# a fresh upload nothing holds survives the sweep (protection period), a held one too
$F = (Upload-Legacy $tokA $payload 'fresh.bin').mxc
$H = (Upload-Legacy $tokA $payload 'held.bin').mxc
$null = Send-Image $rB $H 'held' $tokA
Start-Sleep -Seconds 5
Check '[1.7] sweep leaves a fresh unheld upload (protected) and a held one alone' (((Download-Status $F $tokA) -eq 200) -and ((Download-Status $H $tokA) -eq 200)) ''
Stop-Server $script:p

# ================= Scenario 2: purge_history takes a range of holders, from the index =================
Log '################ Scenario 2: purge_history range ################'
$db2 = "$S\e2e10db-2"; Remove-Item -Recurse -Force $db2 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db2 | Out-Null
$cfg2 = "$S\e2e10-2.toml"
@('[global]','server_name = "localhost"',('database_path = "' + ($db2.Replace([string][char]92, '/')) + '"'),'port = 8015','address = ["127.0.0.1"]',
  'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
  'save_unredacted_events = true','log = "info"') -join "`n" | Set-Content -Path $cfg2 -Encoding ascii
$script:p = Start-Server $cfg2 's2'
$regA = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokA = $regA.access_token
$rC = Create-Room $tokA 'room c'
$M2 = (Upload-Legacy $tokA $payload 'm2.bin').mxc
$M3 = (Upload-Legacy $tokA $payload 'm3.bin').mxc
$c1 = Send-Image $rC $M2 'c1' $tokA
$c2 = Send-Image $rC $M2 'c2' $tokA
$null = Redact $rC $c1 $tokA           # c1 -> Backup holder (original retained)
$c3 = Send-Text $rC 'marker' $tokA
$c4 = Send-Image $rC $M3 'c4' $tokA    # after the marker: outside the purge range
$purge = Purge-Before $rC $c3 $tokA
$st2 = Download-Status $M2 $tokA; $st3 = Download-Status $M3 $tokA
Check '[2.1] purge before the marker removes Event c2 and Backup c1: M2 gone (410), M3 outside the range stays (200)' ($purge.purge_id -and $st2 -eq 410 -and $st3 -eq 200) "m2=$st2 m3=$st3"
$purge2 = Purge-Before $rC $c3 $tokA
$log2 = (Get-Content "$OUT\s2.out" -Raw) -replace "`e\[[0-9;]*m", ''
$st3b = Download-Status $M3 $tokA
$errs = @([regex]::Matches($log2, '(?m)^.*ERROR.*$') | ForEach-Object { $_.Value })
Check '[2.2] purging the same range again is a no-op without errors' ($purge2.purge_id -and $errs.Count -eq 0 -and $st3b -eq 200) "purge_id=$($purge2.purge_id) m3=$st3b errors=$($errs.Count) first=$(if ($errs.Count) { $errs[0].Substring(0, [Math]::Min(160, $errs[0].Length)) })"
Stop-Server $script:p


# ================= Scenario 3: the sweep, with the protection period overridden by the env var =================
Log '################ Scenario 3: sweep with WBFUWUNEL_MEDIA_GRACE_SECONDS=3 ################'
$db3 = "$S\e2e10db-3"; Remove-Item -Recurse -Force $db3 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db3 | Out-Null
$cfg3 = "$S\e2e10-3.toml"
@('[global]','server_name = "localhost"',('database_path = "' + ($db3.Replace([string][char]92, '/')) + '"'),'port = 8015','address = ["127.0.0.1"]',
  'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
  'media_gc_sweep_interval = 2','log = "info"') -join "`n" | Set-Content -Path $cfg3 -Encoding ascii
$env:WBFUWUNEL_MEDIA_GRACE_SECONDS = '3'
$script:p = Start-Server $cfg3 's3'
$regA = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokA = $regA.access_token
$rD = Create-Room $tokA 'room d'
$U = (Upload-Legacy $tokA $payload 'unheld.bin').mxc
$Hd = (Upload-Legacy $tokA $payload 'held.bin').mxc
$null = Send-Image $rD $Hd 'held' $tokA
Check '[3.0] both served right after upload' (((Download-Status $U $tokA) -eq 200) -and ((Download-Status $Hd $tokA) -eq 200)) ''
Start-Sleep -Seconds 8
$stu = Download-Status $U $tokA; $sth = Download-Status $Hd $tokA
Check '[3.1] after the 3 s grace the unheld upload is swept (410); the held one stays (200)' ($stu -eq 410 -and $sth -eq 200) "unheld=$stu held=$sth"
$log3 = (Get-Content "$OUT\s3.out" -Raw) -replace "`e\[[0-9;]*m", ''
Check '[3.2] startup warned about the override and the sweep logged a removal' (($log3 -match 'WBFUWUNEL_MEDIA_GRACE_SECONDS') -and ($log3 -match 'Unreferenced media sweep finished')) ''
Stop-Server $script:p
Remove-Item Env:WBFUWUNEL_MEDIA_GRACE_SECONDS -ErrorAction SilentlyContinue

# ================= Scenario 4: media from before the holder model stays unmanaged when a thumbnail is made for it =================
# Needs a binary from before mxc_managed existed (PR #24); point E2E_OLD_EXE at one, or the scenario is skipped.
# The bug this guards: create_file_metadata used to write mxc_managed for any row, so the first thumbnail of a
# pre-model image made it "managed with no holders" and the sweep took it after the protection period.
$OLDEXE = $env:E2E_OLD_EXE
if (-not $OLDEXE -or -not (Test-Path $OLDEXE)) { Log '################ Scenario 4 skipped: set E2E_OLD_EXE to a pre-holder-model binary ################' }
else {
Log '################ Scenario 4: thumbnail of pre-model media does not hand it to the sweep ################'
function Upload-Png($tok, [byte[]]$bytes, $name) {
  $req = New-Object System.Net.Http.HttpRequestMessage ([System.Net.Http.HttpMethod]::Post, "$B/_matrix/media/v3/upload?filename=$name")
  $req.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $tok)
  $req.Content = New-Object System.Net.Http.ByteArrayContent (,$bytes)
  $req.Content.Headers.ContentType = New-Object System.Net.Http.Headers.MediaTypeHeaderValue('image/png')
  $resp = $script:PackHttpClient.SendAsync($req).Result
  $json = $resp.Content.ReadAsStringAsync().Result | ConvertFrom-Json
  @{ status = [int]$resp.StatusCode; mxc = $json.content_uri }
}
function Get-Thumbnail($mxc, $tok) {
  $id = $mxc -replace '^mxc://localhost/', ''
  $req = New-Object System.Net.Http.HttpRequestMessage ([System.Net.Http.HttpMethod]::Get, "$B/_matrix/client/v1/media/thumbnail/localhost/${id}?width=32&height=32&method=scale")
  $req.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $tok)
  $resp = $script:PackHttpClient.SendAsync($req).Result; @{ status = [int]$resp.StatusCode; bytes = $resp.Content.ReadAsByteArrayAsync().Result }
}
Add-Type -AssemblyName System.Drawing
$bmp = New-Object System.Drawing.Bitmap 64, 64
$gfx = [System.Drawing.Graphics]::FromImage($bmp); $gfx.Clear([System.Drawing.Color]::Red); $gfx.Dispose()
$ms = New-Object IO.MemoryStream; $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); $png = $ms.ToArray(); $bmp.Dispose()

$db4 = "$S\e2e10db-4"; Remove-Item -Recurse -Force $db4 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db4 | Out-Null
$cfg4 = "$S\e2e10-4.toml"
@('[global]','server_name = "localhost"',('database_path = "' + ($db4.Replace([string][char]92, '/')) + '"'),'port = 8015','address = ["127.0.0.1"]',
  'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
  'media_gc_sweep_interval = 2','log = "info"') -join "`n" | Set-Content -Path $cfg4 -Encoding ascii

# old binary: an image referenced by a message, and one nobody references; neither gets an mxc_managed row
$saveExe = $EXE; $EXE = $OLDEXE
$script:p = Start-Server $cfg4 's4-old'
$regO = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokO = $regO.access_token
$rL = Create-Room $tokO 'legacy room'
$Lheld = (Upload-Png $tokO $png 'held.png').mxc
$Lfree = (Upload-Png $tokO $png 'free.png').mxc
$null = Send-Image $rL $Lheld 'legacy image' $tokO
Check '[4.0] old binary serves both uploads' (((Download-Status $Lheld $tokO) -eq 200) -and ((Download-Status $Lfree $tokO) -eq 200)) ''
Stop-Server $script:p
$EXE = $saveExe

# new binary, protection period 3 s: thumbnails are made for both, a fresh unheld upload is the control that the sweep runs
$env:WBFUWUNEL_MEDIA_GRACE_SECONDS = '3'
$script:p = Start-Server $cfg4 's4-new'
$tokN = Login-Alice
$tHeld = Get-Thumbnail $Lheld $tokN; $tFree = Get-Thumbnail $Lfree $tokN
$isPng = ($tHeld.bytes.Length -gt 8 -and $tHeld.bytes[1] -eq 0x50 -and $tHeld.bytes[2] -eq 0x4E -and $tHeld.bytes[3] -eq 0x47)
Check '[4.1] thumbnails generated for the pre-model images (200, PNG, smaller than the original)' ($tHeld.status -eq 200 -and $tFree.status -eq 200 -and $isPng -and $tHeld.bytes.Length -lt $png.Length) "held=$($tHeld.status)/$($tHeld.bytes.Length)B free=$($tFree.status)/$($tFree.bytes.Length)B original=$($png.Length)B"
$Nfree = (Upload-Legacy $tokN $payload 'new-unheld.bin').mxc
Start-Sleep -Seconds 8
$sHeld = Download-Status $Lheld $tokN; $sFree = Download-Status $Lfree $tokN; $sNew = Download-Status $Nfree $tokN
$sThumb = (Get-Thumbnail $Lfree $tokN).status
Check '[4.2] after the grace: pre-model images stay (200), even the unreferenced one and its thumbnail; the new unheld upload is swept (410)' ($sHeld -eq 200 -and $sFree -eq 200 -and $sThumb -eq 200 -and $sNew -eq 410) "held=$sHeld free=$sFree thumb=$sThumb new=$sNew"
$log4 = (Get-Content "$OUT\s4-new.out" -Raw) -replace "`e\[[0-9;]*m", ''
Check '[4.3] the sweep ran' ($log4 -match 'Unreferenced media sweep finished') ''
Stop-Server $script:p
Remove-Item Env:WBFUWUNEL_MEDIA_GRACE_SECONDS -ErrorAction SilentlyContinue
}

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"

# A pending ReceiveAsync or an undisposed socket can keep this process alive long after the
# last line is written — every batch run this session looked like a hang for that reason, with
# the results already on disk. Leave on purpose, and say in the exit code whether it passed:
# a FAIL used to be invisible to anything that only looked at the exit status.
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
