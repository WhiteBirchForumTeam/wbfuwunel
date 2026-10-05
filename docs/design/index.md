# 設計文件索引

一個資料夾回答一類問題。**接下來做什麼**在 [/docs/design/overview/roadmap.md](overview/roadmap.md)，
**做過什麼、為什麼**在 [/CHANGELOG-fork.md](../../CHANGELOG-fork.md)，都不在這裡。

**引用的寫法**（維護者 2026-09-30 定）：程式碼註解與文件裡提到另一份文件，一律寫**從 repo 根目錄算的完整路徑加章節**，
例如 `/docs/design/keys/room-device-version.md §4.1`；只有指同一份文件自己的章節才只寫 `§4.1`。
🚫 不帶檔名的 `§N`、以及「橋的設計 §3」「問題書 §6」這類別名，都不要用 —— 搬家或改名時 grep 完整路徑就找得到每一處。

## `overview/`：這個 fork 是什麼

| 文件 | 講什麼 |
|---|---|
| [/docs/design/overview/fork-overview.md](overview/fork-overview.md) | 跟上游 tuwunel 是什麼關係、改動要怎麼進來（提案 → 同意 → 分支 → PR 四步） |
| [/docs/design/overview/why-not-matrix-and-core-design.md](overview/why-not-matrix-and-core-design.md) | ⚠️ **草案**：為什麼不直接用 Matrix、核心設計收斂出來的結論與待驗項目 |
| [/docs/design/overview/repo-structure.md](overview/repo-structure.md) | ⚠️ **導覽不是權威**：哪件事去哪個檔案看（行數是 2026-09-01 量的，會過期） |
| [/docs/design/overview/roadmap.md](overview/roadmap.md) | 順序、狀態、前提；每一項的「為什麼」在它指向的那份文件裡 |

## `build/`：怎麼建、怎麼跑測試

| 文件 | 講什麼 |
|---|---|
| [/docs/design/build/windows-build.md](build/windows-build.md) | Windows 上的 MSVC 環境、e2e profile、🚨 **兩套測試**（單元與整合）各自怎麼跑 |

## `wire/`：線上格式與載著它的管線

| 文件 | 講什麼 |
|---|---|
| [/docs/design/wire/wire-format.md](wire/wire-format.md) | **現行契約**：pack 的每一個 byte、kind／subtype 分配表、flags、`id` 的型別 byte |
| [/docs/design/wire/pack-pipeline.md](wire/pack-pipeline.md) | 連線就是佇列、handler 的契約、連線上限與 client 位址怎麼解析（§2.1／§2.2） |
| [/docs/design/wire/api-bridge.md](wire/api-bridge.md) | 常用 Matrix API 走通道的那座橋：怎麼搬、搬的順序、每一支的成本 |
| [/docs/design/wire/hide-server-version.md](wire/hide-server-version.md) | 📄 **提案，等維護者同意**：預設不說自己的版本（六個洩漏點、`publish_server_version`、哪些**不能**藏）|
| [/docs/design/wire/client-ip-header-chain.md](wire/client-ip-header-chain.md) | client 位址讀一條寫死的 header 鏈（XFF 最右邊 → `X-Real-IP` → `X-Client-IP` → `Client-IP`），而「最右邊」要是真的最右邊（外部審查的 🟡：現在是「最右邊**解得開的**」，會退到 client 可控的值）|
| [/docs/design/wire/session-teardown.md](wire/session-teardown.md) | 登出／刪裝置／撤 token 要把**那個裝置**的連線拆掉（外部審查 #8 後半）。⚠️ 鎖帳號刻意**不**走這條（§4.3）|
| [/docs/design/wire/wbf-vectors.json](wire/wbf-vectors.json) | 黃金向量。⚠️ `src/core/wbf/vectors.rs` 用 `include_str!` 讀它、client repo 也複製一份對著測 —— **改路徑要連程式一起改** |

📎 每個 kind 的逐支對照表在 [/docs/bridge-specs/index.md](../bridge-specs/index.md)（那是**分配結果**，設計理由在 `wire/api-bridge.md`）。

## `media/`：上傳，以及一份檔案還有誰在用

| 文件 | 講什麼 |
|---|---|
| [/docs/design/media/chunked-upload.md](media/chunked-upload.md) | 分塊上傳的設計與取捨（模型被重定三次的紀錄） |
| [/docs/design/media/chunked-upload-spec.md](media/chunked-upload-spec.md) | **給 client 開發者的線上規格**：byte 怎麼排、每個訊息、錯誤碼、流程 |
| [/docs/design/media/media-holders.md](media/media-holders.md) | **現行做法**：持有者集合（外鍵）取代引用計數，誰加、誰拿掉、什麼時候刪 |
| [/docs/design/media/media-attachments.md](media/media-attachments.md) | E2EE 房間裡 server 讀不到內容，所以附件由送訊息的請求**宣告** |
| [/docs/design/media/purge-release-set.md](media/purge-release-set.md) | 清歷史**只釋放它真的刪掉的**（外部審查 #3：原本被保留的事件 —— 房間頭像、本地圖片 —— 媒體會被刪掉）。§9：**每房成員頭像**有自己的 `RoomAvatar{room,user}` 持有者，否則上傳滿 7 天就被掃掉 |
| [/docs/design/media/media-gc.md](media/media-gc.md) | 📕 **歷史**：精確計數、哨兵、收集器、墓碑、`migrate-references`（計數與哨兵已退場） |
| [/docs/design/media/media-refcount.md](media/media-refcount.md) | 📕 **歷史**：最早的列式索引，已被計數取代、計數又被持有者集合取代 |

## `events/`：時間線、它的編號，與把它推出去

| 文件 | 講什麼 |
|---|---|
| [/docs/design/events/room-seq-and-recent.md](events/room-seq-and-recent.md) | 每房連續序號 `r_seq`、全域 `g_seq`、跨房間的「最近 N 則」（`Event/Recent`） |
| [/docs/design/events/event-push.md](events/event-push.md) | 連線訂閱自己的帳號，server 把新事件推過來 |
| [/docs/design/events/streaming-messages.md](events/streaming-messages.md) | Draft Message：草稿一開始就是一則真的佔位訊息，之後的變化只廣播不進庫 |

## `keys/`：E2EE

| 文件 | 講什麼 |
|---|---|
| [/docs/design/keys/e2ee-over-channel.md](keys/e2ee-over-channel.md) | E2EE 完全不靠 `/sync`：金鑰端點上橋、發 to-device、自己的金鑰存量 |
| [/docs/design/keys/to-device.md](keys/to-device.md) | `0x16 Device` 的訂閱、推送與銷毀。🚨 §2：水位是**佇列頭**，`Fetch` 不要帶 `cd_seq` |
| [/docs/design/keys/room-device-version.md](keys/room-device-version.md) | 裝置版本號、房間版本號、送出時比對（`1506`）、裝置變動廣播 |
| [/docs/design/keys/e2ee-send-guard-problem.md](keys/e2ee-send-guard-problem.md) | ⚠️ **問題書不是提案**：上一份要回答的題目與達標條件 |

## `accounts/`：server 自己擁有的帳號

| 文件 | 講什麼 |
|---|---|
| [/docs/design/accounts/server-user.md](accounts/server-user.md) | `@system`、顯示名稱 `[SYS] <server_name>`，以及怎麼防止它變成真人的帳號 |

## `history/`：留著因為推理還值得讀

| 文件 | 講什麼 |
|---|---|
| [/docs/design/history/review-followups-2026-09-06.md](history/review-followups-2026-09-06.md) | 📕 PR #24 合併後的再審與外部審查的逐條驗證（三支都已合併） |

---

📕 **標成歷史的文件不要照著實作** —— 它們留下來是因為「為什麼換掉」比「換成什麼」更難重建。
每一份的抬頭都寫著它被什麼取代了。
