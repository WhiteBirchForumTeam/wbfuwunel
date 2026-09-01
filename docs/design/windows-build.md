# 在 Windows 上建置與執行

> 這份文件回答 [why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md)
> §7 的待驗第 2 題：**「Conduit 家族在 Windows 上編不編得出來？」**
>
> **答案：編得出來，而且跑得動。** 下面是驗證方法與實測結果，以及 Windows 上少掉什麼。
> 實測日期 2026-09-01，版本 `1.9.0-91 (v1.9.0-91-g63b8b74e2c)`，目標
> `x86_64-pc-windows-msvc`。

## 需要的東西

| 需求 | 為什麼 | 怎麼來 |
|---|---|---|
| MSVC + Windows SDK | 連結器，以及 RocksDB 的 C++ | Visual Studio 2022（實測 Community，`cl.exe` 14.37.32822、SDK 10.0.22621） |
| CMake | `aws-lc-sys` 用它建置 | ⭐ **VS 2022 自帶**，不必另外安裝：`…\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin`（實測 3.26.4） |
| LLVM / clang | `rust-rocksdb` 開了 `bindgen-runtime`，需要 `libclang.dll` | `winget install LLVM.LLVM`，再設 `LIBCLANG_PATH` |
| NASM | `aws-lc-sys` 在 x86_64 Windows 要它組譯 | `winget install NASM.NASM` |
| Rust 1.95.0 | `rust-toolchain.toml` 釘死的版本 | `rustup`，進到 repo 目錄執行 `rustup toolchain install` |

⚠️ `rust-toolchain.toml` 的 `targets` 清單只列 Linux 目標。**不必為了 Windows 改它** ——
host 目標本來就會裝，那份清單是給交叉編譯用的。

## 建置

先進 MSVC 環境（`vcvars64.bat`），把 LLVM、NASM、VS 的 CMake 放進 `PATH`，設好
`LIBCLANG_PATH`，然後：

```sh
cargo build -p tuwunel --release --no-default-features --features \
  brotli_compression,element_hacks,gzip_compression,media_thumbnail,release_max_log_level,url_preview,zstd_compression
```

兩個容易踩到的地方：

- **要 `-p tuwunel`。** 這是 virtual workspace，在根目錄下 `--features` 會被拒絕。
- **`--no-default-features` 是必要的**，因為預設 feature 含三個 Linux-only 的項目：

| 拿掉的 feature | 原因 |
|---|---|
| `io_uring` | Linux 專屬的非同步 IO 介面 |
| `jemalloc` / `jemalloc_conf` | jemalloc 不支援 MSVC |
| `systemd` | 只有 Linux 有 systemd |

🚫 `tuwunel_mods`（熱重載）在 Windows 上**不能開** —— `src/core/mods/mod.rs` 直接用
`libloading::os::unix`。它本來就不在預設 feature 裡。

實測：**36 分 30 秒**（release，thin LTO），產物
`target\release\tuwunel.exe` 約 **110 MB**，單一 binary，只動態連 MSVC CRT。

## 實測結果

編譯成功不等於跑得動 —— RocksDB 能不能在 Windows 開起來才是這題真正要問的。實跑：

```
INFO tuwunel::server: 1.9.0-91 (v1.9.0-91-g63b8b74e2c) server_name=localhost
INFO tuwunel_database::engine::open: Opened database. columns=133 sequence=0 time=395.6317ms
WARN tuwunel_service::migrations: Created new RocksDB database with version 17
INFO tuwunel_router::serve: Listening on ["tcp:127.0.0.1:8009"]
```

- **RocksDB 開得起來**：133 個 column family，395 ms，`CURRENT` / `MANIFEST` / `LOCK`
  都正常落地。
- `GET /_matrix/client/versions` → 正常回應（`r0.0.1` 到 `v1.19`）。
- `GET /_matrix/federation/v1/version` → `403`，符合設定的 `allow_federation = false`。
- `--generate-config` → 4176 行，exit 0。**這條特別值得跑**：它會走
  `src/core/config/regenerate/write.rs`，那是全 repo `#[cfg(unix)]` 最密集、non-unix
  fallback 最容易出事的檔案。

## ⚠️ 已知地雷：`database_path` 沒有 Windows 預設

`src/core/config/mod.rs` 的 `default_database_path()` 硬寫 `/var/lib/tuwunel`，**沒有平台
分支**。在 Windows 上這會解析到當前磁碟根目錄的 `\var\lib\tuwunel`。

👉 **設定檔一定要自己寫 `database_path`。** 這是目前唯一必須手動處理的差異。

## Windows 上少掉什麼

程式碼對這些都有 `#[cfg(not(unix))]` 的對應分支，所以是**功能不存在**，不是會壞掉：

| 少掉的 | 行為 |
|---|---|
| UNIX socket 監聽 | 設定檢查會**明確拒絕** `unix_socket_path`（`src/core/config/check.rs`），不是靜默忽略 |
| systemd 整合 | socket activation、watchdog、啟動通知都沒有 |
| journald 日誌 | 只有 stdout |
| 完整訊號處理 | 只有 Ctrl+C 觸發關機；沒有 SIGHUP 那類重載（`src/main/signals.rs`） |
| 自我重啟 | `restart::restart()` 是 `#[cfg(unix)]`，Windows 上不會自我重啟 |
| 熱重載 | `tuwunel_mods` 用 `libloading::os::unix`，不能開 |
| rlimit / rusage / statfs 指標 | 退回中性值（`src/core/utils/sys/` 的 `limits.rs`、`usage.rs`、`storage.rs`） |

⭐ **這些 fallback 是上游本來就寫好的**，不是這個 fork 加的。所以「Windows 支援」不是要
從零長出來，而是既有的路徑本來就通 —— 這對設計文件的判斷很重要。

## 順帶修正一則舊說法

設計文件的待驗 6 曾順口提到「Windows 沒有 C 加密函式庫原生建置路徑」。**這句話是錯的**，
已經在該文件修正：`aws-lc-sys` 在 Windows x86_64 有正規建置路徑（CMake + NASM），這次的
建置就是走它過的。
