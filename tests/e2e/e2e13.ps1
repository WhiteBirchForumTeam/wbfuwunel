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
# 0x11/0x9F is inside the bridge range but assigned to nothing, so the bridge road does not know it.
$unmapped = Call $ws (Bridged-Pack 0x11 0x9F 41 $null $null)
Check '[0.1] a bridge call for a pair the bridge table does not have -> UnknownKind, and the reply carries IS_BRIDGED' `
  ($null -ne $unmapped -and $unmapped.subtype -eq 3 -and $unmapped.meta.code_id -eq $UNKNOWN_KIND -and (Is-BridgedReply $unmapped) -and $unmapped.seq -eq 41) `
  "flags=0x$('{0:X2}' -f $unmapped.flags) $(Describe $unmapped)"

$httpUnmapped = Send-Pack (Bridged-Pack 0x11 0x9F 42 $null $null) $tok
Check '[0.2] the same over POST /_wbf/v1/pack: the bridge does not care about the transport' `
  ($httpUnmapped.subtype -eq 3 -and $httpUnmapped.meta.code_id -eq $UNKNOWN_KIND -and (Is-BridgedReply $httpUnmapped) -and $httpUnmapped.seq -eq 42) `
  "flags=0x$('{0:X2}' -f $httpUnmapped.flags) $(Describe $httpUnmapped)"

$native = Call $ws (New-Pack 0x11 0x9F 0 0 43 @() @())
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

$reserved = Send-Pack (New-Pack 0x11 0x9F 0x20 0 46 @() @()) $tok
Check '[0.6] bit5 is still reserved: Corrupt' ($reserved.subtype -eq 3 -and $reserved.meta.code_id -eq $CORRUPT) "$(Describe $reserved)"

$anonymous = Ws-Open $null
$anon = Call $anonymous (Bridged-Pack 0x11 0x9F 47 $null $null)
Check '[0.7] a connection that has not logged in gets the same UnknownKind: the table is asked before anything about the session' `
  ($null -ne $anon -and $anon.meta.code_id -eq $UNKNOWN_KIND -and (Is-BridgedReply $anon)) "$(Describe $anon)"
$anonymous.Dispose()

Log '################ Scenario 1: batch 1, each endpoint through the bridge and through Matrix HTTP ################'
# Reads: the bridge's body and the HTTP body compared after canonicalizing (keys sorted, arrays sorted, and the
# fields that change between two calls dropped: `age`, `last_seen_ts`, `last_seen_ip`). Writes: done through the
# bridge, read back over HTTP. The gates: the bridge must be refused exactly where HTTP is.
$alice = $reg.user_id; $aliceDevice = $reg.device_id
$regB = Api Post '/_matrix/client/v3/register' '{"username":"bob","password":"pw-pw-pw-pw","auth":{"type":"m.login.dummy"}}' $null
$regC = Api Post '/_matrix/client/v3/register' '{"username":"carol","password":"pw-pw-pw-pw","auth":{"type":"m.login.dummy"}}' $null
$bob = $regB.user_id; $tokB = $regB.access_token
$carol = $regC.user_id; $tokC = $regC.access_token
$script:BridgeSeq = 1000

function To-JsonBytes($value) { if ($null -eq $value) { @() } else { [Text.Encoding]::UTF8.GetBytes((ConvertTo-Json $value -Compress -Depth 20)) } }
# One bridge call; the reply gains .status (from meta), .text and .body (the data, as text and as JSON).
function Bridge($ws, [byte]$kind, [byte]$subtype, $variables, $body) {
  $script:BridgeSeq++
  $p = Call $ws (New-Pack $kind $subtype $IS_BRIDGED 0 $script:BridgeSeq (To-JsonBytes $variables) (To-JsonBytes $body))
  if ($null -eq $p) { return @{ subtype = -1; meta = $null; text = ''; body = $null; status = 0; flags = 0 } }
  $p.text = if ($p.data.Length -gt 0) { [Text.Encoding]::UTF8.GetString([byte[]]$p.data) } else { '' }
  $p.body = $null; if ($p.text) { try { $p.body = $p.text | ConvertFrom-Json } catch {} }
  $p.status = if ($null -ne $p.meta) { [int]$p.meta.status } else { 0 }
  $p
}
function Http([string]$method, [string]$path, $body, $tok) {
  $req = New-Object System.Net.Http.HttpRequestMessage ((New-Object System.Net.Http.HttpMethod $method), "$B$path")
  if ($tok) { $req.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $tok) }
  if ($null -ne $body) { $req.Content = New-Object System.Net.Http.StringContent ((ConvertTo-Json $body -Compress -Depth 20), [Text.Encoding]::UTF8, 'application/json') }
  $resp = $script:PackHttpClient.SendAsync($req).Result
  $text = $resp.Content.ReadAsStringAsync().Result
  $json = $null; if ($text) { try { $json = $text | ConvertFrom-Json } catch {} }
  @{ status = [int]$resp.StatusCode; text = $text; json = $json }
}
$VOLATILE = @('age', 'last_seen_ts', 'last_seen_ip')
function Canon($value) {
  if ($null -eq $value) { return 'null' }
  if ($value -is [System.Management.Automation.PSCustomObject]) {
    $parts = foreach ($name in @($value.PSObject.Properties.Name | Where-Object { $VOLATILE -notcontains $_ } | Sort-Object)) { (ConvertTo-Json $name -Compress) + ':' + (Canon $value.$name) }
    return '{' + (@($parts) -join ',') + '}'
  }
  if ($value -is [System.Array]) { $items = @(foreach ($item in $value) { Canon $item }) | Sort-Object; return '[' + (@($items) -join ',') + ']' }
  return (ConvertTo-Json $value -Compress)
}
function Same-As-Http($bridged, $http) { $bridged.subtype -eq 2 -and $bridged.status -eq $http.status -and (Canon $bridged.body) -eq (Canon $http.json) }
function Enc([string]$text) { [uri]::EscapeDataString($text) }
function Is-Ack($p) { $p.subtype -eq 2 -and (Is-BridgedReply $p) -and $p.status -ge 200 -and $p.status -lt 300 }

$wsA = Ws-Open $tok; $wsB = Ws-Open $tokB; $wsC = Ws-Open $tokC

# ---- 0x11 Account ----
$w = Bridge $wsA 0x11 0x20 $null $null
$hw = Http GET '/_matrix/client/v3/account/whoami' $null $tok
Check '[1.1] WhoAmI: an Ack with IS_BRIDGED, and the body is what HTTP returns' ((Is-Ack $w) -and (Same-As-Http $w $hw) -and $w.body.user_id -eq $alice) "bridge=$($w.text) http=$($hw.text)"

$set = Bridge $wsA 0x11 0x23 @{ user_id = $alice; field = 'displayname' } @{ displayname = 'Alice via bridge' }
$hname = Http GET "/_matrix/client/v3/profile/$(Enc $alice)/displayname" $null $tok
$gname = Bridge $wsA 0x11 0x22 @{ user_id = $alice; field = 'displayname' } $null
Check '[1.2] SetProfileField through the bridge is what HTTP reads back; GetProfileField agrees with HTTP' ((Is-Ack $set) -and $hname.json.displayname -eq 'Alice via bridge' -and (Same-As-Http $gname $hname)) "set=$($set.status) http=$($hname.text) bridge=$($gname.text)"

$gp = Bridge $wsA 0x11 0x21 @{ user_id = $alice } $null
$hp = Http GET "/_matrix/client/v3/profile/$(Enc $alice)" $null $tok
Check '[1.3] GetProfile agrees with HTTP' (Same-As-Http $gp $hp) "bridge=$($gp.text) http=$($hp.text)"

$del = Bridge $wsA 0x11 0x24 @{ user_id = $alice; field = 'displayname' } $null
$hgone = Http GET "/_matrix/client/v3/profile/$(Enc $alice)/displayname" $null $tok
Check '[1.4] DeleteProfileField through the bridge: HTTP no longer has the display name' ((Is-Ack $del) -and ($hgone.status -eq 404 -or $null -eq $hgone.json.displayname)) "delete=$($del.status) http=$($hgone.status) $($hgone.text)"

$sad = Bridge $wsA 0x11 0x26 @{ user_id = $alice; event_type = 'com.example.bridge' } @{ answer = 42 }
$had = Http GET "/_matrix/client/v3/user/$(Enc $alice)/account_data/com.example.bridge" $null $tok
$gad = Bridge $wsA 0x11 0x25 @{ user_id = $alice; event_type = 'com.example.bridge' } $null
Check '[1.5] SetAccountData / GetAccountData: written through the bridge, read back identically on both' ((Is-Ack $sad) -and $had.json.answer -eq 42 -and (Same-As-Http $gad $had)) "http=$($had.text) bridge=$($gad.text)"

# ---- 0x13 Room ----
$cr = Bridge $wsA 0x13 0x20 $null @{ name = 'bridged'; preset = 'public_chat' }
$room = $cr.body.room_id
$hjr = Http GET '/_matrix/client/v3/joined_rooms' $null $tok
$jr = Bridge $wsA 0x13 0x28 $null $null
Check '[1.8] CreateRoom through the bridge; JoinedRooms agrees with HTTP and contains it' ((Is-Ack $cr) -and $room -and @($hjr.json.joined_rooms) -contains $room -and (Same-As-Http $jr $hjr)) "room=$room bridge=$($jr.text)"

$srad = Bridge $wsA 0x11 0x28 @{ user_id = $alice; room_id = $room; event_type = 'com.example.room' } @{ pinned = $true }
$hrad = Http GET "/_matrix/client/v3/user/$(Enc $alice)/rooms/$(Enc $room)/account_data/com.example.room" $null $tok
$grad = Bridge $wsA 0x11 0x27 @{ user_id = $alice; room_id = $room; event_type = 'com.example.room' } $null
Check '[1.6] SetRoomAccountData / GetRoomAccountData agree with HTTP' ((Is-Ack $srad) -and $hrad.json.pinned -eq $true -and (Same-As-Http $grad $hrad)) "http=$($hrad.text)"

$st = Bridge $wsA 0x11 0x2A @{ user_id = $alice; room_id = $room; tag = 'u.work' } @{ order = 0.5 }
$htags = Http GET "/_matrix/client/v3/user/$(Enc $alice)/rooms/$(Enc $room)/tags" $null $tok
$gt = Bridge $wsA 0x11 0x29 @{ user_id = $alice; room_id = $room } $null
$dt = Bridge $wsA 0x11 0x2B @{ user_id = $alice; room_id = $room; tag = 'u.work' } $null
$htags2 = Http GET "/_matrix/client/v3/user/$(Enc $alice)/rooms/$(Enc $room)/tags" $null $tok
Check '[1.7] SetTag / GetTags / DeleteTag: the tag appears and disappears on HTTP' ((Is-Ack $st) -and $htags.json.tags.'u.work'.order -eq 0.5 -and (Same-As-Http $gt $htags) -and (Is-Ack $dt) -and $null -eq $htags2.json.tags.'u.work') "before=$($htags.text) after=$($htags2.text)"

$inv = Bridge $wsA 0x13 0x24 @{ room_id = $room } @{ user_id = $carol }
$cmem = Http GET "/_matrix/client/v3/rooms/$(Enc $room)/state/m.room.member/$(Enc $carol)" $null $tok
$cj = Bridge $wsC 0x13 0x21 @{ room_id_or_alias = $room } @{}
$cjr = Http GET '/_matrix/client/v3/joined_rooms' $null $tokC
Check '[1.9] Invite (alice) and Join by room id (carol), both through the bridge' ((Is-Ack $inv) -and $cmem.json.membership -eq 'invite' -and (Is-Ack $cj) -and @($cjr.json.joined_rooms) -contains $room) "invite=$($cmem.text) join=$($cj.text)"

$sa = Bridge $wsA 0x13 0x2B @{ room_alias = '#bridged:localhost' } @{ room_id = $room }
$ha = Http GET "/_matrix/client/v3/directory/room/$(Enc '#bridged:localhost')" $null $tok
$ga = Bridge $wsA 0x13 0x2A @{ room_alias = '#bridged:localhost' } $null
Check '[1.10] SetAlias / GetAlias: `#` in a path variable survives the trip, and both agree' ((Is-Ack $sa) -and $ha.json.room_id -eq $room -and (Same-As-Http $ga $ha)) "http=$($ha.text) bridge=$($ga.text)"

$bj = Bridge $wsB 0x13 0x21 @{ room_id_or_alias = '#bridged:localhost'; via = @('localhost') } @{}
Check '[1.11] Join by alias with a `via` array (bob)' ((Is-Ack $bj) -and $bj.body.room_id -eq $room) "$($bj.text) meta=$($bj.metaText)"

$members = Bridge $wsA 0x13 0x29 @{ room_id = $room } $null
$hmembers = Http GET "/_matrix/client/v3/rooms/$(Enc $room)/members" $null $tok
$joinedOnly = Bridge $wsA 0x13 0x29 @{ room_id = $room; membership = 'join' } $null
$hjoinedOnly = Http GET "/_matrix/client/v3/rooms/$(Enc $room)/members?membership=join" $null $tok
Check '[1.12] Members agrees with HTTP, with and without a query variable' ((Same-As-Http $members $hmembers) -and (Same-As-Http $joinedOnly $hjoinedOnly) -and @($hjoinedOnly.json.chunk).Count -eq 3) "all=$(@($hmembers.json.chunk).Count) joined=$(@($hjoinedOnly.json.chunk).Count)"

function Member-Of($who) { (Http GET "/_matrix/client/v3/rooms/$(Enc $room)/state/m.room.member/$(Enc $who)" $null $tok).json.membership }
$kick = Bridge $wsA 0x13 0x25 @{ room_id = $room } @{ user_id = $bob; reason = 'bridge kick' }
$afterKick = Member-Of $bob
$ban = Bridge $wsA 0x13 0x26 @{ room_id = $room } @{ user_id = $bob; reason = 'bridge ban' }
$afterBan = Member-Of $bob
$unban = Bridge $wsA 0x13 0x27 @{ room_id = $room } @{ user_id = $bob }
$afterUnban = Member-Of $bob
Check '[1.13] Kick, Ban, Unban through the bridge change the membership HTTP reads' ((Is-Ack $kick) -and $afterKick -eq 'leave' -and (Is-Ack $ban) -and $afterBan -eq 'ban' -and (Is-Ack $unban) -and $afterUnban -eq 'leave') "kick=$afterKick ban=$afterBan unban=$afterUnban"

$null = Http POST "/_matrix/client/v3/join/$(Enc $room)" @{} $tokB
$bobKicks = Bridge $wsB 0x13 0x25 @{ room_id = $room } @{ user_id = $alice }
$hbobKicks = Http POST "/_matrix/client/v3/rooms/$(Enc $room)/kick" @{ user_id = $alice } $tokB
Check '[1.14] a Matrix refusal is an Error with the same status and errcode HTTP gives, and the Matrix body in data' `
  ($bobKicks.subtype -eq 3 -and (Is-BridgedReply $bobKicks) -and $bobKicks.meta.code -eq 'Forbidden' -and $bobKicks.status -eq $hbobKicks.status -and $bobKicks.meta.errcode -eq $hbobKicks.json.errcode -and $bobKicks.body.errcode -eq $hbobKicks.json.errcode) `
  "bridge=$($bobKicks.metaText) http=$($hbobKicks.status) $($hbobKicks.text)"

# ---- 0x14 Event ----
$topic = Bridge $wsA 0x14 0x23 @{ room_id = $room; event_type = 'm.room.topic'; state_key = '' } @{ topic = 'bridged topic' }
$htopic = Http GET "/_matrix/client/v3/rooms/$(Enc $room)/state/m.room.topic/" $null $tok
$gtopic = Bridge $wsA 0x14 0x22 @{ room_id = $room; event_type = 'm.room.topic'; state_key = '' } $null
Check '[1.15] SetStateEvent with an empty state_key; GetStateEvent agrees with HTTP' ((Is-Ack $topic) -and $topic.body.event_id -and $htopic.json.topic -eq 'bridged topic' -and (Same-As-Http $gtopic $htopic)) "set=$($topic.text) http=$($htopic.text)"

$state = Bridge $wsA 0x14 0x21 @{ room_id = $room } $null
$hstate = Http GET "/_matrix/client/v3/rooms/$(Enc $room)/state" $null $tok
Check '[1.16] GetState agrees with HTTP' (Same-As-Http $state $hstate) "events=$(@($hstate.json).Count) bridgeBytes=$($state.text.Length)"

$topicEvent = $topic.body.event_id
$ge = Bridge $wsA 0x14 0x20 @{ room_id = $room; event_id = $topicEvent } $null
$hge = Http GET "/_matrix/client/v3/rooms/$(Enc $room)/event/$(Enc $topicEvent)" $null $tok
$ctx = Bridge $wsA 0x14 0x25 @{ room_id = $room; event_id = $topicEvent; limit = 2 } $null
$hctx = Http GET "/_matrix/client/v3/rooms/$(Enc $room)/context/$(Enc $topicEvent)?limit=2" $null $tok
Check '[1.17] GetEvent and Context (with a numeric query variable) agree with HTTP' ((Same-As-Http $ge $hge) -and (Same-As-Http $ctx $hctx)) "event=$($ge.status) context=$($ctx.status)"

$txn = [guid]::NewGuid().ToString('N')
$msg = (Http PUT "/_matrix/client/v3/rooms/$(Enc $room)/send/m.room.message/$txn" @{ msgtype = 'm.text'; body = 'to be redacted' } $tok).json.event_id
$red = Bridge $wsA 0x14 0x24 @{ room_id = $room; event_id = $msg; txn_id = "bridge-$txn" } @{ reason = 'through the bridge' }
$hred = Http GET "/_matrix/client/v3/rooms/$(Enc $room)/event/$(Enc $msg)" $null $tok
Check '[1.18] Redact through the bridge: HTTP reads the event redacted' ((Is-Ack $red) -and $red.body.event_id -and @($hred.json.content.PSObject.Properties).Count -eq 0) "redaction=$($red.text) event=$($hred.text)"

# ---- 0x15 Receipt ----
$txn2 = [guid]::NewGuid().ToString('N')
$readMe = (Http PUT "/_matrix/client/v3/rooms/$(Enc $room)/send/m.room.message/$txn2" @{ msgtype = 'm.text'; body = 'read me' } $tok).json.event_id
$typing = Bridge $wsA 0x15 0x20 @{ room_id = $room; user_id = $alice } @{ typing = $true; timeout = 3000 }
$markers = Bridge $wsA 0x15 0x21 @{ room_id = $room } @{ 'm.fully_read' = $readMe }
$receipt = Bridge $wsA 0x15 0x22 @{ room_id = $room; receipt_type = 'm.read'; event_id = $readMe } @{}
$hfully = Http GET "/_matrix/client/v3/user/$(Enc $alice)/rooms/$(Enc $room)/account_data/m.fully_read" $null $tok
Check '[1.19] Typing, ReadMarkers, Receipt through the bridge; the fully-read marker is what HTTP reads' ((Is-Ack $typing) -and (Is-Ack $markers) -and (Is-Ack $receipt) -and $hfully.json.event_id -eq $readMe) "typing=$($typing.status) markers=$($markers.status) receipt=$($receipt.status) fully_read=$($hfully.text)"

# ---- carol leaves and forgets ----
$leave = Bridge $wsC 0x13 0x22 @{ room_id = $room } @{ reason = 'bye' }
$cjr2 = Http GET '/_matrix/client/v3/joined_rooms' $null $tokC
$forget = Bridge $wsC 0x13 0x23 @{ room_id = $room } @{}
Check '[1.20] Leave and Forget through the bridge (carol)' ((Is-Ack $leave) -and @($cjr2.json.joined_rooms) -notcontains $room -and (Is-Ack $forget)) "leave=$($leave.status) forget=$($forget.status)"

$da = Bridge $wsA 0x13 0x2C @{ room_alias = '#bridged:localhost' } $null
$hda = Http GET "/_matrix/client/v3/directory/room/$(Enc '#bridged:localhost')" $null $tok
Check '[1.21] DeleteAlias through the bridge: HTTP no longer resolves it' ((Is-Ack $da) -and $hda.status -eq 404) "delete=$($da.status) http=$($hda.status)"

# ---- 0x16 Device ----
$ld = Bridge $wsA 0x16 0x20 $null $null
$hld = Http GET '/_matrix/client/v3/devices' $null $tok
$gd = Bridge $wsA 0x16 0x21 @{ device_id = $aliceDevice } $null
$hgd = Http GET "/_matrix/client/v3/devices/$(Enc $aliceDevice)" $null $tok
$ud = Bridge $wsA 0x16 0x22 @{ device_id = $aliceDevice } @{ display_name = 'renamed through the bridge' }
$hgd2 = Http GET "/_matrix/client/v3/devices/$(Enc $aliceDevice)" $null $tok
Check '[1.22] ListDevices and GetDevice agree with HTTP; UpdateDevice renames what HTTP reads' ((Same-As-Http $ld $hld) -and (Same-As-Http $gd $hgd) -and (Is-Ack $ud) -and $hgd2.json.display_name -eq 'renamed through the bridge') "renamed=$($hgd2.text)"

# ---- the refusals ----
$nobody = Bridge $wsA 0x11 0x22 @{ user_id = '@nobody:localhost'; field = 'displayname' } $null
$hnobody = Http GET "/_matrix/client/v3/profile/$(Enc '@nobody:localhost')/displayname" $null $tok
Check '[1.23] a Matrix 404 is an Error whose status, errcode and code match HTTP' `
  ($nobody.subtype -eq 3 -and $nobody.status -eq $hnobody.status -and $nobody.meta.errcode -eq $hnobody.json.errcode -and $nobody.meta.code -ne 'Internal') "bridge=$($nobody.metaText) http=$($hnobody.status) $($hnobody.text)"

$missing = Bridge $wsA 0x13 0x22 @{} @{}
$extra = Bridge $wsA 0x13 0x22 @{ room_id = $room; roomId = $room } @{}
$idNonZero = Call $wsA (New-Pack 0x11 0x20 $IS_BRIDGED (Conv 1) 1999 @() @())
Check '[1.24] a missing path variable, an undeclared variable and a non-zero id are InvalidRequest, and nothing is called' `
  ($missing.meta.code -eq 'InvalidRequest' -and $extra.meta.code -eq 'InvalidRequest' -and $idNonZero.meta.code -eq 'InvalidRequest' -and (Is-BridgedReply $missing)) "missing=$($missing.metaText) extra=$($extra.metaText) id=$($idNonZero.metaText)"

$wsAnon = Ws-Open $null
$anonWho = Bridge $wsAnon 0x11 0x20 $null $null
$httpPackWho = Send-Pack (New-Pack 0x11 0x20 $IS_BRIDGED 0 2000 @() @()) $tok
$httpPackBody = if ($httpPackWho.data.Length -gt 0) { [Text.Encoding]::UTF8.GetString([byte[]]$httpPackWho.data) | ConvertFrom-Json } else { $null }
Check '[1.25] a connection that has not logged in is refused by the endpoint itself (401, M_MISSING_TOKEN); the same call over HTTP pack with a token works' `
  ($anonWho.subtype -eq 3 -and $anonWho.meta.code -eq 'Unauthorized' -and $anonWho.status -eq 401 -and $anonWho.meta.errcode -eq 'M_MISSING_TOKEN' -and $httpPackWho.subtype -eq 2 -and $httpPackBody.user_id -eq $alice -and (Is-BridgedReply $httpPackWho)) `
  "anon=$($anonWho.metaText) httppack=$($httpPackWho.metaText)"
$wsAnon.Dispose()

$suspend = Http PUT "/_matrix/client/v1/admin/suspend/$(Enc $bob)" @{ suspended = $true } $tok
$suspendedCreate = Bridge $wsB 0x13 0x20 $null @{ name = 'should not exist' }
$hsuspendedCreate = Http POST '/_matrix/client/v3/createRoom' @{ name = 'should not exist either' } $tokB
$null = Http PUT "/_matrix/client/v1/admin/suspend/$(Enc $bob)" @{ suspended = $false } $tok
Check '[1.26] a suspended account cannot CreateRoom through the bridge, exactly as over HTTP: the gate is the HTTP gate' `
  ($suspend.status -eq 200 -and $suspendedCreate.subtype -eq 3 -and $suspendedCreate.meta.errcode -eq 'M_USER_SUSPENDED' -and $suspendedCreate.status -eq $hsuspendedCreate.status -and $hsuspendedCreate.json.errcode -eq 'M_USER_SUSPENDED') `
  "suspend=$($suspend.status) bridge=$($suspendedCreate.metaText) http=$($hsuspendedCreate.status) $($hsuspendedCreate.text)"

$lock = Http PUT "/_matrix/client/v1/admin/lock/$(Enc $bob)" @{ locked = $true } $tok
$lockedWhoAmI = Bridge $wsB 0x11 0x20 $null $null
$hlockedWhoAmI = Http GET '/_matrix/client/v3/account/whoami' $null $tokB
$null = Http PUT "/_matrix/client/v1/admin/lock/$(Enc $bob)" @{ locked = $false } $tok
# Refused one step earlier than the bridge: the WebSocket asks before every pack whether its session is still good
# (ws.rs `revalidate`), so a locked account never reaches the table. That refusal is the channel's own Unauthorized,
# not the endpoint's reply, which is why it names M_USER_LOCKED in the message and has no errcode or status field.
Check '[1.27] a locked account is refused on a bridged pack before the bridge runs, as HTTP refuses it (401 M_USER_LOCKED)' `
  ($lock.status -eq 200 -and $lockedWhoAmI.subtype -eq 3 -and $lockedWhoAmI.meta.code -eq 'Unauthorized' -and "$($lockedWhoAmI.meta.message)".Contains('M_USER_LOCKED') -and $hlockedWhoAmI.status -eq 401 -and $hlockedWhoAmI.json.errcode -eq 'M_USER_LOCKED') `
  "lock=$($lock.status) bridge=$($lockedWhoAmI.metaText) http=$($hlockedWhoAmI.status) $($hlockedWhoAmI.text)"

$wsA.Dispose(); $wsB.Dispose(); $wsC.Dispose()
Stop-Server $server

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
