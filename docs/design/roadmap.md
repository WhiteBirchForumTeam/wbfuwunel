# 開發路線圖

> **這份文件回答：接下來做什麼、可能做什麼、明確不做什麼。** 它是給維護者排優先序用的，
> 不是設計文件 —— 每一項的「為什麼」與「怎麼做」在它指向的那份文件裡，這裡只講順序、狀態、前提。
>
> 狀態標記：✅ 已合併 · 🔧 進行中 · 📄 有提案待同意 · 🔲 下一步 · 💭 候選（還沒決定要不要做）· 🚫 明確不做。
> 每一項改狀態時順手改這裡；這裡的狀態如果跟 [`CHANGELOG-fork.md`](../../CHANGELOG-fork.md) 對不上，以 CHANGELOG 為準。
>
> 最後更新：2026-09-03。

## 0. 目標，一句話

一套**由維護者完全掌控**的即時通訊服務：資料存在哪、留多久、誰讀得到，由維護者決定；
容量有界、算得出來；大檔案與串流是一等公民。完整的理由與非目標在
[why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md)（下稱「核心設計」）。

**分岔會很大，而且會持續變大。** 與 Matrix 規格相容不是目標；上游持續合併進來，方向由這裡決定。

## 1. 已經做完的

| 項目 | 狀態 | 去哪看 |
|---|---|---|
| fork 定位、分支模型、改動流程、Windows 建置 | ✅ | [fork-overview.md](fork-overview.md)、[windows-build.md](windows-build.md) |
| 媒體引用計數（精確計數、merge operator、哨兵） | ✅ PR #7 | [media-gc.md](media-gc.md) §2、§4 |
| 媒體的真正刪除：收集器、墓碑 410、`migrate-references` | ✅ PR #10 | [media-gc.md](media-gc.md) §3、§5、§6 |
| 每個 mxc 一鎖，關掉收集器與同時 +1 的窗口 | ✅ PR #12 | [media-gc.md](media-gc.md) §3.3 |
| 分塊上傳／下載 B 支：WebSocket 通道、上傳中間層（進度列＋記憶體＋每上傳一鎖）、規格黃金向量 | ✅ PR #18 | [wbf-wire-format.md](wbf-wire-format.md) §6.1、[chunked-upload.md](chunked-upload.md) §3.1、[wbf-vectors.json](wbf-vectors.json) |
| 分塊上傳／下載 A 支：wbf pack、`EncryptedFileInfo`、有序上傳與續傳、串流模式、按塊下載、sweeper、`POST /_wbf/v1/pack` | ✅ PR #16 | [chunked-upload.md](chunked-upload.md)、[chunked-upload-spec.md](chunked-upload-spec.md)、[wbf-wire-format.md](wbf-wire-format.md) |

這三支合起來就是核心設計 §5.4「刪除語意」的 server 端，**寬限期被維護者拿掉了**（立刻生效），
TTL 兜底改成「不確定就持有」加 `migrate-references` 重算。

**現在成立的性質**：媒體的壽命等於引用它的訊息（含原文備份）的壽命。訊息消失，媒體立刻消失。
用量等於實際保留的內容，沒有漏水。

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

## 3. 候選（要不要做，由維護者決定）

| 項目 | 一句話 | 前提 / 觸發條件 |
|---|---|---|
| 💭 歷史保留政策 | 事件超過 N 天自動 purge（config ＋ 每房間覆寫 ＋ 一個 worker 呼叫既有的 `purge_history`），原文備份跟著同一期限走 | 維護者 2026-09-03 的看法是**事件一直長是自然的，不必加**。列在這裡是因為若哪天要「算得出來的容量」變成「有上限的容量」，這是唯一的開關 |
| 💭 遠端媒體快取 TTL | 別台伺服器的媒體被抓來快取後沒有過期時間，收集器也不碰它 | **只有打開聯邦才會發生**（`allow_federation = false` 時連出去的請求在 `federation/execute.rs` 就被擋）。開聯邦之前必做 |
| 💭 RocksDB 空間回收 | 刪除只寫 tombstone 記錄，空間靠 compaction；大量清理後可能要手動 compaction 或調 periodic compaction | 第一次在真實資料上跑 `migrate-references` 之後量一次 |
| 💭 「備份存在時 purge 只釋放一次」的 Services 級測試 | 雙路徑互斥目前只有 e2e 涵蓋 | 需要能在測試裡建起 Services 的夾具；有了夾具很多「靠讀碼確認」的東西都能變測試 |
| 💭 admin 指令顯示「誰引用」 | 列式索引退場後只剩數字 | 維護者當時明說要數字；除非除錯時真的需要，不做 |

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
