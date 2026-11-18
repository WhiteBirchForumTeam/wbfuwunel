# Windows e2e-profile build: MSVC environment, then cargo with the Windows feature set (/docs/design/build/windows-build.md).
$ErrorActionPreference = 'Continue'
$vcvars = 'C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat'
cmd /c "call ""$vcvars"" >nul && set" | ForEach-Object {
  if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "env:$($matches[1])" -Value $matches[2] }
}
$env:PATH = "$env:USERPROFILE\.cargo\bin;C:\Program Files\LLVM\bin;$env:LOCALAPPDATA\bin\NASM;C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin;$env:PATH"
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
Set-Location (Join-Path $PSScriptRoot '..\..')

'--- 工具版本 ---'
cargo --version
(Get-Command cl.exe).Source
(Get-Command cmake.exe).Source
(Get-Command nasm.exe).Source
(Get-Command clang.exe).Source
# 並行度上限（維護者 2026-11-18：這台機器最多 2 條）。🚨 吃記憶體的是連結，而 cargo 的
# 預設 `-j` 是邏輯核心數。外面先設好 CARGO_BUILD_JOBS 就照外面的，方便別台機器調高。
if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = '2' }
"--- 開始建置（CARGO_BUILD_JOBS=$env:CARGO_BUILD_JOBS）---"
$features = 'brotli_compression,element_hacks,gzip_compression,media_thumbnail,release_max_log_level,url_preview,zstd_compression'
cargo build -p tuwunel --profile e2e --no-default-features --features $features 2>&1
"EXITCODE=$LASTEXITCODE"
