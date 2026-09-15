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
Check '[1.26] a suspended account cannot CreateRoom through the bridge, exactly as over HTTP: the gate is the HTTP gate, and it is 403 Forbidden (MSC3823)' `
  ($suspend.status -eq 200 -and $suspendedCreate.subtype -eq 3 -and $suspendedCreate.meta.errcode -eq 'M_USER_SUSPENDED' -and $suspendedCreate.status -eq $hsuspendedCreate.status -and $hsuspendedCreate.json.errcode -eq 'M_USER_SUSPENDED' -and $hsuspendedCreate.status -eq 403 -and $suspendedCreate.meta.code -eq 'Forbidden') `
  "suspend=$($suspend.status) bridge=$($suspendedCreate.metaText) http=$($hsuspendedCreate.status) $($hsuspendedCreate.text)"

$lock = Http PUT "/_matrix/client/v1/admin/lock/$(Enc $bob)" @{ locked = $true } $tok
$lockedWhoAmI = Bridge $wsB 0x11 0x20 $null $null
$hlockedWhoAmI = Http GET '/_matrix/client/v3/account/whoami' $null $tokB
$null = Http PUT "/_matrix/client/v1/admin/lock/$(Enc $bob)" @{ locked = $false } $tok
# Refused one step earlier than the bridge: the WebSocket asks before every pack whether its session is still good
# (ws.rs `revalidate`), so a locked account never reaches the table. That refusal is the channel's own Unauthorized,
# not the endpoint's reply: no IS_BRIDGED and no data, but the same Matrix fields (errcode, soft_logout, status).
Check '[1.27] a locked account is refused on a bridged pack before the bridge runs, with the errcode and soft_logout HTTP gives' `
  ($lock.status -eq 200 -and $lockedWhoAmI.subtype -eq 3 -and $lockedWhoAmI.meta.code -eq 'Unauthorized' -and $lockedWhoAmI.meta.errcode -eq 'M_USER_LOCKED' -and $lockedWhoAmI.meta.soft_logout -eq $true -and $lockedWhoAmI.status -eq 401 -and ($lockedWhoAmI.flags -band $IS_BRIDGED) -eq 0 -and $hlockedWhoAmI.status -eq 401 -and $hlockedWhoAmI.json.errcode -eq 'M_USER_LOCKED' -and $hlockedWhoAmI.json.soft_logout -eq $true) `
  "lock=$($lock.status) bridge=$($lockedWhoAmI.metaText) http=$($hlockedWhoAmI.status) $($hlockedWhoAmI.text)"

$wsA.Dispose(); $wsB.Dispose(); $wsC.Dispose()
Stop-Server $server

Log '################ Scenario 2: batch 2, registration and the UIAA endpoints through the bridge ################'
# docs/design/wbf-api-bridge.md §3 batch 2. Registration: an anonymous connection registers with inhibit_login and
# then sends the native Session/Login. UIAA: the 401's flows and session arrive in data, the second round carries
# auth. Every gate is the HTTP one, so each refusal is compared with what HTTP answers.
function Same-Refusal($bridged, $http) { $bridged.subtype -eq 3 -and (Is-BridgedReply $bridged) -and $bridged.status -eq $http.status -and "$($bridged.meta.errcode)" -eq "$($http.json.errcode)" -and $http.status -ge 400 }
function Is-Uiaa-Challenge($p) { $p.subtype -eq 3 -and (Is-BridgedReply $p) -and $p.status -eq 401 -and $null -ne $p.body.flows -and "$($p.body.session)" -ne '' }
function Password-Auth($user, $password, $session) { @{ type = 'm.login.password'; session = $session; identifier = @{ type = 'm.id.user'; user = $user }; password = $password } }
function Login-Http-Device($user, $password) { Http POST '/_matrix/client/v3/login' @{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = $user }; password = $password } $null }

$db2 = "$S\e2e13db2"; Remove-Item -Recurse -Force $db2 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db2 | Out-Null
$cfg2 = Write-Config $db2 86400
$server = Start-Server $cfg2 's2'
$admin2 = Api Post '/_matrix/client/v3/register' '{"username":"root","password":"pw-pw-pw-pw","auth":{"type":"m.login.dummy"}}' $null

# ---- 0x10 Session: the read-only queries, anonymous ----
$anon = Ws-Open $null
$avail = Bridge $anon 0x10 0x21 @{ username = 'dave' } $null
$havail = Http GET '/_matrix/client/v3/register/available?username=dave' $null $null
$taken = Bridge $anon 0x10 0x21 @{ username = 'system' } $null
$htaken = Http GET '/_matrix/client/v3/register/available?username=system' $null $null
Check '[2.1] UsernameAvailable: a free name and the server user''s name, anonymously, as over HTTP' `
  ((Same-As-Http $avail $havail) -and $avail.body.available -eq $true -and (Same-Refusal $taken $htaken) -and $taken.meta.errcode -eq 'M_USER_IN_USE') `
  "free=$($avail.text) system=$($taken.metaText) http=$($htaken.status) $($htaken.text)"

$types = Bridge $anon 0x10 0x23 $null $null
$htypes = Http GET '/_matrix/client/v3/login' $null $null
Check '[2.2] LoginTypes anonymously, as over HTTP' ((Same-As-Http $types $htypes) -and @($types.body.flows).Count -gt 0) "bridge=$($types.text)"

$validity = Bridge $anon 0x10 0x22 @{ token = 'nothing-configured' } $null
$hvalidity = Http GET '/_matrix/client/v1/register/m.login.registration_token/validity?token=nothing-configured' $null $null
Check '[2.3] RegistrationTokenValidity is reached by its v1 path and answers as over HTTP' `
  ((Same-As-Http $validity $hvalidity) -or (Same-Refusal $validity $hvalidity)) "bridge=$($validity.status) $($validity.metaText) $($validity.text) http=$($hvalidity.status) $($hvalidity.text)"

# ---- Registration: UIAA, then the native Login ----
$round1 = Bridge $anon 0x10 0x20 $null @{ username = 'dave'; password = 'pw-dave-1'; inhibit_login = $true }
$hround1 = Http POST '/_matrix/client/v3/register' @{ username = 'dave-http'; password = 'pw-dave-1'; inhibit_login = $true } $null
Check '[2.4] Register without auth: the UIAA challenge (flows, session) arrives in data, the same flows HTTP offers' `
  ((Is-Uiaa-Challenge $round1) -and $hround1.status -eq 401 -and (Canon $round1.body.flows) -eq (Canon $hround1.json.flows)) `
  "bridge=$($round1.metaText) $($round1.text) http=$($hround1.status) $($hround1.text)"

$round2 = Bridge $anon 0x10 0x20 $null @{ username = 'dave'; password = 'pw-dave-1'; inhibit_login = $true; auth = @{ type = 'm.login.dummy'; session = $round1.body.session } }
Check '[2.5] Register with auth and inhibit_login: the account exists, no token was minted' `
  ((Is-Ack $round2) -and $round2.body.user_id -eq '@dave:localhost' -and $null -eq $round2.body.access_token) "bridge=$($round2.text)"

$login = Call $anon (Json-Pack 16 1 0 900 @{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = 'dave' }; password = 'pw-dave-1' } @())
$whoDave = Bridge $anon 0x11 0x20 $null $null
Check '[2.6] the native Login on the same connection makes it the new account' `
  ($login.subtype -eq 2 -and $login.meta.user_id -eq '@dave:localhost' -and (Is-Ack $whoDave) -and $whoDave.body.user_id -eq '@dave:localhost') `
  "login=$($login.metaText) whoami=$($whoDave.text)"

$daveDevice = $login.meta.device_id; $daveToken = $login.meta.access_token
$held = @(Ws-Open $daveToken; Ws-Open $daveToken; Ws-Open $daveToken)
$anon2 = Ws-Open $null
$over = Call $anon2 (Json-Pack 16 1 0 901 @{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = 'dave' }; password = 'pw-dave-1'; device_id = $daveDevice } @())
Check '[2.7] after registering, a Login past the device''s connection limit is still TooManyConnections' `
  ($over.subtype -eq 3 -and $over.meta.code_id -eq 1402) "reply=$($over.metaText)"
$held | ForEach-Object { $_.Dispose() }; $anon2.Dispose(); $anon.Dispose()

$anon3 = Ws-Open $null
$systemReg = Bridge $anon3 0x10 0x20 $null @{ username = 'system'; password = 'pw-pw-pw-pw'; inhibit_login = $true }
$hsystemReg = Http POST '/_matrix/client/v3/register' @{ username = 'system'; password = 'pw-pw-pw-pw'; inhibit_login = $true } $null
Check '[2.8] nobody registers "system" through the bridge either' ((Same-Refusal $systemReg $hsystemReg) -and $systemReg.meta.errcode -eq 'M_USER_IN_USE') `
  "bridge=$($systemReg.metaText) http=$($hsystemReg.status) $($hsystemReg.text)"
$anon3.Dispose()

# ---- UIAA: devices, password ----
$regE = Api Post '/_matrix/client/v3/register' '{"username":"erin","password":"pw-erin-1","auth":{"type":"m.login.dummy"}}' $null
$wsE = Ws-Open $regE.access_token
$e2 = Login-Http-Device 'erin' 'pw-erin-1'; $e3 = Login-Http-Device 'erin' 'pw-erin-1'; $e4 = Login-Http-Device 'erin' 'pw-erin-1'

$del1 = Bridge $wsE 0x16 0x23 @{ device_id = $e2.json.device_id } @{}
$hdel1 = Http DELETE "/_matrix/client/v3/devices/$(Enc $e3.json.device_id)" @{} $regE.access_token
$delWrong = Bridge $wsE 0x16 0x23 @{ device_id = $e2.json.device_id } @{ auth = (Password-Auth 'erin' 'not-the-password' $del1.body.session) }
Check '[2.9] DeleteDevice: the challenge offers what HTTP offers; a wrong password is M_FORBIDDEN and keeps the session' `
  ((Is-Uiaa-Challenge $del1) -and $hdel1.status -eq 401 -and (Canon $del1.body.flows) -eq (Canon $hdel1.json.flows) -and $delWrong.status -eq 401 -and $delWrong.meta.errcode -eq 'M_FORBIDDEN' -and $delWrong.body.session -eq $del1.body.session) `
  "challenge=$($del1.text) wrong=$($delWrong.metaText) $($delWrong.text)"

$delOk = Bridge $wsE 0x16 0x23 @{ device_id = $e2.json.device_id } @{ auth = (Password-Auth 'erin' 'pw-erin-1' $del1.body.session) }
$afterDel = Bridge $wsE 0x16 0x20 $null $null
Check '[2.10] DeleteDevice with the password: gone from ListDevices' `
  ((Is-Ack $delOk) -and (Is-Ack $afterDel) -and -not (@($afterDel.body.devices | ForEach-Object { $_.device_id }) -contains $e2.json.device_id)) `
  "delete=$($delOk.status) devices=$(@($afterDel.body.devices | ForEach-Object { $_.device_id }) -join ',')"

$dels1 = Bridge $wsE 0x16 0x24 $null @{ devices = @($e3.json.device_id) }
$delsOk = Bridge $wsE 0x16 0x24 $null @{ devices = @($e3.json.device_id); auth = (Password-Auth 'erin' 'pw-erin-1' $dels1.body.session) }
$e3Who = Http GET '/_matrix/client/v3/account/whoami' $null $e3.json.access_token
Check '[2.11] DeleteDevices: challenge, then the device and its token are gone' `
  ((Is-Uiaa-Challenge $dels1) -and (Is-Ack $delsOk) -and $e3Who.status -eq 401) "delete=$($delsOk.status) $($delsOk.text) e3=$($e3Who.status)"

$pw1 = Bridge $wsE 0x11 0x2C $null @{ new_password = 'pw-erin-2'; logout_devices = $true }
$pwOk = Bridge $wsE 0x11 0x2C $null @{ new_password = 'pw-erin-2'; logout_devices = $true; auth = (Password-Auth 'erin' 'pw-erin-1' $pw1.body.session) }
$e4Who = Http GET '/_matrix/client/v3/account/whoami' $null $e4.json.access_token
$stillMe = Bridge $wsE 0x11 0x20 $null $null
$newLogin = Login-Http-Device 'erin' 'pw-erin-2'
Check '[2.12] ChangePassword with logout_devices: other devices logged out, this connection kept, the new password works' `
  ((Is-Uiaa-Challenge $pw1) -and (Is-Ack $pwOk) -and $e4Who.status -eq 401 -and (Is-Ack $stillMe) -and $newLogin.status -eq 200) `
  "change=$($pwOk.status) e4=$($e4Who.status) whoami=$($stillMe.status) login=$($newLogin.status)"

$own1 = Bridge $wsE 0x16 0x23 @{ device_id = $regE.device_id } @{}
$ownOk = Bridge $wsE 0x16 0x23 @{ device_id = $regE.device_id } @{ auth = (Password-Auth 'erin' 'pw-erin-2' $own1.body.session) }
$afterOwn = Bridge $wsE 0x11 0x20 $null $null
Check '[2.13] deleting this connection''s own device: the reply arrives, the next pack is refused before the bridge' `
  ((Is-Ack $ownOk) -and $afterOwn.subtype -eq 3 -and ($afterOwn.flags -band $IS_BRIDGED) -eq 0 -and $afterOwn.meta.errcode -eq 'M_UNKNOWN_TOKEN') `
  "delete=$($ownOk.status) next=$($afterOwn.metaText)"
$wsE.Dispose()

$regF = Api Post '/_matrix/client/v3/register' '{"username":"frank","password":"pw-frank-1","auth":{"type":"m.login.dummy"}}' $null
$wsF = Ws-Open $regF.access_token
$de1 = Bridge $wsF 0x11 0x2D $null @{}
$hde1 = Http POST '/_matrix/client/v3/account/deactivate' @{} $regF.access_token
$deOk = Bridge $wsF 0x11 0x2D $null @{ auth = (Password-Auth 'frank' 'pw-frank-1' $de1.body.session) }
$afterDe = Bridge $wsF 0x11 0x20 $null $null
$frankLogin = Login-Http-Device 'frank' 'pw-frank-1'
Check '[2.14] Deactivate: the same challenge as HTTP, then the account is gone: the next pack refused, no login' `
  ((Is-Uiaa-Challenge $de1) -and (Canon $de1.body.flows) -eq (Canon $hde1.json.flows) -and (Is-Ack $deOk) -and $afterDe.subtype -eq 3 -and ($afterDe.flags -band $IS_BRIDGED) -eq 0 -and $frankLogin.status -ne 200) `
  "deactivate=$($deOk.status) $($deOk.text) next=$($afterDe.metaText) login=$($frankLogin.status) $($frankLogin.text)"
$wsF.Dispose()
Stop-Server $server

# ---- The registration gates: a token, a forbidden name, registration closed ----
$db3 = "$S\e2e13db3"; Remove-Item -Recurse -Force $db3 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db3 | Out-Null
$cfg3 = Write-Config $db3 86400 0 0 0 @('registration_token = "sekrit-e2e13"', 'forbidden_usernames = ["^bad"]')
$server = Start-Server $cfg3 's2-token'
$anon4 = Ws-Open $null
$valid = Bridge $anon4 0x10 0x22 @{ token = 'sekrit-e2e13' } $null
$hvalid = Http GET '/_matrix/client/v1/register/m.login.registration_token/validity?token=sekrit-e2e13' $null $null
$invalid = Bridge $anon4 0x10 0x22 @{ token = 'wrong' } $null
Check '[2.15] RegistrationTokenValidity: the configured token is valid, another is not, as over HTTP' `
  ((Same-As-Http $valid $hvalid) -and $valid.body.valid -eq $true -and (Is-Ack $invalid) -and $invalid.body.valid -eq $false) "valid=$($valid.text) invalid=$($invalid.text)"

$t1 = Bridge $anon4 0x10 0x20 $null @{ username = 'gina'; password = 'pw-gina-1'; inhibit_login = $true }
$ht1 = Http POST '/_matrix/client/v3/register' @{ username = 'gina-http'; password = 'pw-gina-1'; inhibit_login = $true } $null
$tWrong = Bridge $anon4 0x10 0x20 $null @{ username = 'gina'; password = 'pw-gina-1'; inhibit_login = $true; auth = @{ type = 'm.login.registration_token'; token = 'wrong'; session = $t1.body.session } }
$htWrong = Http POST '/_matrix/client/v3/register' @{ username = 'gina-http'; password = 'pw-gina-1'; inhibit_login = $true; auth = @{ type = 'm.login.registration_token'; token = 'wrong'; session = $ht1.json.session } } $null
$tOk = Bridge $anon4 0x10 0x20 $null @{ username = 'gina'; password = 'pw-gina-1'; inhibit_login = $true; auth = @{ type = 'm.login.registration_token'; token = 'sekrit-e2e13'; session = $t1.body.session } }
Check '[2.16] with a registration token required: the same flows as HTTP, a wrong token refused as HTTP refuses it, the right one registers' `
  ((Is-Uiaa-Challenge $t1) -and (Canon $t1.body.flows) -eq (Canon $ht1.json.flows) -and $tWrong.subtype -eq 3 -and $tWrong.status -eq $htWrong.status -and "$($tWrong.meta.errcode)" -eq "$($htWrong.json.errcode)" -and (Is-Ack $tOk) -and $tOk.body.user_id -eq '@gina:localhost') `
  "flows=$($t1.text) wrong=$($tWrong.status) $($tWrong.metaText) http=$($htWrong.status) $($htWrong.text) ok=$($tOk.text)"

$bad = Bridge $anon4 0x10 0x20 $null @{ username = 'badguy'; password = 'pw-pw-pw-pw'; inhibit_login = $true }
$hbad = Http POST '/_matrix/client/v3/register' @{ username = 'badguy'; password = 'pw-pw-pw-pw'; inhibit_login = $true } $null
Check '[2.17] a forbidden username is refused through the bridge as over HTTP' (Same-Refusal $bad $hbad) "bridge=$($bad.metaText) http=$($hbad.status) $($hbad.text)"
$anon4.Dispose()
Stop-Server $server

$db4 = "$S\e2e13db4"; Remove-Item -Recurse -Force $db4 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db4 | Out-Null
$cfg4 = Write-Config $db4 86400
(Get-Content $cfg4 -Raw).Replace('allow_registration = true', 'allow_registration = false') | Set-Content -Path $cfg4 -Encoding ascii
$server = Start-Server $cfg4 's2-closed'
$anon5 = Ws-Open $null
$closed = Bridge $anon5 0x10 0x20 $null @{ username = 'henry'; password = 'pw-pw-pw-pw'; inhibit_login = $true }
$hclosed = Http POST '/_matrix/client/v3/register' @{ username = 'henry'; password = 'pw-pw-pw-pw'; inhibit_login = $true } $null
Check '[2.18] with registration closed, the bridge refuses it exactly as HTTP does' (Same-Refusal $closed $hclosed) "bridge=$($closed.metaText) http=$($hclosed.status) $($hclosed.text)"
$anon5.Dispose()
Stop-Server $server

Log "################ RESULT: pass=$($script:Pass) fail=$($script:Fail) ################"
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
