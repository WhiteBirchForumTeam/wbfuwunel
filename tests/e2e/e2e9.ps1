. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
$OUT = "$S\e2e9-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
function Write-Config9([string]$db, [int]$sweep) {
  $cfg = "$S\e2e9.toml"
  @('[global]','server_name = "localhost"',('database_path = "' + ($db -replace '\\','/') + '"'),'port = 8015','address = ["127.0.0.1"]',
    'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
    'save_unredacted_events = false',('media_gc_sweep_interval = ' + $sweep),'log = "info"') -join "`n" | Set-Content -Path $cfg -Encoding ascii
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
  $resp = $script:Http.SendAsync($req).Result
  $json = $resp.Content.ReadAsStringAsync().Result | ConvertFrom-Json
  @{ status = [int]$resp.StatusCode; mxc = $json.content_uri }
}
function Send-Raw($room, $type, $body, $tok, $attachments) {
  $txn = [guid]::NewGuid().ToString('N')
  $req = New-Object System.Net.Http.HttpRequestMessage ([System.Net.Http.HttpMethod]::Put, "$B/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/send/$type/$txn")
  $req.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $tok)
  if ($attachments) { $req.Headers.TryAddWithoutValidation('X-Wbf-Attachments', ($attachments -join ',')) | Out-Null }
  $req.Content = New-Object System.Net.Http.StringContent (($body | ConvertTo-Json -Compress -Depth 5), [Text.Encoding]::UTF8, 'application/json')
  $resp = $script:Http.SendAsync($req).Result
  $text = $resp.Content.ReadAsStringAsync().Result
  $json = $null; try { $json = $text | ConvertFrom-Json } catch {}
  @{ status = [int]$resp.StatusCode; event_id = $json.event_id; errcode = $json.errcode; error = $json.error; text = $text }
}
function Encrypted-Body($tag) { @{ algorithm = 'm.megolm.v1.aes-sha2'; ciphertext = "AwgAEnACgAkLmt6qF84IK$tag"; device_id = 'E2E9DEV'; sender_key = 'IlRMeOPX2e0MurIyfWEucYBRVOEEUMrOHqn'; session_id = "sess$tag" } }
function Redact($room, $eid, $tok) { Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($room))/redact/$([uri]::EscapeDataString($eid))/$([guid]::NewGuid().ToString('N'))" '{"reason":"e2e"}' $tok }
function Download-Status($mxc, $tok) { Start-Sleep -Milliseconds 800; (Get-Bytes $mxc $tok).status }
function Create-Room($tok, [bool]$encrypted, $name) {
  $body = @{ preset = 'private_chat'; name = $name }
  if ($encrypted) { $body.initial_state = @(@{ type = 'm.room.encryption'; state_key = ''; content = @{ algorithm = 'm.megolm.v1.aes-sha2' } }) }
  (Api Post '/_matrix/client/v3/createRoom' ($body | ConvertTo-Json -Compress -Depth 6) $tok).room_id
}
$payload = [byte[]](1..600 | ForEach-Object { $_ % 251 })

# ================= Scenario 1: attachments declared with the send =================
$db1 = "$S\e2e9db-1"; Remove-Item -Recurse -Force $db1 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db1 | Out-Null
$cfg = Write-Config9 $db1 2
Log '################ Scenario 1: declared attachments, redaction, sweep, warning ################'
$p = Start-Server $cfg 's1'
$regA = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$regB = Api Post '/_matrix/client/v3/register' '{"username":"bob","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokA = $regA.access_token; $tokB = $regB.access_token
$rE = Create-Room $tokA $true 'encrypted'
$null = Api Post "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($rE))/invite" (@{ user_id = $regB.user_id } | ConvertTo-Json -Compress) $tokA
$null = Api Post "/_matrix/client/v3/join/$([uri]::EscapeDataString($rE))" '{}' $tokB
$rP = Create-Room $tokA $false 'plain'
Log "rooms encrypted=$rE plain=$rP"

# [1.0] the server says who it is
$versions = Api Get '/_matrix/client/versions' $null $null
$srv = $versions.'net.zemos.msc4383.server'
Check '[1.0] /versions names the fork' ($srv.name -eq 'wbfuwunel' -and $versions.unstable_features.'org.wbftw.wbfuwunel' -eq $true) "name=$($srv.name) flag=$($versions.unstable_features.'org.wbftw.wbfuwunel')"
$ws = Ws-Open $tokA
$hello = Ws-Call $ws (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e9'; features = @() } $null)
Check '[1.0b] Hello: engine=wbfuwunel, features has attachments' ($hello.meta.engine -eq 'wbfuwunel' -and (@($hello.meta.features) -contains 'attachments')) "engine=$($hello.meta.engine) features=$(@($hello.meta.features) -join ',')"

# [1.1] encrypted send with a declared attachment, then redaction removes the media
$upA = Upload-Legacy $tokA $payload 'a.bin'
$sendA = Send-Raw $rE 'm.room.encrypted' (Encrypted-Body 'A') $tokA @($upA.mxc)
Check '[1.1] encrypted send with X-Wbf-Attachments accepted' ($sendA.status -eq 200 -and $sendA.event_id) "status=$($sendA.status) $($sendA.text.Substring(0, [Math]::Min(80, $sendA.text.Length)))"
Check '[1.1b] media served while referenced' ((Download-Status $upA.mxc $tokA) -eq 200) ''
$null = Redact $rE $sendA.event_id $tokA
$after = Download-Status $upA.mxc $tokA
Check '[1.1c] redaction of the declaring event removes the media (410)' ($after -eq 410) "status=$after"

# [1.2] refused declarations refuse the whole send
$upB = Upload-Legacy $tokA $payload 'b.bin'
$bad1 = Send-Raw $rE 'm.room.encrypted' (Encrypted-Body 'B1') $tokB @($upB.mxc)
Check '[1.2] declaring someone else''s upload -> 400, names the mxc' ($bad1.status -eq 400 -and $bad1.error -like "*$($upB.mxc)*") "status=$($bad1.status) err=$($bad1.error)"
$bad2 = Send-Raw $rE 'm.room.encrypted' (Encrypted-Body 'B2') $tokA @('mxc://localhost/doesnotexist')
Check '[1.2b] declaring unknown media -> 400' ($bad2.status -eq 400 -and $bad2.error -like '*doesnotexist*') "status=$($bad2.status) err=$($bad2.error)"
$bad3 = Send-Raw $rE 'm.room.encrypted' (Encrypted-Body 'B3') $tokA @('not-an-mxc')
Check '[1.2c] declaring a non-mxc -> 400' ($bad3.status -eq 400) "status=$($bad3.status) err=$($bad3.error)"
$bad4 = Send-Raw $rE 'm.room.encrypted' (Encrypted-Body 'B4') $tokA @('mxc://elsewhere.example/abc')
Check '[1.2d] declaring remote media -> 400' ($bad4.status -eq 400) "status=$($bad4.status) err=$($bad4.error)"
$leaked = @((Room-Messages $rE $tokA 'b' 50).chunk | Where-Object { $_.type -eq 'm.room.encrypted' -and $_.content.ciphertext -like '*B?' })
Check '[1.2e] refused sends wrote no event' ($leaked.Count -eq 0) "leaked=$($leaked.Count)"

# [1.3] two events declare one media: the second redaction frees it
$sendB1 = Send-Raw $rE 'm.room.encrypted' (Encrypted-Body 'C1') $tokA @($upB.mxc)
$sendB2 = Send-Raw $rE 'm.room.encrypted' (Encrypted-Body 'C2') $tokA @($upB.mxc)
$null = Redact $rE $sendB1.event_id $tokA
$mid = Download-Status $upB.mxc $tokA
$null = Redact $rE $sendB2.event_id $tokA
$end = Download-Status $upB.mxc $tokA
Check '[1.3] shared attachment survives the first redaction, goes with the second' ($mid -eq 200 -and $end -eq 410) "after1=$mid after2=$end"

# [1.4] plaintext room: no declaration needed, content is read
$upC = Upload-Legacy $tokA $payload 'c.bin'
$sendC = Send-Raw $rP 'm.room.message' @{ msgtype = 'm.image'; body = 'c.bin'; url = $upC.mxc; info = @{ mimetype = 'application/octet-stream' } } $tokA $null
$null = Redact $rP $sendC.event_id $tokA
Check '[1.4] plaintext m.image without header still counted and released' ($sendC.status -eq 200 -and (Download-Status $upC.mxc $tokA) -eq 410) "status=$($sendC.status)"

# [1.5] Event/Send over the wbf channel
$upD = Upload-Legacy $tokA $payload 'd.bin'
$sendMeta = @{ room_id = $rE; type = 'm.room.encrypted'; txn_id = [guid]::NewGuid().ToString('N'); attachments = @($upD.mxc) }
$content = [Text.Encoding]::UTF8.GetBytes(((Encrypted-Body 'D') | ConvertTo-Json -Compress))
$ackD = Ws-Call $ws (Json-Pack 0x14 2 0 2 $sendMeta $content)
Check '[1.5] Event/Send -> Ack with event_id' ($ackD.subtype -eq 2 -and $ackD.meta.event_id) (Describe $ackD)
$evD = Api Get "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($rE))/event/$([uri]::EscapeDataString($ackD.meta.event_id))" $null $tokA
Check '[1.5b] the event is in the room with the same content' ($evD.type -eq 'm.room.encrypted' -and $evD.content.session_id -eq 'sessD') "type=$($evD.type)"
$null = Redact $rE $ackD.meta.event_id $tokA
Check '[1.5c] redacting the pack-sent event frees the declared media' ((Download-Status $upD.mxc $tokA) -eq 410) ''
$sendMeta2 = @{ room_id = $rE; type = 'm.room.encrypted'; txn_id = [guid]::NewGuid().ToString('N'); attachments = @($upD.mxc) }
$rej = Ws-Call $ws (Json-Pack 0x14 2 0 3 $sendMeta2 $content)
Check '[1.5d] Event/Send declaring removed media -> Error InvalidRequest' ($rej.subtype -eq 3 -and $rej.meta.code -eq 'InvalidRequest') (Describe $rej)
$sendMeta3 = @{ room_id = $rE; type = 'm.room.encrypted'; txn_id = 'dup-txn'; attachments = @() }
$first = Ws-Call $ws (Json-Pack 0x14 2 0 4 $sendMeta3 $content)
$second = Ws-Call $ws (Json-Pack 0x14 2 0 5 $sendMeta3 $content)
Check '[1.5e] same txn_id twice -> same event_id' ($first.meta.event_id -and $first.meta.event_id -eq $second.meta.event_id) "first=$($first.meta.event_id) second=$($second.meta.event_id)"

# [1.6] a legacy client in an encrypted room: uploaded, sent encrypted without declaring -> one notice
$upE = Upload-Legacy $tokB $payload 'e.bin'
$undeclared1 = Send-Raw $rE 'm.room.encrypted' (Encrypted-Body 'E1') $tokB $null
Start-Sleep -Seconds 2
$syncB = Api Get '/_matrix/client/v3/sync?timeout=0' $null $tokB
$fromServer = @()
if ($syncB.rooms.invite) { foreach ($prop in $syncB.rooms.invite.PSObject.Properties) { $ev = @($prop.Value.invite_state.events | Where-Object { $_.type -eq 'm.room.member' -and $_.sender -eq '@conduit:localhost' -and $_.state_key -eq $regB.user_id }); if ($ev.Count -gt 0) { $fromServer += $prop.Name } } }
Check '[1.6] undeclared encrypted send after a legacy upload -> one DM invite from the server user' ($undeclared1.status -eq 200 -and $fromServer.Count -eq 1) "invites_from_server=$($fromServer.Count)"
$inviteRoom = $fromServer[0]
$null = Api Post "/_matrix/client/v3/join/$([uri]::EscapeDataString($inviteRoom))" '{}' $tokB
$notice = (Room-Messages $inviteRoom $tokB 'b' 20).chunk | Where-Object { $_.type -eq 'm.room.message' } | Select-Object -First 1
Check '[1.6b] the DM holds the English warning' ($notice.content.body -like 'This server keeps an uploaded file*') "body=$($notice.content.body.Substring(0, [Math]::Min(60, [string]$notice.content.body.Length)))"
$undeclared2 = Send-Raw $rE 'm.room.encrypted' (Encrypted-Body 'E2') $tokB $null
Start-Sleep -Seconds 2
$syncB2 = Api Get '/_matrix/client/v3/sync?timeout=0' $null $tokB
$invites2 = 0; if ($syncB2.rooms.invite) { $invites2 = @($syncB2.rooms.invite.PSObject.Properties).Count }
Check '[1.6c] a second undeclared send does not warn again' ($undeclared2.status -eq 200 -and $invites2 -eq 0) "pending_invites=$invites2"

# [1.7] the sweep: the protection clock is the stored creation time (not the file mtime), so a
# fresh upload nothing holds must survive the sweep; a held one too. Removal after seven
# days cannot be exercised live (the floor is hard); the decision is unit-tested.
$upF = Upload-Legacy $tokA $payload 'f.bin'
$upG = Upload-Legacy $tokA $payload 'g.bin'
$null = Send-Raw $rE 'm.room.encrypted' (Encrypted-Body 'G') $tokA @($upG.mxc)
Start-Sleep -Seconds 5
$fStatus = Download-Status $upF.mxc $tokA; $gStatus = Download-Status $upG.mxc $tokA
Check '[1.7] sweep leaves a fresh unheld upload (200) and a held one (200) alone' ($fStatus -eq 200 -and $gStatus -eq 200) "fresh=$fStatus held=$gStatus"
Check '[1.7b] media referenced by a live event is untouched by the sweep' ((Download-Status $upE.mxc $tokB) -eq 200) ''

$ws.Dispose()
Stop-Server $p

# ================= Scenario 2: a retained original holds the references; purge releases once =================
Log '################ Scenario 2: redact with retained original, then purge ################'
$db2 = "$S\e2e9db-2"; Remove-Item -Recurse -Force $db2 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db2 | Out-Null
$cfg2 = "$S\e2e9-2.toml"
@('[global]','server_name = "localhost"',('database_path = "' + ($db2.Replace([string][char]92, '/')) + '"'),'port = 8015','address = ["127.0.0.1"]',
  'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
  'save_unredacted_events = true','log = "info"') -join "`n" | Set-Content -Path $cfg2 -Encoding ascii
$p = Start-Server $cfg2 's2'
$regA = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokA = $regA.access_token
$rP = Create-Room $tokA $false 'plain-retained'
$upA = Upload-Legacy $tokA $payload 'shared.bin'
$e1 = Send-Raw $rP 'm.room.message' @{ msgtype = 'm.image'; body = 'one'; url = $upA.mxc } $tokA $null
$e2 = Send-Raw $rP 'm.room.message' @{ msgtype = 'm.image'; body = 'two'; url = $upA.mxc } $tokA $null
$null = Redact $rP $e1.event_id $tokA
Check '[2.1] redacted with the original retained: media still served' ((Download-Status $upA.mxc $tokA) -eq 200) ''
function Purge-Before($room, $eid, $tok) {
  $r = Api Post "/_synapse/admin/v1/purge_history/$([uri]::EscapeDataString($room))" (@{ purge_up_to_event_id = $eid; delete_local_events = $true } | ConvertTo-Json -Compress) $tok
  Start-Sleep -Seconds 3
  $r
}
$purge1 = Purge-Before $rP $e2.event_id $tokA
$afterPurge1 = Download-Status $upA.mxc $tokA
$msgsNow = @((Room-Messages $rP $tokA 'b' 50).chunk | Where-Object { $_.type -eq 'm.room.message' } | ForEach-Object { $_.event_id })
Check '[2.2] purging the redacted event (original retained) releases once: the live second event keeps the media' ($purge1.purge_id -and ($msgsNow -notcontains $e1.event_id) -and ($msgsNow -contains $e2.event_id) -and $afterPurge1 -eq 200) "purge_id=$($purge1.purge_id) status=$afterPurge1 remaining=$($msgsNow.Count)"
$null = Redact $rP $e2.event_id $tokA
$e3 = Send-Raw $rP 'm.room.message' @{ msgtype = 'm.text'; body = 'marker' } $tokA $null
$purge2 = Purge-Before $rP $e3.event_id $tokA
$afterPurge2 = Download-Status $upA.mxc $tokA
Check '[2.3] purging the last holder (its original dropped) frees the media exactly then (410)' ($purge2.purge_id -and $afterPurge2 -eq 410) "status=$afterPurge2"
$log2 = (Get-Content "$OUT\s2.out" -Raw) -replace "`e\[[0-9;]*m", ''
Check '[2.4] no negative count was ever logged' (-not ($log2 -match 'reference count is negative')) ''
Stop-Server $p

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"

# A pending ReceiveAsync or an undisposed socket can keep this process alive long after the
# last line is written — every batch run this session looked like a hang for that reason, with
# the results already on disk. Leave on purpose, and say in the exit code whether it passed:
# a FAIL used to be invisible to anything that only looked at the exit status.
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
