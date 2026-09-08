# 開發路線圖

> **這份文件回答：接下來做什麼、可能做什麼、明確不做什麼。** 它是給維護者排優先序用的，
> 不是設計文件 —— 每一項的「為什麼」與「怎麼做」在它指向的那份文件裡，這裡只講順序、狀態、前提。
>
> 狀態標記：✅ 已合併 · 🔧 進行中 · 📄 有提案待同意 · 🔲 下一步 · 💭 候選（還沒決定要不要做）· 🚫 明確不做。
> 每一項改狀態時順手改這裡；這裡的狀態如果跟 [`CHANGELOG-fork.md`](../../CHANGELOG-fork.md) 對不上，以 CHANGELOG 為準。
>
> 最後更新：2026-09-07（PR #30 合併後）。下一步：§2.6 的 `media/upload-lifecycle`，等維護者的 checklist。

## 0. 目標，一句話

一套**由維護者完全掌控**的即時通訊服務：資料存在哪、留多久、誰讀得到，由維護者決定；
容量有界、算得出來；大檔案與串流是一等公民。完整的理由與非目標在
[why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md)（下稱「核心設計」）。

**分岔會很大，而且會持續變大。** 與 Matrix 規格相容不是目標；上游持續合併進來，方向由這裡決定。

## 1. 已經做完的

| 項目 | 狀態 | 去哪看 |
|---|---|---|
| fork 定位、分支模型、改動流程、Windows 建置 | ✅ | [fork-overview.md](fork-overview.md)、[windows-build.md](windows-build.md) |
| 媒體引用計數（精確計數、merge operator、哨兵）—— **已被 #24 的持有者集合取代** | ✅ PR #7 → 退場 | [media-gc.md](media-gc.md) §2、§4（歷史） |
| 媒體的真正刪除：收集器、墓碑 410、`migrate-references`（migrate 已在 #24 拔掉） | ✅ PR #10 | [media-gc.md](media-gc.md) §3、§6 |
| 每個 mxc 一鎖，關掉收集器與同時加持有者的窗口 | ✅ PR #12 | [media-gc.md](media-gc.md) §3.3 |
| 分塊上傳／下載 B 支：WebSocket 通道、上傳中間層（進度列＋記憶體＋每上傳一鎖）、規格黃金向量 | ✅ PR #18 | [wbf-wire-format.md](wbf-wire-format.md) §6.1、[chunked-upload.md](chunked-upload.md) §3.1、[wbf-vectors.json](wbf-vectors.json) |
| 分塊上傳／下載 A 支：wbf pack、`EncryptedFileInfo`、有序上傳與續傳、串流模式、按塊下載、sweeper、`POST /_wbf/v1/pack` | ✅ PR #16 | [chunked-upload.md](chunked-upload.md)、[chunked-upload-spec.md](chunked-upload-spec.md)、[wbf-wire-format.md](wbf-wire-format.md) |
| 每房 `r_seq`、全域 `g_seq`、`Event/Recent` | ✅ PR #22 | [room-seq-and-recent.md](room-seq-and-recent.md) |
| **媒體持有者集合**取代計數；附件隨送訊息宣告（header／`Event/Send`）；7 天掃描；舊 client 一次性警告；fork 自報引擎名 | ✅ PR #24 | [media-holders.md](media-holders.md)、[media-attachments.md](media-attachments.md) |

媒體這幾支合起來是核心設計 §5.4「刪除語意」的 server 端，**沒有寬限期**（訊息消失媒體立刻消失）；
「不確定就持有」現在的形狀是：**沒有 `mxc_managed` 列的既存媒體永不自動刪**，沒有重算工具。

**現在成立的性質**：這個模型上線後上傳的媒體，壽命等於持有它的東西（事件、原文備份、頭像）的壽命；
上傳後 7 天內沒有任何持有者的會被掃掉。用量等於實際保留的內容加上既存媒體，沒有漏水。

## 2. 下一步（順序已定）

### 2.1 ✅ 媒體層：分塊上傳、續傳、按塊下載（A 支 #16、B 支 #18 都已合併）

核心設計 §5.2 的主體，也是 fork 的第一個「使用者看得到」的功能。提案 [chunked-upload.md](chunked-upload.md) 維護者 2026-09-03
同意（PR #15）；**A 支（pack、上傳、下載、sweeper、HTTP 一包一請求）已合併（PR #16，2026-09-03）；B 支（WebSocket 通道 `/_wbf/v1/ws`、上傳中間層、規格黃金向量 `wbf-vectors.json`）已合併（PR #18，2026-09-04）**。server 端到這裡就算能用；下一步是 client（§4），server 側留的是主動推送（邊上傳邊下載）、發送拆 mpsc task、benchmark。
pack 格式與 kind 分配在 [wbf-wire-format.md](wbf-wire-format.md)，**未來所有 HTTP 請求都會遷到這條通道**。

- **範圍**（最終模型，見 [chunked-upload-spec.md](chunked-upload-spec.md)）：`Create` 一次宣告（16 byte `EncryptedFileInfo` ＋ client 加密的檔案描述）；塊嚴格有序、`seq` 即塊索引、server 不判斷密文長度只記位置；串流模式；單檔上限與截斷；下載按塊或明文位置一次整塊交回。完整性是 client 的 AEAD 標籤加 pack 的 CRC-32C，**沒有 Merkle**（早期草案的殘留，維護者 2026-09-03 改用 CRC）。
- **前提**：核心設計 §7 待驗 3（塊大小）要先量；§7 待驗 1（要不要 DAG）**不擋這一步** —— 媒體層不依賴事件層的形狀。
- **接縫**（見 [repo-structure.md](repo-structure.md)）：`src/api/client/media.rs` 新端點、`src/service/media/` 分塊與樹的邏輯、
  `src/database/maps.rs` 塊索引、`src/service/storage/` 既有的 object_store 抽象放 bytes。
- **與引用計數的關係**：一個分塊媒體仍是一個 mxc、一個計數；塊是 mxc 底下的東西，`media.delete()` 的前綴刪除順手連塊一起刪。
  這是為什麼刪除要先做：分塊上去之後，孤兒的體積會是現在的十倍百倍。
- **流程**：先寫提案 → 維護者同意 → 開分支。提案要回答塊大小、pending upload 的過期（既有 `mediaid_pending` 可參考）、
  未完成上傳的清理（跟 `media_gc_migrate_skip_recent_seconds` 是同一個問題）。

### 2.2 🔲 串流播放（分塊之後自然得到）

分塊做完，串流播放就是客戶端「用明文位置要對的塊」；server 端已經沒有要補的了，B 支的 WebSocket 只是把同一套 pack 換條線。
不另開項目，併在 2.1 的驗收裡：**一個大於 1 GB 的檔案，中途 seek 不必下載前面的部分。**

### 2.3 📄 流式訊息（文字 token 串流）

提案已在 [streaming-messages.md](streaming-messages.md)，核心設計 §5.3。走短暫訊息旁路，講完才寫一個正式事件，歷史零污染。
程式碼落點對照 `rooms/typing/` 與 sync 喚醒，提案 §3 已寫。等維護者同意就能開分支；它與媒體層互不依賴，可以並行。

### 2.4 ✅ 每房 `r_seq`、全域 `g_seq` 與 `Event/Recent`（issue #20，PR #22 已合併 2026-09-06）

client（wbf-matrix-client）的聊天模型要 server 配合的兩件事。設計 [room-seq-and-recent.md](room-seq-and-recent.md)：
兩個位置寫進存起來的 PDU 的 `unsigned`（一個寫入點、所有讀路徑自動帶）、startup migration 回填既有 room；
`Event/Recent` 依 client 快取的 `cg_seq` 只回差異，k 路合併不加索引。順手加了 `[profile.e2e]`（windows-build.md）。


### 2.5 ✅ E2EE 下的媒體引用 → 持有者集合（提案 PR #23，實作 PR #24，2026-09-06 合併）

破口：計數的 +1 來自 server 讀 content，E2EE 房間讀不到。定案分兩層：**來源**換成送訊息時宣告 `attachments`
（[media-attachments.md](media-attachments.md)），**形狀**換成外鍵集合（[media-holders.md](media-holders.md)），
`migrate-references` 拔掉，既存媒體永不自動刪。**client 端必須同步**（spec §12），否則 E2EE 房間的附件 7 天後被掃掉。

### 2.6 🔧 合併後再審與外部審查的修補（[review-followups-2026-09-06.md](review-followups-2026-09-06.md)），三支，維護者 2026-09-06 同意

PR #24 合併後對 main 重看一次，加上 `../external-review` 兩輪（2026-09-05）逐條對現在的程式碼驗證；外部審查 14 條裡 4 條已由 #24 修掉、8 條仍在、1 條降級、1 條文件講反話，另抓到 1 條新的 P1。

| 分支 | 內容 | 狀態 |
|---|---|---|
| `media/managed-origin` | 既存媒體被縮圖拉進 `mxc_managed` 後 7 天被掃（P1，只有 `create`／Seal 確立受管）；`(mxc, Interfix)` 前綴；頭像幽靈持有者；墓碑 TTL 文件 | ✅ PR #26 |
| `wbf/auth-and-ws-lifetime` | `/_wbf/*` 與 WebSocket 不查帳號鎖定（P1）；WebSocket 在登出／到期後仍有權限、關機時 `State` 懸空（P1） | ✅ PR #28 |
| `media/upload-lifecycle` | Seal 在本地儲存收整檔進記憶體、S3 的 1 MiB parts（P1）；`Status` 冷載入不持鎖、sweeper 鎖下不重讀進度（P2）；升級前舊上傳卡配額（P2） | 🔲 |

### 2.7 ✅ WS 的 `Login`／`Refresh`／`Logout`（提案 #29，實作 PR #30，2026-09-07 合併）

維護者 2026-09-06 提出：登入該有 WS 專用的 pack 格式，升級帶 Bearer 可以留著。定案：kind `0x10 Session`（§3.3 早就留給登入這一章）下三個 subtype，meta 沿用 Matrix `/login` 的請求體、
登入本體抽成 service 函式讓 HTTP 與 WS 共用；允許不帶 Bearer 升級但只接受 `Hello`／`Ping`／`Login`／`Refresh`，**未登入 30 秒就斷**（從升級起算，不是 idle）；
Logout 後直接關線；同一條連線重複 Login 允許；**登入限速預設開**（每秒 1、突發 10），同一個 token bucket 管 HTTP `/login`／`/refresh` 與 WS。排在 `media/upload-lifecycle` 之前。

### 2.8 ✅ wbf pack 處理管線第一部分：連線即佇列、每 device 4 條、發送 task、`Event/Batch` 串流（提案 #32，實作 PR #33，2026-09-08 合併）

維護者 2026-09-07 的 checklist：protocol 講了封包與協議，沒講「設計」——一個 pack 從連線進到回應出的整條線，上傳、登入與未來每個 HTTP→WS 都走它。
定案：client 多連線分工（server 不知道）；每個 (user, device) 最多 4 條 WS，超過踢新的，匿名不算、HTTP 不算；一條連線依序處理、落地才 Ack；
`Recent` 是 client 拉的視窗（預設 320 條），在 WS 上拆成 `Event/Batch` 串流（meta `{tc, bc, fs, ls, r}`，`tc` 是一窗的條數、`r = 0` 一窗結束），下一窗帶 `before`，HTTP 回 `Unsupported`。
第一部分（§1–§6）已合併；三處跟提案不同（名額在發 token 前拿、一趟收齊一窗、一問一答 handler 保留簽名）在文件裡標 📎。
**第二部分**（§8：review-followups §2.5 → §2.2 → §2.8，上傳生命週期）🔲 未開，順序與同支／另開由維護者定。client 端要跟的東西：wbf-matrix-client #15。

## 3. 候選（要不要做，由維護者決定）

| 項目 | 一句話 | 前提 / 觸發條件 |
|---|---|---|
| 💭 歷史保留政策 | 事件超過 N 天自動 purge（config ＋ 每房間覆寫 ＋ 一個 worker 呼叫既有的 `purge_history`），原文備份跟著同一期限走 | 維護者 2026-09-03 的看法是**事件一直長是自然的，不必加**。列在這裡是因為若哪天要「算得出來的容量」變成「有上限的容量」，這是唯一的開關 |
| 💭 遠端媒體快取 TTL | 別台伺服器的媒體被抓來快取後沒有過期時間，收集器也不碰它 | **只有打開聯邦才會發生**（`allow_federation = false` 時連出去的請求在 `federation/execute.rs` 就被擋）。開聯邦之前必做 |
| 💭 RocksDB 空間回收 | 刪除只寫 tombstone 記錄，空間靠 compaction；大量清理後可能要手動 compaction 或調 periodic compaction | 第一次大量刪房或 purge 之後量一次（`migrate-references` 已拔掉） |
| 💭 Services 級的測試夾具 | 「可刪」決策（本地 ∧ 有 `mxc_managed` ∧ 無持有者）、purge 與備份到期的冪等，目前只有 e2e 涵蓋（雙扣本身已被集合語意消掉） | 需要能在測試裡建起 Services 的夾具；有了夾具很多「靠讀碼確認」的東西都能變測試 |
| ✅ admin 指令顯示「誰持有」 | `!admin media refcount <mxc>` 從 #24 起印持有者清單與是否受管 | 持有者集合天然有這個答案，不用另做 |

## 4. 大的未定（核心設計 §7）

這些不是功能，是會改變後面每一步形狀的決定。**還沒定，也不急著定**，但每次要開一個新的大項目前先看一眼它們有沒有變成擋路的。

1. **不聯邦的話，事件層還需不需要 DAG？** 沒有遠端分叉，房間可以退化成單調遞增的日誌，刪除與容量都簡單得多；代價是放棄可驗證歷史與未來接聯邦。
   **媒體層不等它**；同步語意（核心設計 Phase 1）等它。
2. **塊大小**：1 MiB 或 4 MiB，拿實際網路與記憶體去量。擋 2.1。
3. **串流文字在 E2EE 下的金鑰安排**。擋 2.3 在加密房間的部分，不擋明文房間。
4. **客戶端策略**：自己從 SDK 寫小的，還是站在現成的上。核心設計 Phase 2，最後才做。

## 5. 明確不做

- 🚫 **server 端內容過濾／轉檔**：E2EE 下只看得到密文，做不到（核心設計 §5.4、§5.5）。
- 🚫 **與 Matrix 規格相容**：一旦要相容，每個改進都得走 MSC 流程。
- 🚫 **fork 成熟客戶端**：拿到的是 UI 的掌控權，不是系統的（核心設計 Phase 2）。
- 🚫 **改 crate 名、binary 名、設定路徑**：跟上游每次 merge 都衝突，永久的（[fork-overview.md](fork-overview.md)）。
- 🚫 **刪除的寬限期**：維護者要立刻生效；「刪錯能救」的答案是重新上傳。

## 6. 每一項怎麼進來

不論大小，一樣的四步（[fork-overview.md](fork-overview.md)）：`docs/design/` 寫提案 → 維護者同意 → 開分支 → PR 進 `main`。
feat、重大 fix、refactor 合併後在 [`CHANGELOG-fork.md`](../../CHANGELOG-fork.md) 兩張表各留一列，並回來改這裡的狀態標記。
