# wbfuwunel `main` —— 這個 fork 的整合分支

`main` 是這個 fork 實際運行的分支：**上游 `tuwunel` 的 `main`，加上下面列出的每一條功能分支**。
這份檔案只記這個 fork 加的東西，不記上游的變更；上游自己的紀錄看 [`RELEASE.md`](RELEASE.md)。

- **`upstream/main`**：上游 [`matrix-construct/tuwunel`](https://github.com/matrix-construct/tuwunel) 的遠端追蹤 ref，永遠不帶本地改動。
- **`upstream-main`**：上游的本地鏡像分支，只快轉。
- **`main`**：上游 + 所有已合併的功能分支（這條）。改動只透過 PR 進來。

> 📌 **這是「這個 fork 到底跟上游差在哪」的唯一權威清單。** `git log --oneline upstream/main..main`
> 只看得到動了什麼，看不到**為什麼**。每一條合併進 `main` 的 **feat、重大 fix、refactor** 分支都要在下面兩張表各留一列；
> 純文件的分支（`docs/*`）不列，它們的產物就在 `docs/design/`，README 那張表是索引。

## 怎麼維護

- 跟上游同步：`git fetch upstream && git branch -f upstream-main upstream/main`，然後在 `main` 上
  `git merge upstream/main`。🚫 **不 rebase、不 force push、不改已推出去的歷史。**
- 開新功能：先在 `docs/design/` 寫方案 → 維護者同意 → 開分支 → 開 PR（目標 `main`）。
- PR 合併之後才寫這裡，而且只寫 feat、重大 fix、refactor。提案階段的東西留在 `docs/design/`。
- **合併紀錄那張表的「備註」要寫為什麼，不只寫做了什麼。** 三個月後讀的人需要的是決定的理由、
  踩到的坑、行為的變化；「加了 X」從 commit 訊息就看得到。
- ⚠️ **行為有變、或有已知限制，一定要寫進來。** 這裡是使用者與維護者會先看到的地方，藏在設計文件
  深處的警告等於沒寫。
- 測試（Windows）：單元 `cargo test -p <crate> --lib`；release build 與實跑見
  [`docs/design/windows-build.md`](docs/design/windows-build.md)；端到端用真伺服器跑（腳本在各 PR 說明裡）。

## 共用程式碼的不變式

`src/service/media_refs/` 是**媒體引用的唯一寫入點**：任何東西開始或停止引用一份媒體，都透過它的
`add_event_refs` / `del_event_refs` / `set_avatar_ref` 記帳，不自己碰 `mxc_refcount`。
目前七個呼叫點：事件寫入（`timeline/append.rs`）、backfill（`timeline/backfill.rs`）、redact
（`timeline/redact.rs`）、歷史清除（`timeline/purge.rs`）、事件刪除（`timeline/pdus.rs`）、
retention worker 丟備份（`retention/mod.rs`）、頭像（profile 寫入點）。

📌 **新增一個寫 `pduid_pdu` 的地方，就欠這個帳一筆。** 漏掉的那個呼叫點會讓那些事件的媒體讀成
「無人引用」，而刪除是不可逆的方向。

## 功能分支

| 分支 | 做什麼 | 關鍵檔案 |
|---|---|---|
| `media/gc-delete` | **媒體的真正刪除。** 每個 −1 在交易 commit 後把 mxc 交給收集器（`Txn::on_execute` 掛鉤），收集器照「MIN < 計數 ≤ 0 就刪」動手：刪 bytes、寫墓碑、刪計數列同一交易，之後 GET 回 **410 Gone**（墓碑是終局，365 天 TTL）。負數照刪但先 `error!`；遠端媒體不碰；`media_gc_enabled = false` 只印 log。`!admin media migrate-references [--dry-run]` 離線重算（事件＋原文備份＋本地頭像），哨兵消失、本地孤兒刪除、太新的上傳跳過。 | `src/database/txn.rs`（`on_execute`）、`src/service/media_refs/{mod,collect,migrate}.rs`、`src/service/media/{mod,data,thumbnail}.rs`（`collect`、`Tombstone`、410 傳遞）、`src/admin/media/{migrate_references,refcount,delete}.rs`、`src/core/config/mod.rs`（`media_gc_enabled`、`media_gc_migrate_skip_recent_seconds`） |
| `media/refcount-counter` | **精確的媒體引用計數。** `mxc_refcount: mxc → i64`，±1 走 RocksDB merge operator，是一筆純寫入、排進事件或 profile 既有的交易；讀取時才合併，所以 worker 讀到的是數字不是前綴 seek。redact 不歸零：`save_unredacted_events` 保留的原文備份才是持有者，備份被丟掉的地方才 −1。既存媒體第一次被 ±1 變成哨兵（`i64::MIN`），之後所有 ±1 都被吞掉，等 migrate 重算。**這一版仍不刪任何 byte。** | `src/database/engine/merge.rs`、`src/database/txn.rs`（`Txn::merge`、walker 認得 merge 記錄）、`src/database/engine/descriptor.rs`（`MergeKind`）、`src/service/media_refs/`、`src/service/rooms/retention/mod.rs`（`drop_original`）、`src/admin/media/refcount.rs` |
| `media/refcount-index` | **已被上面那條取代，留作紀錄。** 列式索引 `mxc_holder`（鍵 `mxc ‖ 持有者種類 ‖ 持有者 id`），一列一個持有者，前綴 seek 答「還有沒有人用」。取代的理由：移除引用的當下答不出「現在幾個」，只能事後 seek，那不算計數。CF 已標 `DROPPED`。 | `src/service/media_refs/`（介面保留）、`src/core/matrix/media_ref.rs`（已移除） |

## 合併紀錄

| 日期 | 分支（PR） | 合併 | 備註 |
|---|---|---|---|
| 2026-09-02 | `media/refcount-index`（#5） | ✅ | **上半：只建索引，不刪任何東西。** 起因是 redaction 只把事件剝空、完全不碰媒體 bytes，而且沒有任何索引把事件連到媒體，所以媒體比引用它的每一則訊息都活得久（設計文件 [media-refcount.md](docs/design/media-refcount.md)）。新增服務 `media_refs` 與 CF `mxc_holder`，兩種持有者：事件（寫入／backfill／redact／歷史清除／房間清除五處維護）與個人頭像（設定頭像與停用帳號兩處）。房間頭像是 state 事件，本來就走事件那條。新增 `!admin media list-references`，沒有它「索引是對的」就是不可否證的宣稱。審查抓到 backfill 的 `raw_put` 繞過索引，同支修掉（`c568e77c0`）。真伺服器端到端驗過五個檢查點（多重引用、頭像、redact、刪房間）；變異測試 8/8。**已知限制**：索引只認上線之後的事，舊事件與頭像顯示為無人引用，所以重建工具必須跟刪除一起來。 |
| 2026-09-02 | `media/refcount-counter`（#7） | ✅ | **精確計數取代列式索引，仍不刪任何 byte。** 設計文件 [media-gc.md](docs/design/media-gc.md)：第一版（候選表＋寬限期）被維護者推翻（「redact 當下答不出計數，只能事後 seek，那不算計數」），定案是哨兵懶惰植入零掃描、migrate 離線、上傳不是引用、沒有寬限期。引擎層加 counter merge operator（`engine/merge.rs`，operand 9 bytes：tag + i64，`Init`/`Add`/`Set`；只提供 full merge，partial 一律拒絕）、`Txn::merge`、descriptor 的 `MergeKind`；`mxc_holder` 標 `DROPPED`，新 `mxc_refcount`（Counter）與 `mxc_tombstone`（TTL 365 天，尚未使用）。**⚠️ 三條行為都要知道**：(1) **redact 不歸零** —— `save_unredacted_events`（預設開）保留的原文備份才是持有者，備份被丟掉的地方就是 −1 的地方（retention worker reap、`purge_history` 丟備份，都從備份內容讀 mxc 再扣，同一交易；備份關閉時 redact 當下扣），所以媒體活得跟「管理員還看得到那則訊息」一樣久；(2) **既存媒體不重算也不會被自動清** —— 第一次被 ±1 變成哨兵 `i64::MIN`，`!admin media refcount` 印「no reference count」或「sentinel」，兩者都不是 0；(3) **上傳不是引用** —— `create_file_metadata` 只把列開在 0。**e2e 抓到的 bug（單元＋型別＋build 全綠都沒抓到）**：`Txn::execute` 走過 WriteBatch 的原始表示通知 watcher，`next_record` 只認 put/delete，遇到 merge 記錄（0x2/0x6）回 `None` → `Keys::next` panic → 上傳、換頭像全 500。修法：`Tag` 加 `Merge`/`CfMerge`（版面同 Value），並把原本拿 0x2 當「不認得」的測試改成 0x3。📎 教訓：**加一種寫入原語，要掃所有「解析 WriteBatch 表示」的地方。** 審查（rumia、cirno、salvia 三份 APPROVE）後補：DB 級 round-trip 測試（真 RocksDB 透過 descriptor 掛點註冊 operator，flush＋compact 後再驗）；`drop_original` 先讀備份再另一交易刪的讀後刪競態，用 `retention::Service` 一把 `tokio::sync::Mutex<()>` 從讀到 execute 全程持有關掉（一把全域鎖，因為兩個呼叫端都低頻）；負數計數**定案照刪**（規則就是 MIN < 計數 ≤ 0 就刪），但它是配平 bug 的訊號，worker 刪前要 `error!`。**尚缺的測試**：「備份存在時 purge 只釋放一次」要 Services 才跑得動，目前只有 e2e 涵蓋。`list-references` 改名 `refcount`。 |
| 2026-09-03 | `media/gc-delete`（#10） | ✅ | **照計數動手的那一半。** 設計文件 [media-gc.md](docs/design/media-gc.md) §3.1、§5、§6 同步改成「已實作」。**`Txn::on_execute`**：交易寫入落地、通知 watcher 之後才跑登記的閉包，空 batch 與被丟掉的交易不跑 —— 這是「−1 之後才通知收集器」的保證，七個呼叫點零改動（真 DB 單元測試）。**收集器**：`media_refs` 服務的 worker 收 `mpsc` channel，讀計數，MIN < 計數 ≤ 0 就 `media.collect()`；負數在規則內、照刪，但先 `error!`（配平 bug 的訊號）；遠端媒體留給既有 TTL；`media_gc_enabled = false` 只印「would have deleted」。**墓碑**：`Cbor(Tombstone { deleted_at_secs, reason })`，reason 是 `GarbageCollected`／`Migrated`／`AdminDeleted`，與刪計數列同一交易；擋點在 `Data::search_file_metadata` 開頭回 `Error::Request(NotFound, msg, 410)`，`get`、`get_stored`、`get_or_fetch`、`get_thumbnail` 與縮圖遠端回退都用 `is_gone()` 把 410 原樣傳出。**`migrate-references`**：記憶體重算 `pduid_pdu` ＋ **`retained_pdus_raw()`（原文備份才是持有者，漏了會把已 redact 但備份還在的媒體全當孤兒）** ＋ 本地頭像；非 dry-run 時 `clear()` 後 `Set(計數)` 寫回（沒引用寫 0 不留空，每 1000 列一交易），本地且 ≤ 0 的孤兒刪除，mtime 在 `media_gc_migrate_skip_recent_seconds` 內或讀不到的跳過；worker 期間以 `CollectorPause` Drop guard 暫停（panic 也復位）。**e2e 抓到兩件、單元＋型別＋build 都抓不到**：(1) `get_or_fetch` 用 `if let Ok` 吞掉 GONE 再回「not found」，內容 GET 變 404；(2) 強制殺程序丟掉剛寫的墓碑 —— 引擎 `manual_wal_flush = true`，每筆寫入後才手動 flush，別處持 cork 時那次 flush 被略過，程序內的 WAL 緩衝隨 kill 消失；所有寫入都有這窗口、正常關機會 flush，但 bytes 已刪的紀錄不該是被丟掉的那一半，所以 `collect()` 後多付一次 `engine.flush()`，測試腳本改用 `--execute "server shutdown"` 正常關機。真伺服器 e2e 四情境：計數歸零立刻 410（內容與縮圖）、事後再引用同一 mxc 仍 410、從未引用的上傳留著直到 migrate、保留原文時 reap 後才刪、gc 關閉不刪、舊庫 dry-run 掃到 2 份原文備份只刪真孤兒。三份審查（rumia、cirno、salvia）APPROVE，後補：Drop guard、admin 輸出加「離線維護」警語、`overwrite_counts` 的 O(n×m) 換 `HashSet`。**留給之後**：收集器「讀計數 → 刪 bytes」與同時 +1 的毫秒級窗口（每個 mxc 一鎖，等維護者同意）；purge-releases-once 的 Services 級測試。 |
