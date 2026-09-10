# Chunked upload and download over HTTP (POST /_wbf/v1/pack): create, ordered chunks, resume, seal, truncation,
# streaming mode, reads by chunk and by plaintext position, abort and sweeper. Design: docs/design/chunked-upload-spec.md.
# Older style: every line is logged with its expectation in parentheses; read results.txt, there is no pass/fail summary.
$ErrorActionPreference = 'Continue'
# Everything a run writes goes under target/, never next to the scripts.
$REPO = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path.TrimEnd('\')
$S   = Join-Path $REPO 'target\e2e-runs'
New-Item -ItemType Directory -Force $S | Out-Null
$EXE = if ($env:E2E_EXE) { $env:E2E_EXE } else { Join-Path $REPO 'target\e2e\tuwunel.exe' }
$OUT = "$S\e2e6-out"
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
  $cfg = "$S\e2e6.toml"
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

# ================= Scenario 1: 3 chunks, out of order, resume, seal, read back =================
$db1 = "$S\e2e6db-1"; Remove-Item -Recurse -Force $db1 -EA SilentlyContinue; New-Item -ItemType Directory -Force $db1 | Out-Null
$cfg = Write-Config $db1 86400
Log '################ Scenario 1: chunked upload over POST /_wbf/v1/pack ################'
$p = Start-Server $cfg 's1'
$reg = Api Post '/_matrix/client/v3/register' '{"username":"e2e","password":"correct-horse-battery","auth":{"type":"m.login.dummy"}}' $null
$tok = $reg.access_token; Log "user = $($reg.user_id)"

$r = Send-Pack (New-Pack 1 4 0 0 1 ([Text.Encoding]::UTF8.GetBytes('{"nonce":7}')) @()) $tok
Log "[1.0] ping -> $(Describe $r)  (expect Pong, meta echoed, crc ok=$($r.metaCrcOk))"
$r = Send-Pack (New-Pack 1 4 0 0 1 @() @()) $null
Log "[1.0b] no token -> $(Describe $r)  (expect Error Unauthorized http=401)"

# "ciphertext": random bytes; chunk_size 65536 -> wire 65552; total 120000 -> 2 chunks? use 3: total = 2*65552 + 1000
$wire = 65552; $total = 2 * $wire + 1000
$rng = New-Object System.Random(42); $file = New-Object byte[] $total; $rng.NextBytes($file)
$desc = [Text.Encoding]::UTF8.GetBytes('ENCRYPTED-DESCRIPTION-OPAQUE-TO-SERVER')
$r = Send-Pack (Create-Pack 1 132056 65536 3 $desc) $tok
Log "[1.1] Create file_size=132056 chunk_count=3, data=encrypted description -> $(Describe $r)  (expect Ack id!=0, mxc media id = hex(id), chunk_max_bytes=69632)"
$id = [uint64]$r.meta.id; $mxc = $r.meta.mxc
$chunk = { param($i) $s = $i * $wire; $e = [Math]::Min($s + $wire, $total) - 1; $file[$s..$e] }

$r = Send-Pack (New-Pack 3 2 0 $id 0 @() (& $chunk 0)) $tok
Log "[1.2] chunk 0 -> $(Describe $r)  (expect Ack received=1 total_len=$wire finished=false)"
$r = Send-Pack (New-Pack 3 2 0 $id 2 @() (& $chunk 2)) $tok
Log "[1.3] chunk 2 before 1 -> $(Describe $r)  (expect Error OutOfOrder expected_seq=1)"
$r = Send-Pack (New-Pack 3 3 0 $id 5 @() @()) $tok
Log "[1.4] Status -> $(Describe $r)  (expect received=1 total_len=$wire finished=false chunk_size=65536)"
$r = Send-Pack (New-Pack 3 4 0 $id 6 @() @()) $tok
Log "[1.5] Seal before IS_LAST -> $(Describe $r)  (expect Error Conflict: not finished)"
$bad = New-Pack 3 2 0 $id 1 @() (& $chunk 1); $bad[$bad.Length - 10] = $bad[$bad.Length - 10] -bxor 0xFF
$r = Send-Pack $bad $tok
Log "[1.6] chunk 1 with damaged data -> $(Describe $r)  (expect Error Corrupt: DataCrc)"
$r = Send-Pack (New-Pack 3 2 0 $id 1 @() (& $chunk 1)) $tok
Log "[1.7] chunk 1 -> $(Describe $r)  (expect Ack received=2)"
$r = Send-Pack (New-Pack 3 2 0 $id 1 @() (& $chunk 1)) $tok
Log "[1.7b] chunk 1 again -> $(Describe $r)  (expect Ack received=2, idempotent)"
$r = Send-Pack (New-Pack 3 2 8 $id 2 @() (& $chunk 2)) $tok
Log "[1.8] chunk 2 with IS_LAST -> $(Describe $r)  (expect Ack received=3 total_len=$total finished=true)"
$r = Send-Pack (New-Pack 3 2 0 $id 3 @() (New-Object byte[] 100)) $tok
Log "[1.8b] chunk 3 after IS_LAST -> $(Describe $r)  (expect Error Conflict: finished)"
$r = Send-Pack (New-Pack 3 4 0 $id 7 @() @()) $tok
Log "[1.9] Seal -> $(Describe $r)  (expect Ack mxc=$mxc)"
$dl = Get-Bytes $mxc $tok
$same = ($dl.status -eq 200) -and ($dl.bytes.Length -eq $total) -and ([Linq.Enumerable]::SequenceEqual([byte[]]$dl.bytes, [byte[]]$file))
Log "[1.10] standard download: status=$($dl.status) len=$($dl.bytes.Length) identical=$same  (expect 200 / $total / True)"
$r = Send-Pack (Json-Pack 4 1 0 8 @{ mxc = $mxc } @()) $tok
Log "[1.11] Info -> $(Describe $r) description identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$r.data, [byte[]]$desc))  (expect total_len=$total file_size=132056 chunk_size=65536 chunk_count=3, data = the encrypted description, True)"
$r = Send-Pack (Json-Pack 4 2 0 9 @{ mxc = $mxc; chunk = 1 } @()) $tok
$seg = [Linq.Enumerable]::SequenceEqual([byte[]]$r.data, [byte[]]$file[$wire..(2*$wire-1)])
Log "[1.12] Read chunk=1 -> $(Describe $r) identical=$seg dataCrcOk=$($r.dataCrcOk)  (expect chunk=1 pos=65536 (plaintext start) len=$wire, True)"
$r = Send-Pack (Json-Pack 4 2 0 10 @{ mxc = $mxc; pos = (2*65536 + 7) } @()) $tok
$seg = [Linq.Enumerable]::SequenceEqual([byte[]]$r.data, [byte[]]$file[(2*$wire)..($total-1)])
Log "[1.13] Read plaintext pos=2*64KiB+7 (seek) -> $(Describe $r) identical=$seg  (expect chunk=2 pos=131072 len=1000, True)"
$r = Send-Pack (Json-Pack 4 2 0 11 @{ mxc = $mxc; pos = (3*65536) } @()) $tok
Log "[1.14] Read plaintext pos=3*64KiB (past last chunk) -> $(Describe $r)  (expect Error InvalidRequest)"
$r = Send-Pack (Json-Pack 4 2 0 13 @{ mxc = $mxc; chunk = 3 } @()) $tok
Log "[1.14b] Read chunk=3 -> $(Describe $r)  (expect Error InvalidRequest: past last chunk)"
$r = Send-Pack (New-Pack 3 3 0 $id 12 @() @()) $tok
Log "[1.15] Status after seal -> $(Describe $r)  (expect Error NotFound: row gone)"
Stop-Server $p
Log ''; Log '=== [1.16] refcount (expect 0 reference(s)) ==='
Exec $cfg @("media refcount $mxc") 'q116' | ForEach-Object { Log "  $_" }

# ================= Scenario 2: abort, and sweeper with 1s ttl =================
$p = Start-Server $cfg 's2'
$r = Send-Pack (Create-Pack 1 100000 0 2 @()) $tok; $id2 = [uint64]$r.meta.id
$r = Send-Pack (New-Pack 3 2 0 $id2 0 @() (New-Object byte[] 65552)) $tok
Log "[2.1] second upload, chunk 0 -> $(Describe $r)"
$r = Send-Pack (New-Pack 3 5 0 $id2 2 @() @()) $tok
Log "[2.2] Abort -> $(Describe $r)  (expect Ack ok)"
$r = Send-Pack (New-Pack 3 3 0 $id2 3 @() @()) $tok
Log "[2.3] Status after abort -> $(Describe $r)  (expect Error NotFound)"
$r = Send-Pack (Create-Pack 1 100000 0 2 @()) $tok; $id3 = [uint64]$r.meta.id
$r = Send-Pack (New-Pack 3 2 0 $id3 0 @() (New-Object byte[] 65552)) $tok
Log "[2.4] third upload id=$id3 chunk 0 -> $(Describe $r)"
$staging = Get-ChildItem "$db1\media\staging" -EA SilentlyContinue | Select-Object -ExpandProperty Name
Log "      staging files: $($staging -join ', ')  (expect one, hex of id)"
Stop-Server $p
$cfg = Write-Config $db1 1
Start-Sleep -Seconds 2
$p = Start-Server $cfg 's2b'
Start-Sleep -Seconds 3
$r = Send-Pack (New-Pack 3 3 0 $id3 4 @() @()) $tok
Log "[2.5] restarted with ttl=1, Status of third -> $(Describe $r)  (expect Error NotFound: swept at start)"
$staging = Get-ChildItem "$db1\media\staging" -EA SilentlyContinue | Select-Object -ExpandProperty Name
Log "      staging files: '$($staging -join ', ')'  (expect none)"
((Get-Content "$OUT\s2b.out","$OUT\s2b.err" -Raw) -replace "`e\[[0-9;]*m", '' -split "`n") | Where-Object { $_ -match 'Sweeping|staging' } | ForEach-Object { Log "  server: $($_ -replace '^\S+\s+','')" }
Stop-Server $p
# ================= Scenario 3: variable chunk lengths (streaming), size caps, resume by Status =================
$cfg = Write-Config $db1 86400
$p = Start-Server $cfg 's3'
$r = Send-Pack (New-Pack 3 1 0 0 1 (New-Object byte[] 15) @()) $tok
Log "[3.0] Create with a 15-byte meta -> $(Describe $r)  (expect Error InvalidRequest: EncryptedFileInfo is 16 bytes)"
$r = Send-Pack (Create-Pack 1 4096 1024 4 @()) $tok
Log "[3.1] Create chunk_size=1024 (below min 4 KiB) -> $(Describe $r)  (expect Error Conflict)"
$r = Send-Pack (Create-Pack 1 65550 16384 4 @()) $tok
Log "[3.1b] Create chunk_count=4 for file_size=65550/16384 -> $(Describe $r)  (expect Error Conflict: expected 5)"
$r = Send-Pack (Create-Pack 1 65550 16384 5 @()) $tok; $id4 = [uint64]$r.meta.id
Log "[3.2] Create file_size=65550 chunk_size=16384 chunk_count=5 -> $(Describe $r)  (expect chunk_max_bytes=20480)"
# a "stream": chunks of whatever the encryptor produced, every one a different length: 16400, 16416, 16400, 16000, then 30 bytes with IS_LAST
$lens = @(16400, 16416, 16400, 16000, 30); $file4 = New-Object byte[] (($lens | Measure-Object -Sum).Sum); (New-Object Random 7).NextBytes($file4)
$r = Send-Pack (New-Pack 3 2 0 $id4 0 @() (New-Object byte[] 20481)) $tok
Log "[3.3] chunk 0 of 20481 bytes (over chunk_max_bytes) -> $(Describe $r)  (expect Error TooLarge)"
$r = Send-Pack (New-Pack 3 2 0 $id4 0 @() @()) $tok
Log "[3.4] empty chunk -> $(Describe $r)  (expect Error Conflict)"
$off = 0
for ($i = 0; $i -lt 4; $i++) { $r = Send-Pack (New-Pack 3 2 0 $id4 $i @() ([byte[]]$file4[$off..($off+$lens[$i]-1)])) $tok; $off += $lens[$i] }
Log "[3.5] four chunks of 16400/16416/16400/16000 -> $(Describe $r)  (expect received=4 total_len=65216 finished=false: lengths may vary)"
$r = Send-Pack (New-Pack 3 4 0 $id4 4 @() @()) $tok
Log "[3.6] Seal before the last chunk -> $(Describe $r)  (expect Error Conflict: not finished)"
# "reconnect": ask where we are, resume from received
$r = Send-Pack (New-Pack 3 3 0 $id4 5 @() @()) $tok; $resume = [int]$r.meta.received
Log "[3.7] Status -> $(Describe $r)  (expect received=4 -> resume seq=4)"
$r = Send-Pack (New-Pack 3 2 8 $id4 $resume @() ([byte[]]$file4[$off..($off+29)])) $tok
Log "[3.8] chunk $resume of 30 bytes with IS_LAST -> $(Describe $r)  (expect received=5 total_len=65246 finished=true)"
$r = Send-Pack (New-Pack 3 2 8 $id4 $resume @() ([byte[]]$file4[$off..($off+29)])) $tok
Log "[3.9] same last chunk again -> $(Describe $r)  (expect Ack received=5, idempotent)"
$r = Send-Pack (New-Pack 3 4 0 $id4 6 @() @()) $tok; $mxc4 = $r.meta.mxc
Log "[3.10] Seal -> $(Describe $r)  (expect mxc)"
$got = (Get-Bytes $mxc4 $tok).bytes
Log "[3.11] download $($got.Length) bytes identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$got, [byte[]]$file4))  (expect 65246, True)"
$r = Send-Pack (Json-Pack 4 1 0 7 @{ mxc = $mxc4 } @()) $tok
Log "[3.12] Info -> $(Describe $r)  (expect total_len=65246 chunk_size=16384 chunk_count=5)"
$r = Send-Pack (Json-Pack 4 2 0 8 @{ mxc = $mxc4; chunk = 4 } @()) $tok
Log "[3.12b] Read chunk=4 -> $(Describe $r) identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$r.data, [byte[]]$file4[65216..65245]))  (expect pos=65536 (plaintext start) len=30, True)"
$r = Send-Pack (Json-Pack 4 2 0 9 @{ mxc = $mxc4; pos = 16385 } @()) $tok
Log "[3.12c] Read plaintext pos=16385 -> $(Describe $r) identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$r.data, [byte[]]$file4[16400..32815]))  (expect chunk=1 pos=16384 len=16416, True)"
$r = Send-Pack (Json-Pack 4 2 0 10 @{ mxc = $mxc4; pos = (3*16384 + 5) } @()) $tok
Log "[3.12d] Read plaintext pos=3*16384+5 -> $(Describe $r) identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$r.data, [byte[]]$file4[49216..65215]))  (expect chunk=3 pos=49152 len=16000, True)"
# a one-chunk upload: first chunk is also the last
$r = Send-Pack (Create-Pack 1 761 0 1 @()) $tok; $id5 = [uint64]$r.meta.id
$file5 = New-Object byte[] 777; (New-Object Random 9).NextBytes($file5)
$r = Send-Pack (New-Pack 3 2 8 $id5 0 @() $file5) $tok
Log "[3.13] single chunk with IS_LAST -> $(Describe $r)  (expect received=1 total_len=777 finished=true)"
$r = Send-Pack (New-Pack 3 4 0 $id5 2 @() @()) $tok; $mxc5 = $r.meta.mxc
$got = (Get-Bytes $mxc5 $tok).bytes
Log "[3.14] Seal + download $($got.Length) bytes identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$got, [byte[]]$file5))  (expect 777, True; content_type null)"
$r = Send-Pack (Json-Pack 4 1 0 8 @{ mxc = $mxc5 } @()) $tok
Log "[3.15] Info -> $(Describe $r)  (expect total_len=777 chunk_count=1 content_type=null (server never told))"
Stop-Server $p
# ================= Scenario 4: media_upload_max_len ends an upload truncated; the chunk budget follows from it =================
$cfg = Write-Config $db1 86400 100000
$p = Start-Server $cfg 's4'
$r = Send-Pack (Create-Pack 1 100000 65536 2 @()) $tok; $id6 = [uint64]$r.meta.id
$file6 = New-Object byte[] $wire; (New-Object Random 11).NextBytes($file6)
$r = Send-Pack (New-Pack 3 2 0 $id6 0 @() $file6) $tok
Log "[4.1] max_len=100000: chunk 0 of $wire -> $(Describe $r)  (expect received=1 truncated=false)"
$r = Send-Pack (New-Pack 3 2 0 $id6 1 @() (New-Object byte[] $wire)) $tok
Log "[4.2] chunk 1 would cross 100000 -> $(Describe $r)  (expect Error Truncated received=1 total_len=$wire finished=true truncated=true)"
$r = Send-Pack (New-Pack 3 2 0 $id6 1 @() (New-Object byte[] 10)) $tok
Log "[4.3] chunk 1 again, small -> $(Describe $r)  (expect Error Conflict: finished)"
$r = Send-Pack (New-Pack 3 3 0 $id6 2 @() @()) $tok
Log "[4.4] Status -> $(Describe $r)  (expect finished=true truncated=true)"
$r = Send-Pack (New-Pack 3 4 0 $id6 3 @() @()) $tok; $mxc6 = $r.meta.mxc
Log "[4.5] Seal truncated upload -> $(Describe $r)  (expect mxc)"
$got = (Get-Bytes $mxc6 $tok).bytes
Log "[4.6] download $($got.Length) bytes identical to chunk 0=$([Linq.Enumerable]::SequenceEqual([byte[]]$got, [byte[]]$file6))  (expect $wire, True)"
$r = Send-Pack (Json-Pack 4 1 0 4 @{ mxc = $mxc6 } @()) $tok
Log "[4.7] Info -> $(Describe $r)  (expect chunk_count=1 truncated=true)"
# the chunk budget is settled at Create: count must match the sizes, size must fit the limit
Stop-Server $p
$cfg = Write-Config $db1 86400 40000
$p = Start-Server $cfg 's4b'
$r = Send-Pack (Create-Pack 1 50000 16384 4 @()) $tok
Log "[4.8] max_len=40000: Create file_size=50000 -> $(Describe $r)  (expect Error TooLarge)"
$r = Send-Pack (Create-Pack 1 40000 16384 4 @()) $tok
Log "[4.9] Create chunk_count=4 for 40000/16384 -> $(Describe $r)  (expect Error Conflict: expected 3)"
$r = Send-Pack (Create-Pack 1 40000 16384 3 @()) $tok; $id7 = [uint64]$r.meta.id
for ($i = 0; $i -lt 3; $i++) { $r = Send-Pack (New-Pack 3 2 0 $id7 $i @() (New-Object byte[] 1)) $tok }
Log "[4.10] three 1-byte chunks -> $(Describe $r)  (expect received=3 finished=true: count reached, no IS_LAST needed)"
$r = Send-Pack (New-Pack 3 2 0 $id7 3 @() (New-Object byte[] 1)) $tok
Log "[4.11] fourth chunk -> $(Describe $r)  (expect Error Conflict: finished)"
((Get-Content "$OUT\s4b.out","$OUT\s4b.err" -Raw) -replace "`e\[[0-9;]*m", '' -split "`n") | Where-Object { $_ -match 'truncated' } | ForEach-Object { Log "  server: $($_ -replace '^\S+\s+','')" }
Stop-Server $p
# ================= Scenario 5: a stream (file_size 0, chunk_count 0), ended by IS_LAST, description replaced at Seal =================
$cfg = Write-Config $db1 86400 0
$p = Start-Server $cfg 's5'
$r = Send-Pack (Create-Pack 1 0 16384 2 @()) $tok
Log "[5.1] Create file_size=0 chunk_count=2 -> $(Describe $r)  (expect Error Conflict: 0 only with chunk_count 0)"
$desc0 = [Text.Encoding]::UTF8.GetBytes('ENCRYPTED-DESC-BEFORE-SIZE-KNOWN')
$r = Send-Pack (Create-Pack 1 0 16384 0 $desc0) $tok; $id8 = [uint64]$r.meta.id
Log "[5.2] Create stream (0/0) chunk_size=16384 -> $(Describe $r)  (expect Ack)"
$lens8 = @(16400, 16400, 16400, 5000); $file8 = New-Object byte[] (($lens8 | Measure-Object -Sum).Sum); (New-Object Random 13).NextBytes($file8)
$off = 0
for ($i = 0; $i -lt 3; $i++) { $r = Send-Pack (New-Pack 3 2 0 $id8 $i @() ([byte[]]$file8[$off..($off+$lens8[$i]-1)])) $tok; $off += $lens8[$i] }
Log "[5.3] three chunks, no end declared -> $(Describe $r)  (expect received=3 chunk_count=null finished=false)"
$r = Send-Pack (New-Pack 3 3 0 $id8 9 @() @()) $tok
Log "[5.4] Status -> $(Describe $r)  (expect chunk_count=null file_size=null finished=false)"
$r = Send-Pack (New-Pack 3 4 0 $id8 10 @() @()) $tok
Log "[5.5] Seal before IS_LAST -> $(Describe $r)  (expect Error Conflict: not finished)"
$r = Send-Pack (New-Pack 3 2 8 $id8 3 @() ([byte[]]$file8[$off..($off+4999)])) $tok
Log "[5.6] chunk 3 of 5000 with IS_LAST -> $(Describe $r)  (expect received=4 finished=true)"
$desc1 = [Text.Encoding]::UTF8.GetBytes('ENCRYPTED-DESC-WITH-FINAL-SIZE')
$r = Send-Pack (New-Pack 3 4 0 $id8 11 @() $desc1) $tok; $mxc8 = $r.meta.mxc
Log "[5.7] Seal with a new description in data -> $(Describe $r)  (expect mxc)"
$got = (Get-Bytes $mxc8 $tok).bytes
Log "[5.8] download $($got.Length) bytes identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$got, [byte[]]$file8))  (expect 54200, True)"
$r = Send-Pack (Json-Pack 4 1 0 12 @{ mxc = $mxc8 } @()) $tok
Log "[5.9] Info -> $(Describe $r) description=$([Text.Encoding]::UTF8.GetString($r.data))  (expect chunk_count=4 file_size=null, data = the NEW description)"
$r = Send-Pack (Json-Pack 4 2 0 13 @{ mxc = $mxc8; pos = (3*16384 + 1) } @()) $tok
Log "[5.10] Read pos=3*16384+1 -> $(Describe $r) identical=$([Linq.Enumerable]::SequenceEqual([byte[]]$r.data, [byte[]]$file8[49200..54199]))  (expect chunk=3 len=5000, True)"
Stop-Server $p
Log ''; Log 'DONE'
