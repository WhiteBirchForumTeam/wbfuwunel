. (Join-Path $PSScriptRoot 'wbf-helpers.ps1')
# Device versions (docs/design/wbf-room-device-version.md): each joined member's device version and the room's
# version on /members (F2), the account version that moves with its keys (F1), and the encrypted Event/Send that is
# refused with 1506 once the room's version has moved (F4); scenario 2 is the DeviceChanged push to connections that
# declared device versions (F3).
$OUT = "$S\e2e16-out"; New-Item -ItemType Directory -Force $OUT | Out-Null
$RESULT = "$OUT\results.txt"; '' | Out-File $RESULT -Encoding utf8
$script:Pass = 0; $script:Fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { $script:Pass++; Log "  ok   $name  $detail" } else { $script:Fail++; Log "  FAIL $name  $detail" }
}
$IS_BRIDGED = 0x10
$FEATURE = 'org.wbftw.device_versions'

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
function Call($ws, [byte[]]$pack) { Ws-Send $ws $pack; Recv-Or-Null $ws 5000 }
function Enc([string]$text) { [uri]::EscapeDataString($text) }
function Register($name) { Api Post '/_matrix/client/v3/register' ('{"username":"' + $name + '","password":"pw-' + $name + '","auth":{"type":"m.login.dummy"}}') $null }
function Hello($ws, [string[]]$features) { Call $ws (Json-Pack 0x01 0x01 0 1 @{ protocol = 1; client = 'e2e16'; features = $features } $null) }
function Device-Keys($user, $device, [string]$key) {
  @{ user_id = $user; device_id = $device; algorithms = @('m.olm.v1.curve25519-aes-sha2', 'm.megolm.v1.aes-sha2')
     keys = @{ "curve25519:$device" = $key; "ed25519:$device" = $key }
     signatures = @{ $user = @{ "ed25519:$device" = 'c2lnbmF0dXJlZm9yZTJlMTY' } } }
}
$MEGOLM = '{"algorithm":"m.megolm.v1.aes-sha2","ciphertext":"AwgAEnACgAkLmt6qF84IK","device_id":"E2E16","sender_key":"c2VuZGVya2V5","session_id":"c2Vzc2lvbg"}'
$script:SendSeq = 100
# One native Event/Send; $roomVersion $null leaves the field out.
function Send-Event($ws, [string]$room, [string]$type, $roomVersion, [string]$txn, [string]$content = $MEGOLM) {
  $script:SendSeq++
  $meta = [ordered]@{ room_id = $room; type = $type; txn_id = $txn }
  if ($null -ne $roomVersion) { $meta.room_version = [uint64]$roomVersion }
  Call $ws (Json-Pack 0x14 0x02 0 $script:SendSeq $meta ([Text.Encoding]::UTF8.GetBytes($content)))
}
function Members($tok, [string]$room) { Http GET "/_matrix/client/v3/rooms/$(Enc $room)/members" $null $tok }
function Room-Version($members) { $members.json.'org.wbftw.room_version' }
function Device-Version($members, [string]$user) {
  $event = @($members.json.chunk | Where-Object { $_.state_key -eq $user -and $_.content.membership -eq 'join' }) | Select-Object -First 1
  if ($null -eq $event) { $null } else { $event.unsigned.'org.wbftw.device_version' }
}
function Seq-Of([string]$version) { if ($version -match '^(\d+)-') { [uint64]$Matches[1] } else { -1 } }
function Is-1506($p, $expected) { $null -ne $p -and $p.subtype -eq 3 -and $p.meta.code_id -eq 1506 -and $p.meta.code -eq 'RoomDevicesChanged' -and [uint64]$p.meta.room_version -eq [uint64]$expected }
function Is-SendAck($p) { $null -ne $p -and $p.subtype -eq 2 -and "$($p.meta.event_id)".StartsWith('$') }

# Matrix canonical JSON, written out here from the rules (sorted keys, no whitespace, minimal escapes) so the
# hash below is recomputed without borrowing anything from the server.
function Canonical-Json($v) {
  if ($null -eq $v) { return 'null' }
  if ($v -is [string]) {
    $sb = New-Object System.Text.StringBuilder; [void]$sb.Append('"')
    foreach ($ch in $v.ToCharArray()) {
      if ($ch -eq '"') { [void]$sb.Append('\"') } elseif ($ch -eq '\') { [void]$sb.Append('\\') }
      elseif ([int]$ch -lt 0x20) { [void]$sb.Append(('\u{0:x4}' -f [int]$ch)) } else { [void]$sb.Append($ch) }
    }
    [void]$sb.Append('"'); return $sb.ToString()
  }
  if ($v -is [bool]) { return $(if ($v) { 'true' } else { 'false' }) }
  if ($v -is [int] -or $v -is [long] -or $v -is [uint64]) { return "$v" }
  if ($v -is [System.Management.Automation.PSCustomObject]) {
    $names = [string[]]@($v.PSObject.Properties.Name); [Array]::Sort($names, [StringComparer]::Ordinal)
    return '{' + ((@($names | ForEach-Object { (Canonical-Json $_) + ':' + (Canonical-Json $v.$_) })) -join ',') + '}'
  }
  return '[' + ((@($v | ForEach-Object { Canonical-Json $_ })) -join ',') + ']'
}
# §3.4, from what /keys/query shows a third party: only the owner's signatures, no `unsigned`.
function Only-What-Everyone-Sees($key, [string]$owner) {
  if ($null -eq $key) { return $null }
  $copy = $key | ConvertTo-Json -Depth 20 | ConvertFrom-Json
  $copy.PSObject.Properties.Remove('unsigned')
  if ($null -ne $copy.signatures) {
    foreach ($signer in @($copy.signatures.PSObject.Properties.Name)) { if ($signer -ne $owner) { $copy.signatures.PSObject.Properties.Remove($signer) } }
  }
  $copy
}
function Recompute-Hash($query, [string]$owner) {
  $items = @((Only-What-Everyone-Sees $query.master_keys.$owner $owner), (Only-What-Everyone-Sees $query.self_signing_keys.$owner $owner))
  $devices = $query.device_keys.$owner
  $ids = [string[]]@($devices.PSObject.Properties.Name); [Array]::Sort($ids, [StringComparer]::Ordinal)
  foreach ($id in $ids) { $items += ,(Only-What-Everyone-Sees $devices.$id $owner) }
  $framed = New-Object System.IO.MemoryStream
  foreach ($item in $items) {
    $bytes = if ($null -eq $item) { [byte[]]@() } else { [Text.Encoding]::UTF8.GetBytes((Canonical-Json $item)) }
    $len = BE32 ([uint32]$bytes.Length); $framed.Write($len, 0, 4); if ($bytes.Length -gt 0) { $framed.Write($bytes, 0, $bytes.Length) }
  }
  $digest = [System.Security.Cryptography.SHA256]::Create().ComputeHash($framed.ToArray())
  (($digest[0..4] | ForEach-Object { $_.ToString('x2') }) -join '')
}

Log '################ Scenario 1: device versions, room versions, and the send they guard ################'
$db = "$S\e2e16db"; Remove-Item -Recurse -Force $db -EA SilentlyContinue; New-Item -ItemType Directory -Force $db | Out-Null
$cfg = Write-Config $db 86400
$server = Start-Server $cfg 's1'
$regA = Register 'alice'; $regB = Register 'bob'; $regC = Register 'carol'; $regD = Register 'dave'
$alice = $regA.user_id; $bob = $regB.user_id; $carol = $regC.user_id; $dave = $regD.user_id
$tokA = $regA.access_token; $tokB = $regB.access_token; $tokC = $regC.access_token
$bobDevice1 = $regB.device_id

$null = Http POST '/_matrix/client/v3/keys/upload' @{ device_keys = (Device-Keys $bob $bobDevice1 'Ym9ia2V5b25lZm9yZTJlMTY') } $tokB
$room = (Http POST '/_matrix/client/v3/createRoom' @{ preset = 'private_chat' } $tokA).json.room_id
$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/invite" @{ user_id = $bob } $tokA
$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/join" @{} $tokB

$wsA = Ws-Open $tokA
$hello = Hello $wsA @($FEATURE)
Check '[1.1] Hello: the server lists org.wbftw.device_versions among its features' `
  ($hello.subtype -eq 2 -and @($hello.meta.features) -contains $FEATURE) "features=$(@($hello.meta.features) -join ',')"

$m1 = Members $tokA $room
$rv1 = Room-Version $m1
$bobV1 = Device-Version $m1 $bob
$joinedCarry = @($m1.json.chunk | Where-Object { $_.content.membership -eq 'join' } | Where-Object { "$($_.unsigned.'org.wbftw.device_version')" -match '^\d+-[0-9a-f]{10}$' }).Count
Check '[1.2] /members: the room version at the top, a seq-hash device version on every joined member' `
  ($m1.status -eq 200 -and [uint64]$rv1 -gt 0 -and $joinedCarry -eq 2 -and $bobV1 -match '^\d+-[0-9a-f]{10}$') "room_version=$rv1 bob=$bobV1 alice=$(Device-Version $m1 $alice)"

$bridged = Call $wsA (New-Pack 0x13 0x29 $IS_BRIDGED 0 2 ([Text.Encoding]::UTF8.GetBytes((@{ room_id = $room } | ConvertTo-Json -Compress))) @())
$bridgedBody = if ($bridged.data.Length -gt 0) { [Text.Encoding]::UTF8.GetString([byte[]]$bridged.data) | ConvertFrom-Json } else { $null }
$withAt = Call $wsA (New-Pack 0x13 0x29 $IS_BRIDGED 0 3 ([Text.Encoding]::UTF8.GetBytes((@{ room_id = $room; at = 's1_2_3' } | ConvertTo-Json -Compress))) @())
Check '[1.3] the bridge Members carries the same two fields as HTTP; with `at` it is the bridge''s own InvalidRequest' `
  ($bridged.subtype -eq 2 -and $bridgedBody.'org.wbftw.room_version' -eq $rv1 -and @($bridgedBody.chunk).Count -eq @($m1.json.chunk).Count -and $withAt.subtype -eq 3 -and $withAt.meta.code_id -eq 1201) `
  "bridged_rv=$($bridgedBody.'org.wbftw.room_version') at=$($withAt.metaText)"

$sent = Send-Event $wsA $room 'm.room.encrypted' $rv1 't-1'
Check '[1.4] an encrypted send with the current room version is sent' (Is-SendAck $sent) "reply=$($sent.metaText)"

# Condition 1 of §10: Bob adds a device; Alice still holds the old room version.
$bobLogin = Http POST '/_matrix/client/v3/login' @{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = 'bob' }; password = 'pw-bob' } $null
$bobDevice2 = $bobLogin.json.device_id
$null = Http POST '/_matrix/client/v3/keys/upload' @{ device_keys = (Device-Keys $bob $bobDevice2 'Ym9ia2V5dHdvZm9yZTJlMTY') } $bobLogin.json.access_token
$m2 = Members $tokA $room; $rv2 = Room-Version $m2; $bobV2 = Device-Version $m2 $bob
$stale = Send-Event $wsA $room 'm.room.encrypted' $rv1 't-2'
$fresh = Send-Event $wsA $room 'm.room.encrypted' $rv2 't-3'
Check '[1.5] Bob adds a device: his seq moves and his hash changes, the old room version gets 1506 with the new one, and the new one is sent' `
  ((Seq-Of $bobV2) -gt (Seq-Of $bobV1) -and $bobV2.Split('-')[1] -ne $bobV1.Split('-')[1] -and [uint64]$rv2 -gt [uint64]$rv1 -and (Is-1506 $stale $rv2) -and (Is-SendAck $fresh)) `
  "bob=$bobV1 -> $bobV2 rv=$rv1 -> $rv2 stale=$($stale.metaText) fresh=$($fresh.metaText)"

$retry = Send-Event $wsA $room 'm.room.encrypted' $rv1 't-3'
Check '[1.6] a retry of a send that went through answers with its event, even with a stale version' `
  ((Is-SendAck $retry) -and $retry.meta.event_id -eq $fresh.meta.event_id) "first=$($fresh.meta.event_id) retry=$($retry.metaText)"

$query = (Http POST '/_matrix/client/v3/keys/query' @{ device_keys = @{ $bob = @() } } $tokC).json
$recomputed = Recompute-Hash $query $bob
Check '[1.7] the hash is what a third party recomputes from /keys/query with the documented algorithm' `
  ($recomputed -eq $bobV2.Split('-')[1]) "server=$($bobV2.Split('-')[1]) recomputed=$recomputed"

# Each F1 path moves the seq: cross-signing keys (the first upload needs no UIAA), then deleting a device.
$null = Http POST '/_matrix/client/v3/keys/device_signing/upload' @{ master_key = @{ user_id = $bob; usage = @('master'); keys = @{ 'ed25519:Ym9ibWFzdGVyZTJlMTY' = 'Ym9ibWFzdGVyZTJlMTY' } } } $tokB
$bobV3 = Device-Version (Members $tokA $room) $bob
$del1 = Http DELETE "/_matrix/client/v3/devices/$(Enc $bobDevice2)" @{} $tokB
$del2 = Http DELETE "/_matrix/client/v3/devices/$(Enc $bobDevice2)" @{ auth = @{ type = 'm.login.password'; session = $del1.json.session; identifier = @{ type = 'm.id.user'; user = 'bob' }; password = 'pw-bob' } } $tokB
$m4 = Members $tokA $room; $bobV4 = Device-Version $m4 $bob; $rv4 = Room-Version $m4
$query4 = (Http POST '/_matrix/client/v3/keys/query' @{ device_keys = @{ $bob = @() } } $tokC).json
Check '[1.8] a master key upload and a device deletion each move the seq; the hash still recomputes after both' `
  ((Seq-Of $bobV3) -gt (Seq-Of $bobV2) -and $del2.status -eq 200 -and (Seq-Of $bobV4) -gt (Seq-Of $bobV3) -and (Recompute-Hash $query4 $bob) -eq $bobV4.Split('-')[1] -and [uint64]$rv4 -gt [uint64]$rv2) `
  "bob=$bobV2 -> $bobV3 -> $bobV4 delete=$($del1.status)/$($del2.status) rv=$rv4"

# Condition 2 of §10: an invite does not move the room; a join, a leave, a kick and a ban do.
$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/invite" @{ user_id = $dave } $tokA
$rvInvite = Room-Version (Members $tokA $room)
$afterInvite = Send-Event $wsA $room 'm.room.encrypted' $rv4 't-4'
Check '[1.9] an invite leaves the room version where it was: the old one is still sent' `
  ([uint64]$rvInvite -eq [uint64]$rv4 -and (Is-SendAck $afterInvite)) "rv=$rv4 after_invite=$rvInvite reply=$($afterInvite.metaText)"

$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/invite" @{ user_id = $carol } $tokA
$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/join" @{} $tokC
$rvJoin = Room-Version (Members $tokA $room)
$afterJoin = Send-Event $wsA $room 'm.room.encrypted' $rv4 't-5'
$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/leave" @{} $tokC
$rvLeave = Room-Version (Members $tokA $room)
$afterLeave = Send-Event $wsA $room 'm.room.encrypted' $rvJoin 't-6'
$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/ban" @{ user_id = $dave; reason = 'e2e16' } $tokA
$rvBan = Room-Version (Members $tokA $room)
$afterBan = Send-Event $wsA $room 'm.room.encrypted' $rvLeave 't-7'
Check '[1.10] a join, a leave and a ban each move the room version, and the one before is refused' `
  ([uint64]$rvJoin -gt [uint64]$rv4 -and (Is-1506 $afterJoin $rvJoin) -and [uint64]$rvLeave -gt [uint64]$rvJoin -and (Is-1506 $afterLeave $rvLeave) -and [uint64]$rvBan -gt [uint64]$rvLeave -and (Is-1506 $afterBan $rvBan)) `
  "rv=$rv4 join=$rvJoin leave=$rvLeave ban=$rvBan"

$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/invite" @{ user_id = $carol } $tokA
$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/join" @{} $tokC
$rvRejoin = Room-Version (Members $tokA $room)
$null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/kick" @{ user_id = $carol; reason = 'e2e16' } $tokA
$rvKick = Room-Version (Members $tokA $room)
$afterKick = Send-Event $wsA $room 'm.room.encrypted' $rvRejoin 't-8'
$forget = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/forget" @{} $tokC
$rvForget = Room-Version (Members $tokA $room)
$afterForget = Send-Event $wsA $room 'm.room.encrypted' $rvRejoin 't-9'
Check '[1.11] a kick moves the room version, and the kicked member forgetting the room does not move it back' `
  ([uint64]$rvKick -gt [uint64]$rvRejoin -and (Is-1506 $afterKick $rvKick) -and $forget.status -eq 200 -and [uint64]$rvForget -eq [uint64]$rvKick -and (Is-1506 $afterForget $rvKick)) `
  "rejoin=$rvRejoin kick=$rvKick forget=$($forget.status) after_forget=$rvForget"

# Conditions 7 and 8 of §10: no agreement, no check; an agreement cannot be skipped.
$httpSend = Http PUT "/_matrix/client/v3/rooms/$(Enc $room)/send/m.room.encrypted/t-http" ($MEGOLM | ConvertFrom-Json) $tokA
$wsB = Ws-Open $tokB
$undeclared = Send-Event $wsB $room 'm.room.encrypted' $null 't-10'
Check '[1.12] no agreement, no check: HTTP and a connection that did not declare send without a room version' `
  ($httpSend.status -eq 200 -and (Is-SendAck $undeclared)) "http=$($httpSend.status) ws=$($undeclared.metaText)"

$missing = Send-Event $wsA $room 'm.room.encrypted' $null 't-11'
$plain = Send-Event $wsA $room 'm.room.message' $null 't-12' '{"msgtype":"m.text","body":"plain"}'
Check '[1.13] a connection that declared cannot leave the version out of an encrypted send (InvalidRequest); plaintext is not checked' `
  ($missing.subtype -eq 3 -and $missing.meta.code_id -eq 1201 -and (Is-SendAck $plain)) "missing=$($missing.metaText) plain=$($plain.metaText)"

$undeclare = Hello $wsA @('push')
$afterUndeclare = Send-Event $wsA $room 'm.room.encrypted' $null 't-13'
Check '[1.14] a later Hello without the feature takes the declaration back' `
  ($undeclare.subtype -eq 2 -and (Is-SendAck $afterUndeclare)) "reply=$($afterUndeclare.metaText)"

$wsA.Dispose(); $wsB.Dispose()
$before = Members $tokA $room
Stop-Server $server
$server = Start-Server $cfg 's2'
$after = Members $tokA $room
Check '[1.15] after a restart the room version and every device version read the same' `
  ((Room-Version $after) -eq (Room-Version $before) -and (Device-Version $after $bob) -eq (Device-Version $before $bob) -and (Device-Version $after $alice) -eq (Device-Version $before $alice)) `
  "before=$(Room-Version $before)/$(Device-Version $before $bob) after=$(Room-Version $after)/$(Device-Version $after $bob)"
Stop-Server $server

Log '################ Scenario 2: DeviceChanged, only to connections that declared (F3) ################'
function Drain($ws, [int]$quietMs = 2500) {
  $packs = @()
  while ($true) { $p = Recv-Or-Null $ws $quietMs; if ($null -eq $p) { break }; $packs += ,$p }
  ,$packs
}
function Only-DeviceChanged($packs) { ,@($packs | Where-Object { $_.kind -eq 0x14 -and $_.subtype -eq 7 }) }
function New-Device-Keys($user, [string]$password, [string]$key) {
  $login = Http POST '/_matrix/client/v3/login' @{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = $user.Split(':')[0].TrimStart('@') }; password = $password } $null
  $null = Http POST '/_matrix/client/v3/keys/upload' @{ device_keys = (Device-Keys $user $login.json.device_id $key) } $login.json.access_token
}
$db2 = "$S\e2e16db2"; Remove-Item -Recurse -Force $db2 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db2 | Out-Null
$cfg2 = Write-Config $db2 86400
$server = Start-Server $cfg2 's3'
$regA = Register 'alice'; $regB = Register 'bob'; $regC = Register 'carol'
$alice = $regA.user_id; $bob = $regB.user_id; $carol = $regC.user_id
$tokA = $regA.access_token; $tokB = $regB.access_token; $tokC = $regC.access_token
$r1 = (Http POST '/_matrix/client/v3/createRoom' @{ preset = 'private_chat' } $tokA).json.room_id
$r2 = (Http POST '/_matrix/client/v3/createRoom' @{ preset = 'private_chat' } $tokA).json.room_id
foreach ($room in @($r1, $r2)) {
  $null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/invite" @{ user_id = $bob } $tokA
  $null = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/join" @{} $tokB
}
$null = Http POST "/_matrix/client/v3/rooms/$(Enc $r1)/invite" @{ user_id = $carol } $tokA
$null = Http POST "/_matrix/client/v3/rooms/$(Enc $r1)/join" @{} $tokC

$wsDeclared = Ws-Open $tokA; $null = Hello $wsDeclared @($FEATURE)
$wsPlain = Ws-Open $tokA; $null = Hello $wsPlain @('push')
$subD = Call $wsDeclared (Json-Pack 0x14 0x04 (Conv 20) 0 @{} $null)
$subP = Call $wsPlain (Json-Pack 0x14 0x04 (Conv 21) 0 @{} $null)
$null = Drain $wsDeclared 1500; $null = Drain $wsPlain 1500

New-Device-Keys $bob 'pw-bob' 'Ym9ic2Vjb25kZTJlMTY'
$toDeclared = Only-DeviceChanged (Drain $wsDeclared)
$toPlain = Only-DeviceChanged (Drain $wsPlain 1500)
$mR1 = Members $tokA $r1; $mR2 = Members $tokA $r2
$dc = if ($toDeclared.Count -gt 0) { $toDeclared[0] } else { $null }
Check '[2.1] Bob adds a device: one DeviceChanged on the declared connection, both shared rooms in it, each with the version /members reads; the id is its Subscribe''s' `
  ($subD.subtype -eq 2 -and $toDeclared.Count -eq 1 -and $dc.meta.user_id -eq $bob -and $dc.meta.device_version -eq (Device-Version $mR1 $bob) -and [uint64]$dc.meta.rooms.$r1 -eq [uint64](Room-Version $mR1) -and [uint64]$dc.meta.rooms.$r2 -eq [uint64](Room-Version $mR2) -and $dc.meta.gap -eq $false -and $dc.id -eq (Conv 20)) `
  "packs=$($toDeclared.Count) meta=$(if ($dc) { $dc.metaText } else { 'none' }) r1=$(Room-Version $mR1) r2=$(Room-Version $mR2)"
Check '[2.2] the connection that did not declare gets no DeviceChanged' `
  ($subP.subtype -eq 2 -and $toPlain.Count -eq 0) "packs=$($toPlain.Count)"

# A subscribed connection is pushed its own send, possibly before the reply: skip the pushes to reach the reply.
$script:SendSeq++
Ws-Send $wsDeclared (Json-Pack 0x14 0x02 0 $script:SendSeq ([ordered]@{ room_id = $r2; type = 'm.room.encrypted'; txn_id = 't-dc-1'; room_version = [uint64]$dc.meta.rooms.$r2 }) ([Text.Encoding]::UTF8.GetBytes($MEGOLM)))
$sentWithPushed = $null
for ($n = 0; $n -lt 10 -and $null -eq $sentWithPushed; $n++) { $p = Recv-Or-Null $wsDeclared 5000; if ($null -eq $p) { break }; if ($p.kind -eq 0x01) { $sentWithPushed = $p } }
$null = Drain $wsDeclared 1500
Check '[2.3] the room version from DeviceChanged is the one to send with' ((Is-SendAck $sentWithPushed)) "reply=$($sentWithPushed.metaText)"

New-Device-Keys $carol 'pw-carol' 'Y2Fyb2xzZWNvbmRlMmUxNg'
$carolChanged = Only-DeviceChanged (Drain $wsDeclared)
$cc = if ($carolChanged.Count -gt 0) { $carolChanged[0] } else { $null }
# @(if ...): an `if` unrolls a one-element array into its element, and [0] of a string is its first character.
$carolRooms = @(if ($cc) { $cc.meta.rooms.PSObject.Properties.Name })
Check '[2.4] Carol shares only one room with this connection: her DeviceChanged names only that room' `
  ($carolChanged.Count -eq 1 -and $cc.meta.user_id -eq $carol -and $carolRooms.Count -eq 1 -and $carolRooms[0] -eq $r1) "packs=$($carolChanged.Count) rooms=$($carolRooms -join ',') r1=$r1"

$wsDeclared.Dispose(); $wsPlain.Dispose()
Stop-Server $server
Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
