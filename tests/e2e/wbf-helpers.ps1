# Shared helpers for the wbf end-to-end scripts (pack codec, HTTP and WebSocket transport, server start/stop).
# Dot-sourced by e2e*.ps1; not a test by itself. See README.md.
$ErrorActionPreference = 'Continue'
# Everything a run writes (configs, databases, server logs, results) goes under target/, never next to the scripts.
$REPO = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path.TrimEnd('\')
$S   = Join-Path $REPO 'target\e2e-runs'
New-Item -ItemType Directory -Force $S | Out-Null
$EXE = if ($env:E2E_EXE) { $env:E2E_EXE } else { Join-Path $REPO 'target\e2e\tuwunel.exe' }
$OUT = "$S\e2e8-out"
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
# .NET Framework pools HttpWebRequest (Invoke-WebRequest) and ClientWebSocket connections per host through one
# ServicePoint, default limit 2; raised so scripts that keep several WebSockets open never queue behind them.
# (Hygiene, not a fix for anything observed: the 2026-09-07 "server did not come up" was a script clobbering $B,
# see README.)
[System.Net.ServicePointManager]::DefaultConnectionLimit = 64
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

function Write-Config([string]$db, [int]$uploadTtl, [long]$maxLen = 0, [long]$dataMax = 0) {
  $cfg = "$S\e2e8.toml"
  @('[global]','server_name = "localhost"',('database_path = "' + ($db -replace '\\','/') + '"'),'port = 8015','address = ["127.0.0.1"]',
    'allow_registration = true','yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true','allow_federation = false',
    ('media_upload_ttl = ' + $uploadTtl),('media_upload_max_len = ' + $maxLen),'log = "info"') + $(if ($dataMax -gt 0) { @(('wbf_data_max_bytes = ' + $dataMax)) } else { @() }) -join "`n" | Set-Content -Path $cfg -Encoding ascii
  $cfg
}
function Start-Server([string]$cfg, [string]$tag) {
  $p = Start-Process -FilePath $EXE -ArgumentList @('-c', $cfg) -PassThru -NoNewWindow -RedirectStandardOutput "$OUT\$tag.out" -RedirectStandardError "$OUT\$tag.err"
  $lastError = ''
  for ($i = 0; $i -lt 40; $i++) {
    Start-Sleep -Milliseconds 500
    try { $null = Invoke-WebRequest -Uri "$B/_matrix/client/versions" -TimeoutSec 2 -UseBasicParsing; return $p } catch { $lastError = $_.Exception.Message }
  }
  # Before giving up, tell the two failure modes apart: a server that is not there, or this process's HTTP stack
  # (connection pool, stale keep-alives) refusing to reach a server that is. A fresh HttpClient bypasses the pool.
  $fresh = New-Object System.Net.Http.HttpClient; $fresh.Timeout = [TimeSpan]::FromSeconds(3)
  $freshResult = try { "fresh HttpClient -> " + [int]$fresh.GetAsync("$B/_matrix/client/versions").Result.StatusCode } catch { "fresh HttpClient failed too: $($_.Exception.InnerException.Message)" }
  Log "  !! server probe failed 40 times ($tag): last Invoke-WebRequest error: $lastError; $freshResult; process exited=$($p.HasExited)"
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

