. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
$OUT = "$S\e2e10-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
function Write-Config10([string]$db, [string[]]$extra = @()) {
  $cfg = "$S\e2e10.toml"
  (@('[global]','server_name = "localhost"',('database_path = "' + ($db.Replace([string][char]92, '/')) + '"'),'port = 8015','address = ["127.0.0.1"]',
    'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
    'save_unredacted_events = true','redaction_retention_seconds = 1','media_gc_sweep_interval = 2','log = "info"') + $extra) -join "`n" | Set-Content -Path $cfg -Encoding ascii
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

# ================= Scenario 1: the maintainer's example (/docs/design/media/media-holders.md §2.3) =================
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

# 🚨 A per-room avatar (`m.room.member`'s `avatar_url`) is the one picture the reference scanner
# never reads — `MXC_CONTENT_PATHS` has `url` and the thumbnails, not `avatar_url`. Until
# `Holder::RoomAvatar` existed it therefore had **no holder at all**, and this is the sweep that took
# it away: no purge, no redaction, nobody doing anything
# (/docs/design/media/purge-release-set.md §3).
# ⭐ This is the direct test of that half. The PR first claimed it could not be tested because the
# seven-day floor cannot be lowered — wrong: the env var above returns before the floor is applied,
# and /docs/design/media/media-holders.md §5 says so in as many words (review of PR #107, cirno,
# whose point was that I had not read that file).
$roomAvatar = (Upload-Legacy $tokA $payload 'per-room-avatar.bin').mxc
$null = Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($rD))/state/m.room.member/$([uri]::EscapeDataString($regA.user_id))" (@{ membership = 'join'; avatar_url = $roomAvatar } | ConvertTo-Json -Compress) $tokA
$plainUnheld = (Upload-Legacy $tokA $payload 'nobody-holds-this.bin').mxc
Start-Sleep -Seconds 8
$stAvatar = Download-Status $roomAvatar $tokA
$stPlain = Download-Status $plainUnheld $tokA
Check '[3.3] a per-room avatar survives the sweep: its member event holds it' ($stAvatar -eq 200) "avatar=$stAvatar"
Check '[3.4] ... and an upload nobody holds, in the same sweep, is still taken (so [3.3] is not a sweep that stopped working)' ($stPlain -eq 410) "unheld=$stPlain"

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

# ================= Scenario 5: the traditional upload has a server-wide memory budget =================
# /docs/design/media/legacy-upload-concurrency.md. One request's size was bounded and how long it
# may take was bounded, but how many of them may be in memory together was not -- so the real
# ceiling was "people uploading at once x max_request_size", with no number to point at.
Log '################ Scenario 5: the upload memory budget ################'
$db5 = "$S\e2e10db-5"; Remove-Item -Recurse -Force $db5 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db5 | Out-Null
# 2 MiB for the whole server. max_request_size cannot go below 10 MB (config/check refuses to
# start), so the budget is deliberately the smaller of the two here -- which is exactly the
# configuration the startup warning is for, asserted in [5.0].
$cfg5 = Write-Config10 $db5 @('matrix_upload_memory_budget = "2 MiB"')
$script:p = Start-Server $cfg5 's5'
$regA5 = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokA5 = $regA5.access_token

$log5 = Read-ServerLog 's5'
Check '[5.0] a budget below max_request_size is warned about at startup, not refused' `
  ($log5 -match 'matrix_upload_memory_budget is 2097152 bytes but max_request_size is') ''

$small = [byte[]](1..1024 | ForEach-Object { $_ % 251 })
$firstUpload = Upload-Legacy $tokA5 $small 'within-budget.bin'
Check '[5.1] an ordinary upload is unaffected by the budget' `
  ($firstUpload.status -eq 200 -and $firstUpload.mxc) "status=$($firstUpload.status) mxc=$($firstUpload.mxc)"

# Sends an upload's request head (and one byte of body) and then stops. The budget is booked when
# the head has been read, before the body is -- so this holds the bytes without ever sending them.
# 🚨 Raw sockets because that is the only way to hold a reservation open and then vanish mid-body,
# which is the case the whole design rests on: a request can end in more ways than it can succeed.
function Start-Stalled-Upload($tok, [int]$declare, [string]$path = '/_matrix/media/v3/upload') {
  $client = New-Object System.Net.Sockets.TcpClient
  $client.Connect('127.0.0.1', 8015)
  $stream = $client.GetStream()
  $head = "POST $path`?filename=stalled HTTP/1.1`r`nHost: 127.0.0.1:8015`r`n" +
    "Authorization: Bearer $tok`r`nContent-Type: application/octet-stream`r`n" +
    "Content-Length: $declare`r`nConnection: close`r`n`r`n"
  $headBytes = [Text.Encoding]::ASCII.GetBytes($head)
  $stream.Write($headBytes, 0, $headBytes.Length); $stream.Flush()
  # One byte of the body, so the server is certainly past the head and waiting for the rest.
  $stream.Write([byte[]]@(65), 0, 1); $stream.Flush()
  Start-Sleep -Milliseconds 800
  @{ client = $client; stream = $stream }
}
function Stop-Stalled-Upload($stalled) {
  if ($null -eq $stalled) { return }
  $stalled.stream.Close(); $stalled.client.Close()
  Start-Sleep -Milliseconds 800
}

# 1.5 MiB of the 2 MiB budget is booked and never sent; 1 MiB more does not fit.
$stalled = Start-Stalled-Upload $tokA5 1572864
$req429 = New-Object System.Net.Http.HttpRequestMessage ([System.Net.Http.HttpMethod]::Post, "$B/_matrix/media/v3/upload?filename=does-not-fit.bin")
$req429.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $tokA5)
$req429.Content = New-Object System.Net.Http.ByteArrayContent (,([byte[]]::new(1048576)))
$req429.Content.Headers.ContentType = New-Object System.Net.Http.Headers.MediaTypeHeaderValue('application/octet-stream')
$resp429 = $script:PackHttpClient.SendAsync($req429).Result
$json429 = $resp429.Content.ReadAsStringAsync().Result | ConvertFrom-Json
Check '[5.2] with most of the budget held, the next upload is refused instead of being buffered' `
  ([int]$resp429.StatusCode -eq 429) "status=$([int]$resp429.StatusCode)"
# A limit with no retry hint turns a client into a hot loop.
Check '[5.2b] ... as M_LIMIT_EXCEEDED with retry_after_ms' `
  ($json429.errcode -eq 'M_LIMIT_EXCEEDED' -and $json429.retry_after_ms -gt 0) `
  "errcode=$($json429.errcode) retry_after_ms=$($json429.retry_after_ms)"

# 🚨 The gate must not reach every request: a server busy with uploads still has to let people in.
$versions = Http Get '/_matrix/client/versions' $null $null
$loginWhileFull = Api Post '/_matrix/client/v3/login' '{"type":"m.login.password","identifier":{"type":"m.id.user","user":"alice"},"password":"correct-horse-battery"}' $null
# The wbf chunked upload declares its file over the same HTTP endpoint and must not be gated: it
# only ever holds one chunk, which is why this budget is about the other path and not about it.
$chunkedWhileFull = Send-Pack (New-Pack 3 1 0 0 1 (FileInfo-Meta 131072 65536 2) @()) $tokA5
Check '[5.3] while the budget is held, requests that are not traditional uploads carry on' `
  ($versions.status -eq 200 -and $null -ne $loginWhileFull.access_token -and $chunkedWhileFull.subtype -eq 2) `
  "versions=$($versions.status) login=$($null -ne $loginWhileFull.access_token) chunked=$(Describe $chunkedWhileFull)"

# 📎 The reservation is given back by dropping it, so the client going away mid-body is the same
# path as success. Nothing in the server was told this request ended.
Stop-Stalled-Upload $stalled
$afterAbort = Upload-Legacy $tokA5 ([byte[]]::new(1048576)) 'after-abort.bin'
Check '[5.4] a client that vanishes mid-body gives its bytes back, so the next upload fits' `
  ($afterAbort.status -eq 200 -and $afterAbort.mxc) "status=$($afterAbort.status)"

# 🚫 Not 429: no amount of retrying makes a request smaller than it is.
$tooBig = Upload-Legacy $tokA5 ([byte[]]::new(3145728)) 'larger-than-the-budget.bin'
Check '[5.5] an upload larger than the whole budget is refused 413, not asked to retry' `
  ($tooBig.status -eq 413) "status=$($tooBig.status)"

# Both generations of the path are one Ruma endpoint, so they are one budget. Holding it from the
# r0 spelling has to refuse the v3 one.
$stalledOld = Start-Stalled-Upload $tokA5 1572864 '/_matrix/media/r0/upload'
$refusedNew = Upload-Legacy $tokA5 ([byte[]]::new(1048576)) 'other-generation.bin'
Stop-Stalled-Upload $stalledOld
Check '[5.6] the r0 and v3 spellings of upload share the one budget' `
  ($refusedNew.status -eq 429) "status=$($refusedNew.status)"

$afterRelease = Upload-Legacy $tokA5 ([byte[]]::new(1048576)) 'budget-is-back.bin'
Check '[5.7] and the budget is spendable again once nothing holds it' `
  ($afterRelease.status -eq 200) "status=$($afterRelease.status)"

# 🚨 An upload's body is a file, not a document: it must not be parsed as JSON on the way in.
# The parse grew a tree nothing bounded (a JSON-shaped file of a few MiB becomes hundreds), and
# a parsed *object* was re-serialised over the file -- so this deliberately ugly object is the
# one that used to come back with its whitespace gone and its keys re-ordered.
# /docs/design/media/legacy-upload-concurrency.md 2.4 (review 2026-10-14)
$jsonFile = [Text.Encoding]::UTF8.GetBytes('{  "z" : 1,   "a"  :  [2, 3] ,  "m":"  spaced  "  }')
$jsonUp = Upload-Legacy $tokA5 $jsonFile 'object.json'
$jsonBack = Get-Bytes $jsonUp.mxc $tokA5
$sameBytes = $jsonUp.status -eq 200 -and $jsonBack.status -eq 200 -and
  (@(Compare-Object $jsonFile $jsonBack.bytes -SyncWindow 0).Count -eq 0)
Check '[5.8] a JSON object uploaded as a file comes back byte for byte (it is never parsed)' `
  $sameBytes "up=$($jsonUp.status) down=$($jsonBack.status) sent=$($jsonFile.Length)B got=$($jsonBack.bytes.Length)B"
Stop-Server $script:p

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"

# A pending ReceiveAsync or an undisposed socket can keep this process alive long after the
# last line is written — every batch run this session looked like a hang for that reason, with
# the results already on disk. Leave on purpose, and say in the exit code whether it passed:
# a FAIL used to be invisible to anything that only looked at the exit status.
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
