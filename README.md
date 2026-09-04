# wbfuwunel

<!-- ANCHOR: catchphrase -->

## 一套自架、可完全掌控的即時通訊伺服器，從 tuwunel 分岔出來

<!-- ANCHOR_END: catchphrase -->

<!-- ANCHOR: body -->

**wbfuwunel** 是 [`matrix-construct/tuwunel`](https://github.com/matrix-construct/tuwunel)
的 fork。起點是一個以 Rust 寫成的 Matrix homeserver；去處是一套**由維護者完全掌控**的
即時通訊服務：資料存在哪、留多久、誰讀得到，由維護者決定，容量有界，刪掉的東西真的會消失，
大檔案與串流是一等公民。

這不是「上游加幾個 feature」的 fork。**分岔會很大，而且會持續變大**：媒體層、訊息模型、
管理介面都會照自己的需求改，與 Matrix 規格的相容性不是目標。上游會持續合併進來，
但方向由這裡決定。

| | |
|---|---|
| 對外（公開） | [`WhiteBirchForumTeam/wbfuwunel`](https://github.com/WhiteBirchForumTeam/wbfuwunel) |
| 開發 | 維護者自架的 Forgejo `amaid/wbfuwunel`，PR 在這裡開 |
| 上游 | [`matrix-construct/tuwunel`](https://github.com/matrix-construct/tuwunel) |
| 授權 | Apache License 2.0，與上游相同 |

## 這個 fork 跟上游差在哪

**看 [`CHANGELOG-fork.md`](CHANGELOG-fork.md)。** 那是唯一的權威清單：每一筆寫做了什麼、為什麼、
去哪看細節。只想看動了哪些 commit：

```sh
git log --oneline upstream/main..main
```

## 授權與修改聲明

本專案是 `matrix-construct/tuwunel` 的**修改版**，依 Apache License 2.0 發佈。上游的
[`LICENSE`](LICENSE)、著作權標示與作者名單全數保留。**本專案已對上游程式碼做過修改。**

🚫 **本專案不是上游的官方發佈，也不代表上游。** 遇到問題請到本專案回報，不要去麻煩上游的維護者。
上游自己的 releases、Docker image 與支援管道都不適用於這裡。

## 命名：專案改名，程式碼不改名

專案叫 **wbfuwunel**，但 **crate 名、binary 名、設定路徑一律維持上游的 `tuwunel`**。
這是刻意的：改掉它們等於跟上游的每一次 merge 都大量衝突，而且是永久的。所以看到 `tuwunel`
出現在程式碼、路徑或 binary 名裡，那是正確的，不是漏改的。

## 文件

fork 專屬的文件都在 [`docs/design/`](docs/design/)。它們刻意不進上游的 mdBook 目錄，
這張表就是索引：

| 想知道 | 看 |
|---|---|
| **接下來做什麼、可能做什麼、明確不做什麼** | [docs/design/roadmap.md](docs/design/roadmap.md) |
| 這個 fork 跟上游的關係、分支模型、改動的流程 | [docs/design/fork-overview.md](docs/design/fork-overview.md) |
| 為什麼要 fork、目標與非目標、核心設計方向 | [docs/design/why-not-matrix-and-core-design.md](docs/design/why-not-matrix-and-core-design.md) |
| 程式碼結構導覽（要改東西時去哪裡找） | [docs/design/repo-structure.md](docs/design/repo-structure.md) |
| Windows 建置與實跑驗證 | [docs/design/windows-build.md](docs/design/windows-build.md) |
| 媒體引用計數（上半：索引，已被下一筆取代） | [docs/design/media-refcount.md](docs/design/media-refcount.md) |
| 媒體的真正刪除：精確計數、哨兵、立即清理、migrate | [docs/design/media-gc.md](docs/design/media-gc.md) |
| 分塊上傳、續傳、range 下載（提案，以塊加密、CRC、先 HTTP 後 WebSocket） | [docs/design/chunked-upload.md](docs/design/chunked-upload.md) |
| **分塊上傳／下載規格書**（給 client 開發者：byte 排法、每個訊息、錯誤碼、流程） | [docs/design/chunked-upload-spec.md](docs/design/chunked-upload-spec.md) |
| 規格的黃金測試向量（server 實作產生，client 複製一份對著測；漂移在測試階段被抓到） | [docs/design/wbf-vectors.json](docs/design/wbf-vectors.json) |
| 流式訊息（文字 token 串流，走 WebSocket 二進位通道，草案） | [docs/design/streaming-messages.md](docs/design/streaming-messages.md) |
| WebSocket 通道的二進位封包外框（兩者共用） | [docs/design/wbf-wire-format.md](docs/design/wbf-wire-format.md) |

上游的使用文件（[`docs/`](docs/) 其餘部分：設定、部署、維護）大體仍適用，因為程式碼的骨架還是
上游的。但凡 `CHANGELOG-fork.md` 寫了行為有變的地方，以它為準。

## 建置與執行

```sh
cargo build --release
```

Windows 上的完整流程、依賴與實跑驗證在
[docs/design/windows-build.md](docs/design/windows-build.md)。設定檔從
[`tuwunel-example.toml`](tuwunel-example.toml) 複製後修改，`server_name` 與 `database_path`
必填。第一個註冊的帳號是伺服器管理員。

## 改動的流程

1. 先寫文件或方案（`docs/design/`）。
2. 維護者同意。
3. 開分支，開 PR，目標分支 `main`。
4. feat、重大 fix、refactor 合併進 `main` 之後，在 `CHANGELOG-fork.md` 留一筆；純文件的分支不用。

跟上游同步只用 `git merge`。🚫 不 rebase、不 force push、不改已推出去的歷史。

<!-- ANCHOR_END: body -->

<!-- ANCHOR: footer -->

<!-- ANCHOR_END: footer -->
