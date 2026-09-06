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

try { $bad = Ws-Open $null; Log "[1.0] no token -> connected?! state=$($bad.State)" } catch { Log "[1.0] no token -> upgrade refused: $($_.Exception.InnerException.Message)  (expect 401)" }

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
Log ''; Log 'DONE'
