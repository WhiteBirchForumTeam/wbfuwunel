# The wbf WebSocket channel (GET /_wbf/v1/ws): Hello, Ping, one pack per binary message, uploads resumed across HTTP
# and WS, idle timeout. Design: docs/design/wbf-wire-format.md §6.1. Same logging style as e2e6.
$ErrorActionPreference = 'Continue'
# Everything a run writes goes under target/, never next to the scripts.
$REPO = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path.TrimEnd('\')
$S   = Join-Path $REPO 'target\e2e-runs'
New-Item -ItemType Directory -Force $S | Out-Null
$EXE = if ($env:E2E_EXE) { $env:E2E_EXE } else { Join-Path $REPO 'target\e2e\tuwunel.exe' }
$OUT = "$S\e2e7-out"
$B   = 'http://127.0.0.1:8015'
$RESULT = "$OUT\results.txt"
New-Item -ItemType Directory -Force $OUT | Out-Null
'' | Out-File $RESULT -Encoding utf8
function Log($m) { $m | Out-File $RESULT -Append -Encoding utf8; Write-Host $m }

# ---------- CRC-32C (Castagnoli, reflected 0x82F63B78) ----------
# PowerShell integer literals are int32; do the arithmetic in uint64 and mask.
$script:CrcPoly = [uint64]0x82F63B78L
$script:CrcTable = New-Object uint64[] 256
for ($i = 0; $i -lt 256; $i++) {
  [uint64]$c = $i
  for ($k = 0; $k -lt 8; $k++) { if ($c -band 1) { $c = (($c -shr 1) -bxor $script:CrcPoly) -band 0xFFFFFFFFL } else { $c = $c -shr 1 } }
  $script:CrcTable[$i] = $c
}
function Crc32c([byte[]]$bytes, [int]$start, [int]$len) {
  [uint64]$c = 0xFFFFFFFFL
  for ($i = $start; $i -lt $start + $len; $i++) { $c = ($script:CrcTable[($c -bxor [uint64]$bytes[$i]) -band 0xFF] -bxor ($c -shr 8)) -band 0xFFFFFFFFL }
  return [uint32](($c -bxor 0xFFFFFFFFL) -band 0xFFFFFFFFL)
}
function BE32([uint32]$v) { $b = [BitConverter]::GetBytes($v); [Array]::Reverse($b); $b }
function BE64([uint64]$v) { $b = [BitConverter]::GetBytes($v); [Array]::Reverse($b); $b }
function RdBE32([byte[]]$b, [int]$at) { [uint32]((([uint64]$b[$at]) -shl 24) -bor (([uint64]$b[$at+1]) -shl 16) -bor (([uint64]$b[$at+2]) -shl 8) -bor [uint64]$b[$at+3]) }
function RdBE64([byte[]]$b, [int]$at) { [uint64]$v = 0; for ($i = 0; $i -lt 8; $i++) { $v = ($v -shl 8) -bor $b[$at+$i] }; $v }

# ---------- pack encode / decode ----------
# kinds: Control=1 Upload=3 Download=4 ; flags: META_ENCRYPTED=1 WANT_ACK=2 IS_RESPONSE=4
function New-Pack([byte]$kind, [byte]$subtype, [byte]$flags, [uint64]$id, [uint32]$seq, [byte[]]$meta, [byte[]]$data) {
  if ($null -eq $meta) { $meta = @() }; if ($null -eq $data) { $data = @() }
  $ms = New-Object System.IO.MemoryStream
  $ms.WriteByte(1); $ms.WriteByte($kind); $ms.WriteByte($subtype); $ms.WriteByte($flags)
  $ms.Write((BE64 $id), 0, 8); $ms.Write((BE32 $seq), 0, 4)
  $ms.Write((BE32 ([uint32]$meta.Length)), 0, 4); if ($meta.Length) { $ms.Write($meta, 0, $meta.Length) }
  $sofar = $ms.ToArray(); $ms.Write((BE32 (Crc32c $sofar 0 $sofar.Length)), 0, 4)
  $ms.Write((BE32 ([uint32]$data.Length)), 0, 4); if ($data.Length) { $ms.Write($data, 0, $data.Length) }
  $ms.Write((BE32 (Crc32c $data 0 $data.Length)), 0, 4)
  $ms.ToArray()
}
function Json-Pack([byte]$kind, [byte]$subtype, [uint64]$id, [uint32]$seq, $obj, [byte[]]$data) {
  $meta = if ($null -ne $obj) { [Text.Encoding]::UTF8.GetBytes(($obj | ConvertTo-Json -Compress)) } else { @() }
  New-Pack $kind $subtype 0 $id $seq $meta $data
}
function FileInfo-Meta([uint64]$fileSize, [uint32]$chunkSize, [uint32]$chunkCount) { [byte[]]((BE64 $fileSize) + (BE32 $chunkSize) + (BE32 $chunkCount)) }
function Create-Pack([uint32]$seq, [uint64]$fileSize, [uint32]$chunkSize, [uint32]$chunkCount, [byte[]]$desc) { New-Pack 3 1 0 0 $seq (FileInfo-Meta $fileSize $chunkSize $chunkCount) $desc }
function Read-Pack([byte[]]$b) {
  if ($b.Length -lt 32) { return @{ error = "too short ($($b.Length))" } }
  $mlen = RdBE32 $b 16; $meta = $b[20..(20+$mlen-1)]; if ($mlen -eq 0) { $meta = @() }
  $dlenAt = 20 + $mlen + 4; $dlen = RdBE32 $b $dlenAt; $dstart = $dlenAt + 4
  $data = if ($dlen -gt 0) { $b[$dstart..($dstart+$dlen-1)] } else { @() }
  $metaText = if ($mlen -gt 0) { [Text.Encoding]::UTF8.GetString($meta) } else { '' }
  $metaObj = $null; if ($metaText) { try { $metaObj = $metaText | ConvertFrom-Json } catch {} }
  @{ version = $b[0]; kind = $b[1]; subtype = $b[2]; flags = $b[3]; id = (RdBE64 $b 4); seq = (RdBE32 $b 12); meta = $metaObj; metaText = $metaText; data = $data;
     metaCrcOk = ((RdBE32 $b (20+$mlen)) -eq (Crc32c $b 0 (20+$mlen))); dataCrcOk = ((RdBE32 $b ($dstart+$dlen)) -eq (Crc32c $data 0 $data.Length)) }
}

# ---------- HTTP ----------
Add-Type -AssemblyName System.Net.Http
$script:Http = New-Object System.Net.Http.HttpClient
function Send-Pack([byte[]]$pack, $tok) {
  $req = New-Object System.Net.Http.HttpRequestMessage ([System.Net.Http.HttpMethod]::Post, "$B/_wbf/v1/pack")
  if ($tok) { $req.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $tok) }
  $req.Content = New-Object System.Net.Http.ByteArrayContent (,$pack)
  $req.Content.Headers.ContentType = New-Object System.Net.Http.Headers.MediaTypeHeaderValue('application/octet-stream')
  $resp = $script:Http.SendAsync($req).Result
  $bytes = $resp.Content.ReadAsByteArrayAsync().Result
  $p = Read-Pack $bytes; $p.http = [int]$resp.StatusCode; $p
}
function Describe($p) { $k = switch ($p.subtype) { 2 {'Ack'} 3 {'Error'} 5 {'Pong'} default {"sub$($p.subtype)"} }; "$k http=$($p.http) id=$($p.id) seq=$($p.seq) meta=$($p.metaText) data=$($p.data.Length)B" }

function Api($method, $path, $body, $tok) {
  $h = @{}; if ($tok) { $h['Authorization'] = "Bearer $tok" }
  $args = @{ Method = $method; Uri = "$B$path"; Headers = $h; TimeoutSec = 15; UseBasicParsing = $true }
  if ($null -ne $body) { $args['Body'] = $body; $args['ContentType'] = 'application/json' }
  try { (Invoke-WebRequest @args).Content | ConvertFrom-Json } catch { Log "  !! FAILED $method $path : $($_.Exception.Message)"; $null }
}
function Get-Bytes($mxc, $tok) {
  $id = $mxc -replace '^mxc://localhost/', ''
  $req = New-Object System.Net.Http.HttpRequestMessage ([System.Net.Http.HttpMethod]::Get, "$B/_matrix/client/v1/media/download/localhost/$id")
  $req.Headers.Authorization = New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $tok)
  $resp = $script:Http.SendAsync($req).Result; @{ status = [int]$resp.StatusCode; bytes = $resp.Content.ReadAsByteArrayAsync().Result }
}

function Write-Config([string]$db, [int]$uploadTtl, [long]$maxLen = 0) {
  $cfg = "$S\e2e7.toml"
  @('[global]','server_name = "localhost"',('database_path = "' + ($db -replace '\\','/') + '"'),'port = 8015','address = ["127.0.0.1"]',
    'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
    ('media_upload_ttl = ' + $uploadTtl),('media_upload_max_len = ' + $maxLen),'log = "info"') -join "`n" | Set-Content -Path $cfg -Encoding ascii
  $cfg
}
function Start-Server([string]$cfg, [string]$tag) {
  $p = Start-Process -FilePath $EXE -ArgumentList @('-c', $cfg) -PassThru -NoNewWindow -RedirectStandardOutput "$OUT\$tag.out" -RedirectStandardError "$OUT\$tag.err"
  for ($i = 0; $i -lt 40; $i++) { Start-Sleep -Milliseconds 500; try { $null = Invoke-WebRequest -Uri "$B/_matrix/client/versions" -TimeoutSec 2 -UseBasicParsing; return $p } catch {} }
  throw "server did not come up ($tag)"
}
function Stop-Server($p) { Start-Sleep -Seconds 3; if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force }; Start-Sleep -Seconds 2 }
function Exec([string]$cfg, [string[]]$cmds, [string]$tag) {
  $cmds = $cmds + @('server shutdown'); $extra = @(); foreach ($c in $cmds) { $extra += @('--execute', ('"' + $c + '"')) }
  $p = Start-Process -FilePath $EXE -ArgumentList (@('-c', $cfg) + $extra) -PassThru -NoNewWindow -RedirectStandardOutput "$OUT\$tag.out" -RedirectStandardError "$OUT\$tag.err"
  for ($i = 0; $i -lt 240; $i++) { Start-Sleep -Milliseconds 500; if ($p.HasExited) { break } }
  Stop-Server $p
  ((Get-Content "$OUT\$tag.out","$OUT\$tag.err" -Raw) -replace "`e\[[0-9;]*m", '' -split "`n") | Where-Object { $_ -match 'reference|deleted|rror|panicked|Sweeping' } | ForEach-Object { $_ -replace '^\S+\s+', '' }
}

# ---------- WebSocket ----------
function Ws-Open($tok) {
  $ws = New-Object System.Net.WebSockets.ClientWebSocket
  if ($tok) { $ws.Options.SetRequestHeader('Authorization', "Bearer $tok") }
  $ws.ConnectAsync([Uri]'ws://127.0.0.1:8015/_wbf/v1/ws', [Threading.CancellationToken]::None).Wait()
  $ws
}
function Ws-Send($ws, [byte[]]$pack) {
  $ws.SendAsync([ArraySegment[byte]]$pack, [System.Net.WebSockets.WebSocketMessageType]::Binary, $true, [Threading.CancellationToken]::None).Wait()
}
function Ws-SendText($ws, [string]$text) {
  $b = [Text.Encoding]::UTF8.GetBytes($text)
  $ws.SendAsync([ArraySegment[byte]]$b, [System.Net.WebSockets.WebSocketMessageType]::Text, $true, [Threading.CancellationToken]::None).Wait()
}
function Ws-Recv($ws) {
  $ms = New-Object System.IO.MemoryStream; $buf = New-Object byte[] 262144
  do { $r = $ws.ReceiveAsync([ArraySegment[byte]]$buf, [Threading.CancellationToken]::None).Result; $ms.Write($buf, 0, $r.Count) } while (-not $r.EndOfMessage)
  $p = Read-Pack ($ms.ToArray()); $p.http = 'ws'; $p
}
function Ws-Call($ws, [byte[]]$pack) { Ws-Send $ws $pack; Ws-Recv $ws }

# ================= Scenario 1: the WebSocket channel =================
$db1 = "$S\e2e7db-1"; Remove-Item -Recurse -Force $db1 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db1 | Out-Null
$cfg = Write-Config $db1 86400
Log '################ Scenario 1: GET /_wbf/v1/ws ################'
$p = Start-Server $cfg 's1'
$reg = Api Post '/_matrix/client/v3/register' '{"username":"e2e","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tok = $reg.access_token; Log "user = $($reg.user_id)"

# Since the Session kind (wire-format 6.3) an upgrade without a token is allowed: the connection may only Hello, Ping,
# Login or Refresh until it logs in, and is closed after wbf_ws_unauthenticated_timeout. Scenario 4 covers it.
try { $anon = Ws-Open $null; $r = Ws-Call $anon (New-Pack 3 1 0 0 1 (FileInfo-Meta 16 16 1) @()); Log "[1.0] no token -> connected state=$($anon.State); Upload/Create -> $(Describe $r)  (expect Open, Error Unauthorized)"; try { $anon.CloseAsync([System.Net.WebSockets.WebSocketCloseStatus]::NormalClosure, 'bye', [Threading.CancellationToken]::None).Wait(3000) | Out-Null } catch {} } catch { Log "[1.0] no token -> upgrade refused: $($_.Exception.InnerException.Message)  (expect Open: FAIL)" }
try { $badTok = Ws-Open 'not-a-token'; Log "[1.0b] wrong token -> connected?! state=$($badTok.State)  (expect refused: FAIL)" } catch { Log "[1.0b] wrong token -> upgrade refused: $($_.Exception.InnerException.Message)  (expect 401)" }

$ws = Ws-Open $tok
Log "[1.1] connected state=$($ws.State)  (expect Open)"
$r = Ws-Call $ws (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e7.ps1'; features = @() } @())
Log "[1.2] Hello -> $(Describe $r)  (expect Ack protocol=1 server=localhost features)"
$r = Ws-Call $ws (New-Pack 1 4 0 0 2 ([Text.Encoding]::UTF8.GetBytes('{"nonce":42}')) @())
Log "[1.3] Ping -> $(Describe $r)  (expect Pong nonce echoed)"
Ws-SendText $ws 'hello?'; $r = Ws-Recv $ws
Log "[1.4] text frame -> $(Describe $r)  (expect Error Corrupt: text frames are not packs)"

# two uploads interleaved on one connection
$wire = 65552
$descA = [Text.Encoding]::UTF8.GetBytes('DESC-A'); $descB = [Text.Encoding]::UTF8.GetBytes('DESC-B')
$fileA = New-Object byte[] (2*$wire + 1000); (New-Object Random 1).NextBytes($fileA)
$fileB = New-Object byte[] (16400*2 + 300); (New-Object Random 2).NextBytes($fileB)
$r = Ws-Call $ws (Create-Pack 3 132056 65536 3 $descA); $idA = [uint64]$r.meta.id; $mxcA = $r.meta.mxc
Log "[1.5] Create A -> $(Describe $r)"
$r = Ws-Call $ws (Create-Pack 4 33000 16384 3 $descB); $idB = [uint64]$r.meta.id; $mxcB = $r.meta.mxc
Log "[1.6] Create B -> $(Describe $r)"
$r = Ws-Call $ws (New-Pack 3 2 0 $idA 0 @() ([byte[]]$fileA[0..($wire-1)]))
Log "[1.7] A chunk 0 -> $(Describe $r)  (expect received=1)"
$r = Ws-Call $ws (New-Pack 3 2 0 $idB 0 @() ([byte[]]$fileB[0..16399]))
Log "[1.8] B chunk 0 -> $(Describe $r)  (expect received=1)"
$r = Ws-Call $ws (New-Pack 3 2 0 $idA 2 @() ([byte[]]$fileA[(2*$wire)..(2*$wire+999)]))
Log "[1.9] A chunk 2 before 1 -> $(Describe $r)  (expect Error OutOfOrder expected_seq=1, answered from the connection table)"
$r = Ws-Call $ws (New-Pack 3 2 0 $idA 1 @() ([byte[]]$fileA[$wire..(2*$wire-1)]))
Log "[1.10] A chunk 1 -> $(Describe $r)  (expect received=2)"
$r = Ws-Call $ws (New-Pack 3 2 0 $idB 1 @() ([byte[]]$fileB[16400..32799]))
Log "[1.11] B chunk 1 -> $(Describe $r)  (expect received=2)"
$r = Ws-Call $ws (New-Pack 3 2 0 $idA 0 @() ([byte[]]$fileA[0..($wire-1)]))
Log "[1.11b] A chunk 0 resent (lost ack) -> $(Describe $r)  (expect Ack received=2, idempotent; the next chunk must still be accepted)"
# pipelined: three packs sent before reading any reply, replies must come back in order
Ws-Send $ws (New-Pack 3 2 8 $idA 2 @() ([byte[]]$fileA[(2*$wire)..(2*$wire+999)]))
Ws-Send $ws (New-Pack 3 2 8 $idB 2 @() ([byte[]]$fileB[32800..33099]))
Ws-Send $ws (New-Pack 3 3 0 $idA 9 @() @())
$r1 = Ws-Recv $ws; $r2 = Ws-Recv $ws; $r3 = Ws-Recv $ws
Log "[1.12] pipelined: A last -> $(Describe $r1)  (expect received=3 finished=true)"
Log "[1.13] pipelined: B last -> $(Describe $r2)  (expect received=3 finished=true)"
Log "[1.14] pipelined: A Status -> $(Describe $r3)  (expect seq=9 finished=true)"
$r = Ws-Call $ws (New-Pack 3 4 0 $idA 10 @() @())
Log "[1.15] Seal A -> $(Describe $r)  (expect mxc)"
$r = Ws-Call $ws (New-Pack 3 4 0 $idB 11 @() @())
Log "[1.16] Seal B -> $(Describe $r)  (expect mxc)"
$dl = Get-Bytes $mxcA $tok
Log "[1.17] A standard download identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$dl.bytes, [byte[]]$fileA))  (expect True)"
$r = Ws-Call $ws (Json-Pack 4 1 0 12 @{ mxc = $mxcB } @())
Log "[1.18] Info B over WS -> $(Describe $r) desc=$([Text.Encoding]::UTF8.GetString($r.data))  (expect chunk_count=3, DESC-B)"
$r = Ws-Call $ws (Json-Pack 4 2 0 13 @{ mxc = $mxcB; pos = 16385 } @())
Log "[1.19] Read B pos=16385 over WS -> $(Describe $r) identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$r.data, [byte[]]$fileB[16400..32799]))  (expect chunk=1 len=16400, True)"
$bad = New-Pack 3 3 0 $idA 14 @() @(); $bad[5] = $bad[5] -bxor 1
$r = Ws-Call $ws $bad
Log "[1.20] header damaged -> $(Describe $r)  (expect Error Corrupt MetaCrc, id/seq 0)"

# half over HTTP, half over WS (resume across transports)
$fileC = New-Object byte[] (2*$wire); (New-Object Random 3).NextBytes($fileC)
$r = Send-Pack (Create-Pack 1 131072 65536 2 @()) $tok; $idC = [uint64]$r.meta.id; $mxcC = $r.meta.mxc
$r = Send-Pack (New-Pack 3 2 0 $idC 0 @() ([byte[]]$fileC[0..($wire-1)])) $tok
Log "[1.21] C chunk 0 over HTTP -> $(Describe $r)  (expect received=1)"
$r = Ws-Call $ws (New-Pack 3 3 0 $idC 15 @() @())
Log "[1.22] C Status over WS -> $(Describe $r)  (expect received=1)"
$r = Ws-Call $ws (New-Pack 3 2 8 $idC 1 @() ([byte[]]$fileC[$wire..(2*$wire-1)]))
Log "[1.23] C chunk 1 over WS -> $(Describe $r)  (expect received=2 finished=true)"
$r = Ws-Call $ws (New-Pack 3 4 0 $idC 16 @() @())
$dl = Get-Bytes $mxcC $tok
Log "[1.24] Seal C over WS, download identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$dl.bytes, [byte[]]$fileC))  (expect True)"

# WS first, HTTP in the middle, WS last: the connection must not remember anything the row does not
$fileD = New-Object byte[] (2*$wire + 500); (New-Object Random 4).NextBytes($fileD)
$r = Ws-Call $ws (Create-Pack 17 131556 65536 3 @()); $idD = [uint64]$r.meta.id; $mxcD = $r.meta.mxc
$r = Ws-Call $ws (New-Pack 3 2 0 $idD 0 @() ([byte[]]$fileD[0..($wire-1)]))
Log "[1.26] D chunk 0 over WS -> $(Describe $r)  (expect received=1)"
$r = Send-Pack (New-Pack 3 2 0 $idD 1 @() ([byte[]]$fileD[$wire..(2*$wire-1)])) $tok
Log "[1.27] D chunk 1 over HTTP -> $(Describe $r)  (expect received=2)"
$r = Ws-Call $ws (New-Pack 3 2 8 $idD 2 @() ([byte[]]$fileD[(2*$wire)..(2*$wire+499)]))
Log "[1.28] D chunk 2 over WS -> $(Describe $r)  (expect received=3 finished=true: no stale connection state)"
$r = Ws-Call $ws (New-Pack 3 4 0 $idD 18 @() @())
$dl = Get-Bytes $mxcD $tok
Log "[1.29] Seal D, download identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$dl.bytes, [byte[]]$fileD))  (expect True)"

# oversized frame is refused by the socket layer
$huge = New-Pack 3 2 0 $idC 5 @() (New-Object byte[] (17*1024*1024))
try { Ws-Send $ws $huge; $r = Ws-Recv $ws; Log "[1.25] 17 MiB frame -> $(Describe $r) state=$($ws.State)" } catch { Log "[1.25] 17 MiB frame -> connection dropped: $($_.Exception.InnerException.Message)  (expect refused/closed)" }
try { $ws.CloseAsync([System.Net.WebSockets.WebSocketCloseStatus]::NormalClosure, 'bye', [Threading.CancellationToken]::None).Wait(3000) | Out-Null } catch {}
Stop-Server $p
# ================= Scenario 2: idle timeout =================
$cfg = Write-Config $db1 86400; Add-Content -Path $cfg -Value 'wbf_ws_idle_timeout = 2' -Encoding ascii
$p = Start-Server $cfg 's2'
$ws = Ws-Open $tok
$r = Ws-Call $ws (New-Pack 1 4 0 0 1 @() @())
Log "[2.1] ping right after connect -> $(Describe $r)  (expect Pong)"
$buf = New-Object byte[] 4096
$t = $ws.ReceiveAsync([ArraySegment[byte]]$buf, [Threading.CancellationToken]::None)
if ($t.Wait(8000)) { Log "[2.2] silent for 2 s -> server sent $($t.Result.MessageType) state=$($ws.State)  (expect Close)" } else { Log "[2.2] silent 8 s -> nothing from server, state=$($ws.State)  (expect Close: FAIL)" }
$ws2 = Ws-Open $tok
$r = Ws-Call $ws2 (New-Pack 3 3 0 $idD 3 @() @())
Log "[2.3] fresh connection, Status of sealed D -> $(Describe $r)  (expect NotFound: sealed; server still serving)"
try { $ws2.CloseAsync([System.Net.WebSockets.WebSocketCloseStatus]::NormalClosure, 'bye', [Threading.CancellationToken]::None).Wait(3000) | Out-Null } catch {}
Stop-Server $p
# ================= Scenario 3: the session behind the connection (review-followups 2.3 / 2.4) =================
# A locked account is refused on both transports; a logged-out token stops working at the next pack, not never;
# a connection still open at shutdown is closed by the server and the process exits without dangling references.
Log '################ Scenario 3: locked account, logout mid-connection, shutdown with a connection open ################'
$db3 = "$S\e2e7db-3"; Remove-Item -Recurse -Force $db3 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db3 | Out-Null
$cfg3 = "$S\e2e7-3.toml"
@('[global]','server_name = "localhost"',('database_path = "' + ($db3 -replace '\\','/') + '"'),'port = 8015','address = ["127.0.0.1"]',
  'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
  'wbf_ws_idle_timeout = 60','log = "info,tuwunel_api=debug,tuwunel_router=debug,tuwunel_service::services=debug"') -join "`n" | Set-Content -Path $cfg3 -Encoding ascii
$p = Start-Server $cfg3 's3'
$regA = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokA = $regA.access_token
$regB = Api Post '/_matrix/client/v3/register' '{"username":"bob","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokB = $regB.access_token
$ping = New-Pack 1 4 0 0 1 @() @()

# [3.1] locked account: HTTP pack and WS upgrade both refused; unlocking restores both
$null = Api Put "/_synapse/admin/v2/users/$([uri]::EscapeDataString('@bob:localhost'))" '{"locked":true}' $tokA
$r = Send-Pack $ping $tokB
Log "[3.1a] locked bob, HTTP Ping -> $(Describe $r)  (expect http=401 Error Unauthorized)"
try { $wsL = Ws-Open $tokB; Log "[3.1b] locked bob, WS upgrade -> connected?! state=$($wsL.State)  (expect refused: FAIL)" } catch { $inner = $_.Exception; while ($inner.InnerException) { $inner = $inner.InnerException }; Log "[3.1b] locked bob, WS upgrade refused: $($inner.Message)  (expect 401)" }
$null = Api Put "/_synapse/admin/v2/users/$([uri]::EscapeDataString('@bob:localhost'))" '{"locked":false}' $tokA
$r = Send-Pack $ping $tokB
Log "[3.1c] unlocked bob, HTTP Ping -> $(Describe $r)  (expect http=200 Pong)"

# [3.2] logout while connected: the next pack is refused and the server closes the connection
$wsB = Ws-Open $tokB
$r = Ws-Call $wsB $ping
Log "[3.2a] bob connected, Ping -> $(Describe $r)  (expect Pong)"
$null = Api Post '/_matrix/client/v3/logout' '{}' $tokB
$r = Ws-Call $wsB $ping
Log "[3.2b] after HTTP logout, Ping -> $(Describe $r)  (expect Error Unauthorized)"
$buf = New-Object byte[] 4096
$t = $wsB.ReceiveAsync([ArraySegment[byte]]$buf, [Threading.CancellationToken]::None)
if ($t.Wait(5000)) { Log "[3.2c] then server sent $($t.Result.MessageType) code=$($t.Result.CloseStatus) state=$($wsB.State)  (expect Close, PolicyViolation)" } else { Log "[3.2c] no close within 5 s, state=$($wsB.State)  (expect Close: FAIL)" }

# [3.3] shutdown with a connection open: the server closes it, the process exits, nothing dangles
$wsA = Ws-Open $tokA
$r = Ws-Call $wsA $ping
Log "[3.3a] alice connected, Ping -> $(Describe $r)  (expect Pong)"
$admins = (Api Post "/_matrix/client/v3/join/$([uri]::EscapeDataString('#admins:localhost'))" '{}' $tokA).room_id
$txn = [guid]::NewGuid().ToString('N')
$null = Api Put "/_matrix/client/v3/rooms/$([uri]::EscapeDataString($admins))/send/m.room.message/$txn" '{"msgtype":"m.text","body":"!admin server shutdown"}' $tokA
$t = $wsA.ReceiveAsync([ArraySegment[byte]]$buf, [Threading.CancellationToken]::None)
try {
  if ($t.Wait(15000)) { Log "[3.3b] shutdown ordered, server sent $($t.Result.MessageType) code=$($t.Result.CloseStatus) reason='$($t.Result.CloseStatusDescription)' state=$($wsA.State)  (expect Close, EndpointUnavailable = 1001 going away)" } else { Log "[3.3b] no close within 15 s, state=$($wsA.State)  (expect Close: FAIL)" }
} catch { $inner = $_.Exception; while ($inner.InnerException) { $inner = $inner.InnerException }; Log "[3.3b] receive failed: $($inner.GetType().Name): $($inner.Message) state=$($wsA.State)  (expect Close frame, not a failure: FAIL)" }
$exited = $p.WaitForExit(30000)
Log "[3.3c] process exited within 30 s: $exited  (expect True)"
# release_max_log_level strips debug! at compile time, so the probes are the info lines the connection tracker prints at shutdown.
$log3 = ((Get-Content "$OUT\s3.out","$OUT\s3.err" -EA SilentlyContinue) -join "`n") -replace "`e\[[0-9;]*m", ''
$waited = [bool]($log3 -match 'Waiting for long-lived connections to end')
$ended = [bool]($log3 -match 'Long-lived connections ended')
$abnormal = [bool]($log3 -match 'ended abnormally|panicked|dangling references')
Log "[3.3d] log: waited-for-connections=$waited ended=$ended abnormal=$abnormal  (expect True / True / False)"
if (-not $exited) { Stop-Server $p }

# ================= Scenario 4: the Session kind (wire-format 6.3): Login / Refresh / Logout over the channel =================
# Unauthenticated upgrade + login deadline (here 3 s), login by password, wrong password, the login throttle shared by
# HTTP and the channel, refresh, logout closes the connection, a second login switches accounts, logout-all ends the
# user's other sessions, a locked account cannot log in. Old-style script: each line prints its expected value.
Log '################ Scenario 4: Session kind over the WebSocket ################'
$db4 = "$S\e2e7db-4"; Remove-Item -Recurse -Force $db4 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db4 | Out-Null
$cfg4 = "$S\e2e7-4.toml"
@('[global]','server_name = "localhost"',('database_path = "' + ($db4 -replace '\\','/') + '"'),'port = 8015','address = ["127.0.0.1"]',
  'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
  'wbf_ws_idle_timeout = 60','wbf_ws_unauthenticated_timeout = 3','login_rc_per_second = 1','login_rc_burst_count = 4','log = "info"') -join "`n" | Set-Content -Path $cfg4 -Encoding ascii
$p = Start-Server $cfg4 's4'
$regA = Api Post '/_matrix/client/v3/register' '{"username":"alice","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tokA = $regA.access_token
$null = Api Post '/_matrix/client/v3/register' '{"username":"carol","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$ping = New-Pack 1 4 0 0 1 @() @()
$createPack = New-Pack 3 1 0 0 1 (FileInfo-Meta 65536 65536 1) @()
function Login-Pack($user, $password, [bool]$wantRefresh) {
  Json-Pack 16 1 0 1 @{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = $user }; password = $password; initial_device_display_name = 'e2e7'; refresh_token = $wantRefresh } @()
}
# A frame from the server: a pack, a close (server-initiated close, or an exception which also means the socket is gone),
# or a timeout. Never throws, so a closing connection reads as 'close' instead of blowing up the script.
function Recv-Frame($ws, [int]$ms) {
  $buf = New-Object byte[] 65536
  try {
    $t = $ws.ReceiveAsync([ArraySegment[byte]]$buf, [Threading.CancellationToken]::None)
    if (-not $t.Wait($ms)) { return @{ kind = 'timeout' } }
    $res = $t.Result
    if ($res.MessageType -eq [System.Net.WebSockets.WebSocketMessageType]::Close) { return @{ kind = 'close'; code = "$($res.CloseStatus)"; reason = $res.CloseStatusDescription } }
    $bytes = New-Object byte[] $res.Count; [Array]::Copy($buf, $bytes, $res.Count)
    $pk = Read-Pack $bytes; $pk.http = 'ws'; @{ kind = 'pack'; pack = $pk }
  } catch { @{ kind = 'close'; code = 'aborted'; reason = 'receive threw' } }
}
function Meta-Of($pack) { $m = $null; try { $m = $pack.metaText | ConvertFrom-Json } catch {}; $m }
# Raw HTTP /login that reports the status code (the Api helper turns non-2xx into $null).
function Login-Http($user, $password) {
  $req = New-Object System.Net.Http.HttpRequestMessage ([System.Net.Http.HttpMethod]::Post, "$B/_matrix/client/v3/login")
  $body = @{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = $user }; password = $password } | ConvertTo-Json -Compress
  $req.Content = New-Object System.Net.Http.StringContent ($body, [Text.Encoding]::UTF8, 'application/json')
  $resp = $script:Http.SendAsync($req).Result
  @{ status = [int]$resp.StatusCode }
}

# [4.1] unauthenticated upgrade: Hello and Ping answered, other kinds refused, closed at the deadline even while pinging
$anon = Ws-Open $null
$r = Ws-Call $anon (Json-Pack 1 1 0 1 @{ protocol = 1; client = 'e2e7.ps1'; features = @() } @())
Log "[4.1a] anonymous Hello -> $(Describe $r)  (expect Ack, features include login)"
$r = Ws-Call $anon $createPack
Log "[4.1b] anonymous Upload/Create -> $(Describe $r)  (expect Error Unauthorized)"
$closed = $null; $sw = [Diagnostics.Stopwatch]::StartNew()
while ($sw.Elapsed.TotalSeconds -lt 10 -and -not $closed) {
  Ws-Send $anon $ping
  $f = Recv-Frame $anon 1500
  if ($f.kind -eq 'close') { $closed = $f } else { Start-Sleep -Milliseconds 500 }
}
if ($closed) { Log "[4.1c] pinging without logging in: closed after $([math]::Round($sw.Elapsed.TotalSeconds,1)) s, code=$($closed.code)  (expect ~3 s, PolicyViolation)" } else { Log "[4.1c] pinging without logging in: still open after 10 s  (expect Close: FAIL)" }

# [4.2] login by password on a fresh anonymous connection; the session works on the channel and its token works over HTTP
$ws4 = Ws-Open $null
$r = Ws-Call $ws4 (Login-Pack 'alice' 'correct-horse-battery' $true)
$loginMeta = Meta-Of $r
Log "[4.2a] Login alice -> $(Describe $r)  (expect Ack with user_id, device_id, access_token, refresh_token)"
$tokWs = $loginMeta.access_token; $refreshTok = $loginMeta.refresh_token
$r = Ws-Call $ws4 $createPack
Log "[4.2b] after Login, Upload/Create -> $(Describe $r)  (expect Ack)"
$r = Send-Pack $ping $tokWs
Log "[4.2c] the channel-issued token over HTTP pack -> $(Describe $r)  (expect http=200 Pong)"

# [4.3] refresh: a new access token is issued and the connection continues as the same user
$r = Ws-Call $ws4 (Json-Pack 16 2 0 2 @{ refresh_token = $refreshTok } @())
$refreshMeta = Meta-Of $r
Log "[4.3a] Refresh -> $(Describe $r)  (expect Ack with a new access_token)"
$r = Send-Pack $ping $refreshMeta.access_token
Log "[4.3b] new access token over HTTP -> $(Describe $r)  (expect http=200 Pong)"
$r = Ws-Call $ws4 $ping
Log "[4.3c] the connection after Refresh, Ping -> $(Describe $r)  (expect Pong)"

# [4.4] wrong password is refused but keeps the connection; the bucket (burst 4) is shared with HTTP /login.
# The refresh above already spent bucket tokens, so the burst is close to empty; keep sending until RateLimited.
$r = Ws-Call $ws4 (Login-Pack 'alice' 'wrong' $false)
Log "[4.4a] wrong password -> $(Describe $r)  (expect Error Forbidden)"
$rl = $null
for ($i = 0; $i -lt 8 -and -not $rl; $i++) {
  $r = Ws-Call $ws4 (Login-Pack 'alice' 'wrong' $false)
  if ($r.metaText -match 'RateLimited') { $rl = $r }
}
Log "[4.4b] repeated attempts eventually rate-limited -> $(if ($rl) { Describe $rl } else { 'never rate-limited: FAIL' })  (expect Error RateLimited with retry_after_ms)"
$h = Login-Http 'alice' 'correct-horse-battery'
Log "[4.4c] HTTP /login from the same address while the bucket is empty -> http=$($h.status)  (expect 429: HTTP and channel share the bucket)"
$r = Ws-Call $ws4 $ping
Log "[4.4d] the connection is still alice after the refusals, Ping -> $(Describe $r)  (expect Pong)"
Start-Sleep -Seconds 6   # let the bucket refill before the next logins

# [4.5] a second Login on the same connection switches the account (no forced disconnect)
$r = Ws-Call $ws4 (Login-Pack 'carol' 'correct-horse-battery' $false)
$carolMeta = Meta-Of $r
Log "[4.5a] Login carol on alice's connection -> $(Describe $r)  (expect Ack user_id=@carol:localhost)"
$r = Ws-Call $ws4 $ping
Log "[4.5b] the same connection is now carol, Ping -> $(Describe $r)  (expect Pong)"

# [4.6] Logout answers then closes the connection (1000); the device's token is gone
$logoutWs = Ws-Open $null
$r = Ws-Call $logoutWs (Login-Pack 'carol' 'correct-horse-battery' $false)
$logoutMeta = Meta-Of $r
Ws-Send $logoutWs (Json-Pack 16 3 0 3 @{} @())
$ackF = Recv-Frame $logoutWs 5000; $closeF = Recv-Frame $logoutWs 5000
$ackDesc = if ($ackF.kind -eq 'pack') { Describe $ackF.pack } else { $ackF.kind }
Log "[4.6a] Logout -> $ackDesc then $($closeF.kind) code=$($closeF.code)  (expect Ack then close NormalClosure)"
$r = Send-Pack $ping $logoutMeta.access_token
Log "[4.6b] the logged-out device's token over HTTP -> $(Describe $r)  (expect http=401)"

# [4.7] logout-all: another of the user's connections is closed at its next message
$sessA = Ws-Open $null; $rA = Ws-Call $sessA (Login-Pack 'carol' 'correct-horse-battery' $false)
$sessB = Ws-Open $null; $rB = Ws-Call $sessB (Login-Pack 'carol' 'correct-horse-battery' $false)
$r = Ws-Call $sessB $ping
Log "[4.7a] carol on two connections (two devices), the second Pings -> $(Describe $r)  (expect Pong)"
Ws-Send $sessA (Json-Pack 16 3 0 4 @{ all = $true } @())
$null = Recv-Frame $sessA 5000; $null = Recv-Frame $sessA 5000
Ws-Send $sessB $ping
$f = Recv-Frame $sessB 5000
$fDesc = if ($f.kind -eq 'pack') { Describe $f.pack } else { "$($f.kind) code=$($f.code)" }
Log "[4.7b] after Logout all on the first, the second's next message -> $fDesc  (expect Error Unauthorized or a close)"

# [4.8] a locked account cannot log in over the channel (carol is locked by alice, the admin, then unlocked).
# The throttle is checked before the credentials, so let the bucket refill first or the lock is masked by RateLimited.
Start-Sleep -Seconds 6
# A refresh token minted before the lock (logout-all above removed carol's earlier ones with her devices).
$wsR = Ws-Open $null; $rR = Ws-Call $wsR (Login-Pack 'carol' 'correct-horse-battery' $true); $carolRefresh = (Meta-Of $rR).refresh_token
$null = Api Put "/_synapse/admin/v2/users/$([uri]::EscapeDataString('@carol:localhost'))" '{"locked":true}' $tokA
$wsL = Ws-Open $null
$r = Ws-Call $wsL (Login-Pack 'carol' 'correct-horse-battery' $false)
Log "[4.8a] locked carol, Login -> $(Describe $r)  (expect Error Unauthorized M_USER_LOCKED)"
$r = Ws-Call $wsL (Json-Pack 16 2 0 2 @{ refresh_token = $carolRefresh } @())
Log "[4.8b] locked carol, Refresh with a still-valid refresh token -> $(Describe $r)  (expect Error Unauthorized M_USER_LOCKED: a locked account may not mint tokens)"
$r = Ws-Call $wsL (Json-Pack 16 2 0 3 @{ refresh_token = 'refresh_nonsense' } @())
Log "[4.8c] Refresh with an unknown token -> $(Describe $r)  (expect Error Forbidden)"
$null = Api Put "/_synapse/admin/v2/users/$([uri]::EscapeDataString('@carol:localhost'))" '{"locked":false}' $tokA
Stop-Server $p

# ================= Scenario 5: the per-device connection limit (pack-pipeline 2.1) =================
# wbf_ws_max_connections_per_device = 2 here. A bearer upgrade past the limit is refused with 429 before any upgrade;
# a Login over the channel past the limit is refused with TooManyConnections and that connection is closed (1008);
# other devices are not affected; a closed connection gives its place back; logging in again on the same connection
# as the same device does not take a second place.
Log '################ Scenario 5: per-device connection limit ################'
$db5 = "$S\e2e7db-5"; Remove-Item -Recurse -Force $db5 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db5 | Out-Null
$cfg5 = "$S\e2e7-5.toml"
@('[global]','server_name = "localhost"',('database_path = "' + ($db5 -replace '\\','/') + '"'),'port = 8015','address = ["127.0.0.1"]',
  'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
  'wbf_ws_idle_timeout = 60','wbf_ws_unauthenticated_timeout = 30','wbf_ws_max_connections_per_device = 2','login_rc_per_second = 5','login_rc_burst_count = 40','log = "info"') -join "`n" | Set-Content -Path $cfg5 -Encoding ascii
$p = Start-Server $cfg5 's5'
$null = Api Post '/_matrix/client/v3/register' '{"username":"dave","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
function Login-Device-Http($user, $device) {
  (Api Post '/_matrix/client/v3/login' (@{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = $user }; password = 'correct-horse-battery'; device_id = $device } | ConvertTo-Json -Compress) $null).access_token
}
function Login-Device-Pack($user, $device, [uint32]$seq) {
  Json-Pack 16 1 0 $seq @{ type = 'm.login.password'; identifier = @{ type = 'm.id.user'; user = $user }; password = 'correct-horse-battery'; device_id = $device } @()
}
# Opens with a bearer token; reports 'open' or the refusal the upgrade got.
function Try-Open($tok) {
  try { $ws = Ws-Open $tok; @{ ws = $ws; result = 'open' } }
  catch {
    # .NET wraps the handshake failure in one or two AggregateExceptions; the status code is in the innermost message.
    $e = $_.Exception; while ($e.InnerException) { $e = $e.InnerException }
    @{ ws = $null; result = "refused: $($e.Message)" }
  }
}
$tokPhone = Login-Device-Http 'dave' 'PHONE'
$tokDesk = Login-Device-Http 'dave' 'DESK'

# [5.1] two bearer upgrades for the same device are fine; the third is refused before the upgrade (429)
$c1 = Try-Open $tokPhone; $c2 = Try-Open $tokPhone; $c3 = Try-Open $tokPhone
Log "[5.1a] PHONE upgrades 1,2,3 -> $($c1.result) / $($c2.result) / $($c3.result)  (expect open / open / refused 429)"
$r1 = Ws-Call $c1.ws $ping; $r2 = Ws-Call $c2.ws $ping
Log "[5.1b] the two open ones still answer, Ping -> $(Describe $r1) / $(Describe $r2)  (expect Pong / Pong: the old connections were not the ones turned away)"

# [5.2] another device of the same user has its own count
$d1 = Try-Open $tokDesk
Log "[5.2] DESK upgrade while PHONE is full -> $($d1.result)  (expect open)"

# [5.3] a Login over the channel as the full device: Error TooManyConnections, then the server closes that connection (1008)
$anon5 = Ws-Open $null
Ws-Send $anon5 (Login-Device-Pack 'dave' 'PHONE' 1)
$errF = Recv-Frame $anon5 5000; $closeF = Recv-Frame $anon5 5000
$errDesc = if ($errF.kind -eq 'pack') { Describe $errF.pack } else { $errF.kind }
Log "[5.3a] anonymous connection, Login as PHONE -> $errDesc then $($closeF.kind) code=$($closeF.code)  (expect Error TooManyConnections max_connections=2, then close PolicyViolation)"
$r1 = Ws-Call $c1.ws $ping
Log "[5.3b] the refused login did not touch the token or the other connections, Ping -> $(Describe $r1)  (expect Pong)"
$r = Send-Pack $ping $tokPhone
Log "[5.3c] PHONE's token over HTTP is not counted and still works -> $(Describe $r)  (expect http=200 Pong)"

# [5.4] closing one gives its place back; a Login on an anonymous connection then succeeds
try { $c2.ws.CloseAsync([System.Net.WebSockets.WebSocketCloseStatus]::NormalClosure, 'bye', [Threading.CancellationToken]::None).Wait(3000) | Out-Null } catch {}
Start-Sleep -Milliseconds 800
$anon5b = Ws-Open $null
$r = Ws-Call $anon5b (Login-Device-Pack 'dave' 'PHONE' 1)
Log "[5.4a] after closing one PHONE connection, Login as PHONE on a fresh connection -> $(Describe $r)  (expect Ack device_id=PHONE)"

# [5.5] logging in again on the same connection as the same device keeps its one place: a further PHONE login is still refused.
# (Each password login as PHONE replaces PHONE's access token, Matrix semantics, so the old bearer token is not reused here;
# the count is probed with logins on fresh anonymous connections instead.)
$r = Ws-Call $anon5b (Login-Device-Pack 'dave' 'PHONE' 2)
Log "[5.5a] the same connection logs in again as PHONE -> $(Describe $r)  (expect Ack: same device, same place)"
$anon5c = Ws-Open $null
Ws-Send $anon5c (Login-Device-Pack 'dave' 'PHONE' 1)
$errF = Recv-Frame $anon5c 5000; $closeF = Recv-Frame $anon5c 5000
$errDesc = if ($errF.kind -eq 'pack') { Describe $errF.pack } else { $errF.kind }
Log "[5.5b] a third PHONE login right after -> $errDesc then $($closeF.kind)  (expect Error TooManyConnections then close: re-login did not take a second place)"

# [5.6] Logout closes the connection and frees the place
Ws-Send $anon5b (Json-Pack 16 3 0 3 @{} @())
$ackF = Recv-Frame $anon5b 5000; $closeF = Recv-Frame $anon5b 5000
Start-Sleep -Milliseconds 800
$anon5d = Ws-Open $null
$r = Ws-Call $anon5d (Login-Device-Pack 'dave' 'PHONE' 1)
Log "[5.6] after Logout ($($ackF.kind) then $($closeF.kind)) on one PHONE connection, a PHONE login -> $(Describe $r)  (expect Ack: the place came back)"
Stop-Server $p

Log ''; Log 'DONE'
