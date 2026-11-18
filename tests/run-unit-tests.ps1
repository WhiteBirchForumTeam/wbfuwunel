# 兩套單元測試，各跑一次，並行度有上限。
#
# 🚨 為什麼要有這支腳本：`cargo test` 的預設並行度是**邏輯核心數**，而這個 repo 的
# `src/main/tests` 有 44 個 target、總共只有 46 條測試 —— 幾乎一個檔一條，而**每個 target
# 都把整台 server 靜態連結一次**（含 rocksdb）。6 個 rustc 同時做那件事，記憶體峰值就在那裡，
# 跟「測試本身跑多久」無關。維護者 2026-11-18 指定這台機器最多 2 條。
#
# ⚠️ 兩套是兩回事，少跑一套不算全套（/docs/design/build/windows-build.md §整合測試）：
#   ① `--workspace --exclude tuwunel`：純單元，但**不含** `src/main/tests`
#   ② `-p tuwunel --no-default-features --features …`：那 44 個整合 target
#
# 用法：
#   pwsh tests/run-unit-tests.ps1             # 上限 2
#   pwsh tests/run-unit-tests.ps1 -Jobs 6     # 核心多、記憶體夠的機器自己調高
#   pwsh tests/run-unit-tests.ps1 -Only unit  # 只跑 ①（改了程式但還沒要 commit 時用）
[CmdletBinding()]
param(
  [int]$Jobs = 2,
  [ValidateSet('both', 'unit', 'integration')]
  [string]$Only = 'both'
)

$ErrorActionPreference = 'Continue'
Set-Location (Join-Path $PSScriptRoot '..')

if ($Jobs -lt 1) { $Jobs = 1 }
# 兩層都要壓：`-j` 是編譯／連結的並行度，`--test-threads` 是一個 test binary 內部的。
# 🚫 只壓一邊沒有用 —— 大頭在連結那一邊。
$env:CARGO_BUILD_JOBS = "$Jobs"
$env:RUST_TEST_THREADS = "$Jobs"
# MSVC 的 mspdbsrv 在多個 test binary 並行連結時會撞 PDB（LNK1318）。
# ⭐ `debug_assertions` 是另一個旗標，不受影響，所以 debug-only 的檢查照樣活著。
$env:RUSTFLAGS = '-Cdebuginfo=0'

$features = 'brotli_compression,element_hacks,gzip_compression,media_thumbnail,release_max_log_level,url_preview,zstd_compression'
$failed = $false

function Invoke-Suite([string]$title, [string[]]$arguments) {
  "=== $title（-j $Jobs，--test-threads $Jobs）==="
  & cargo @arguments
  if ($LASTEXITCODE -ne 0) { $script:failed = $true; "!!! $title 失敗（exit $LASTEXITCODE）" }
}

if ($Only -ne 'integration') {
  Invoke-Suite '① 單元（workspace，不含 tuwunel）' @(
    'test', '--workspace', '--exclude', 'tuwunel',
    '-j', "$Jobs", '--no-fail-fast', '--', '--test-threads', "$Jobs"
  )
}

if ($Only -ne 'unit') {
  # 🚨 這一套**不能**帶 `-- --test-threads`：`src/main/tests` 每一支都走 `Args::default_test`，
  # 而 `Args::default()` 就是 `Args::parse()` —— 它拿 **test binary 自己的命令列**給 clap 解，
  # 所以 harness 的參數會被當成 server 的參數：`error: unexpected argument '--test-threads' found`，
  # 38 個 target 一起死（2026-11-18 實踩）。
  # ⭐ 這一套靴的是上面的 `RUST_TEST_THREADS`：libtest 讀得到，而 clap 看不到。
  Invoke-Suite '② 整合（src/main/tests 的 44 個 target）' @(
    'test', '-p', 'tuwunel', '--no-default-features', '--features', $features,
    '-j', "$Jobs", '--no-fail-fast'
  )
}

if ($failed) { '################ RESULT: 有失敗 ################'; exit 1 }
'################ RESULT: 兩套都綠 ################'
