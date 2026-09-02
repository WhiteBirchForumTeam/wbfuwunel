# 媒體的真正刪除：候選、寬限、墓碑、重建

> **狀態：提案，尚未實作。** 這份文件要先經維護者同意，才動 `src/`。
>
> 撰寫日期：2026-09-02。上位文件：[media-refcount.md](media-refcount.md)（索引已於 PR #5 合併），
> 更上位：[why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md) §5.4。
>
> 這是「引用計數」的**下半部**：上半部只建索引、一個 byte 都不刪；這一半才真的刪 bytes。
> 刪除不可逆，所以整份文件的每個選擇都先問同一句：**壞掉的時候，是垃圾留著，還是內容不見？**

---

## 1. 目標與範圍

拿到 §5.4 承諾的東西：**引用歸零的媒體，經過寬限期後真的從磁碟消失，容量因此有界。**

| 做 | 不做 |
|---|---|
| 判定「歸零且曾經被引用」的候選 | 🚫 跨使用者去重（E2EE 下不存在） |
| 寬限期 → 刪 bytes → 留墓碑 | 🚫 動事件 DAG |
| 墓碑讓 404 可區分 | 🚫 分塊 / Merkle（獨立的功能） |
| 啟動時一次性重建索引（讓舊資料不再是盲區） | 🚫 遠端快取媒體（照舊走既有 TTL，見 §6） |
| 預設**關閉**，明確開啟才會刪 | |

## 2. 核心問題：「曾經 ≥ 1、現在歸零」怎麼知道

索引的設計是**列本身就是計數**（[media-refcount.md](media-refcount.md) §3.1）。它答得了「現在有沒有人用」，
但答不了「以前有沒有人用過」—— 列刪光之後什麼痕跡都沒有。而 §3.5 的安全規則正是：
**只回收「曾經 ≥ 1、現在歸零」的，從未被引用過的完全不碰**（否則剛上傳、還沒送出的檔案會被刪）。

而且寫入端**不能讀**（`Txn` 是純寫入的 WriteBatch），所以移除引用的那一刻，程式**不知道**自己刪的是不是最後一列。

### 解法：一個只寫不讀的「候選」表

新增 column family **`mxc_gc_candidate`**：鍵 `mxc`，值 `最後一次移除引用的時間`。

- **每一個移除引用的地方**（redact、歷史清除、房間清除、換頭像、清頭像）**順手寫一列**，
  不判斷、不讀 —— 純寫入，塞得進它們既有的交易。
- **從未被引用過的媒體永遠不會出現在這張表**，因為沒有任何移除發生過 → §3.5 的保護**自動成立**，
  不需要額外的計時器或上傳時間。
- 真正的判斷交給 **worker**（它可以讀）：對每個候選，seek 一次 `mxc_holder` 前綴。

| 候選的狀態 | worker 做什麼 |
|---|---|
| 又有人引用了（seek 到列） | 刪掉候選，什麼都不做 —— 它回到「活著」 |
| 沒人引用、但還在寬限期內 | 跳過，下一輪再看 |
| 沒人引用、寬限期已過 | 刪 bytes → 寫墓碑 → 刪候選 |
| 找不到任何檔案鍵（已經被人手動刪了） | 寫墓碑 → 刪候選 |

⭐ 這個切法讓**所有需要讀的邏輯都集中在 worker 一處**，寫入端維持「只多寫一列」的零成本。

### 這樣安全嗎 —— 失敗方向逐條看

| 情境 | 結果 | 方向 |
|---|---|---|
| 候選寫了、worker 還沒跑、程式 crash | 候選留著，下次啟動再看 | ✅ 垃圾留著 |
| 移除引用成功、候選那筆沒寫到（不在同一交易時） | 媒體永遠不會被候選 → 永遠不刪 | ✅ 垃圾留著 |
| worker 判定歸零 → 刪 bytes 之間，有人**新引用**了它 | ❌ bytes 沒了、新事件指著它 | ⚠️ **內容不見** —— 見下 |
| 墓碑寫了、bytes 沒刪成 | 下一輪候選還在、再試；GET 回墓碑 | ✅ 可重試 |

⚠️ **唯一會「內容不見」的是第三列**：check-then-delete 的競態。`Txn` 不能讀，所以做不到原子的
「確認沒人用 → 刪」。壓縮這個窗口的辦法：

1. **寬限期本身**就把「剛歸零」和「真的刪」隔開，大多數的重新引用（例如刪錯了立刻重送）都落在寬限期內。
2. worker 在**刪 bytes 之前的最後一刻再 seek 一次** —— 把窗口縮到毫秒級。
3. **寬限期內的重新引用要重置計時**：任何 `add_*_refs` 順手**刪掉候選列**（純寫入，`txn.del`），
   於是媒體回到「活著」，下次歸零會重新開始算。

剩下的毫秒級窗口是這個設計的**已知殘餘風險**，寫在這裡而不是假裝沒有。要完全消除得改資料庫層
（帶讀的交易），那是另一個量級的改動，本階段不做。

## 3. 寬限期與設定

| 設定 | 預設 | 意義 |
|---|---|---|
| `media_gc_enabled` | **`false`** | 主開關。關著時 worker 只**記錄**會刪什麼，不刪（見 §7 的上線順序） |
| `media_gc_grace_seconds` | `604800`（7 天） | 歸零後多久才真的刪 |
| `media_gc_interval_seconds` | `3600` | worker 每隔多久掃一次候選 |

**為什麼不對齊 `redaction_retention_seconds`（60 天）**：那是「保留未 redact 的原文」的期限，
語意不同 —— 原文保留是為了管理員事後查證，媒體寬限是為了「刪錯了還能救回」。兩者可以獨立設定；
但**寬限期不該長於原文保留期**，否則會出現「原文還在、檔案沒了」。文件在 §8 待決 1 留這一題。

三個都 `reloadable: yes`，寫在 `src/core/config/mod.rs`，照 `redaction_retention_seconds` 的樣子宣告。

## 4. 墓碑

新增 column family **`mxc_tombstone`**：鍵 `mxc`，值 `(刪除時間, 原因)`。原因是一個小 enum：
`GarbageCollected`、`AdminDeleted`（未來 `!admin media delete` 也可以寫它）。

### 4.1 讀取端：讓 404 可區分

內容與縮圖的讀取都收束在 `media.db.search_file_metadata(mxc, dim)`（`get_stored` 與
`get_stored_thumbnail` 都呼叫它）—— 墓碑就擋在**這一個點的前面**：找不到檔案鍵時先查墓碑，
有墓碑就回「已刪除」，沒有才回原本的 `NotFound`。

**HTTP 表達**：`410 Gone` ＋ errcode 維持 `M_NOT_FOUND`。

- 為什麼 `410`：語意就是「曾經存在、已永久移除」，而且**不會**破壞客戶端 —— 它們對非 200 一律當失敗，
  差別只在錯誤訊息。
- 為什麼 errcode 不自訂：非聯邦環境雖然可以自由，但 Element 這類客戶端只認標準碼；自訂碼在它們眼裡
  跟未知錯誤沒兩樣。分辨的責任交給 HTTP 狀態碼 ＋ `error` 字串（`"Media was deleted on <date>"`）。
- 📎 實作細節：`Err!(Request(...))` 巨集**一律填 `BAD_REQUEST`** 當提示，而 `response::status_code`
  只在提示是 `BAD_REQUEST` 時才依 kind 換算。所以 410 要**直接建構**
  `Error::Request(ErrorKind::NotFound, msg, StatusCode::GONE)`，不能走巨集。

### 4.2 墓碑保留多久

RocksDB 的 column family 支援 `ttl`（descriptor 已有多處在用，例如 `mediaid_lazy`）。
建議 **`ttl = 365 天`**：一年內的「為什麼這張圖沒了」查得到，之後自然消失，表不會無限長。
📎 §8 待決 2。

### 4.3 墓碑與 `!admin media list-references` 的關係

`list-references` 對已刪除的 mxc 應該把墓碑也印出來（`deleted 2026-09-02 (garbage-collected)`），
否則管理員看到「Nothing references」會以為索引壞了。

## 5. 重建：讓「上線前的舊資料」不再是盲區

索引只認它上線之後發生的事（[media-refcount.md](media-refcount.md) 的唯一剩餘限制）。
**沒有重建就開 GC，第一次就會刪掉所有舊事件引用的媒體** —— 這是刪除功能的**閘門**，不是可選項。

### 5.1 做法：抄 `rebuild_typed_relations`

那條路已經存在（`src/service/rooms/pdu_metadata/purge.rs`）：`clear()` 整個 CF → `raw_stream()`
掃過全部 `pduid_pdu` → 逐筆重新索引；並在 `src/service/migrations/mod.rs` 用 `global` marker
（`db["global"].get(b"rebuild_relatesto_typed")`）保證**啟動時只跑一次**。

`media_refs` 照做：

1. `mxc_holder.clear()`。
2. 掃全部 `pduid_pdu`，對每筆用 `list_content_mxc_uris(content)` 重寫事件列（已 redact 的內容為空，
   自然不寫 —— 這正是 [media-refcount.md](media-refcount.md) §3.4 說「從內容重算是正確的」的理由）。
3. 掃全部本地使用者（`users.list_local_users()`），對每個 `profile.avatar_url()` 重寫頭像列。
4. marker：`db["global"].insert(b"rebuild_mxc_holder", [])`。

也提供 `!admin media rebuild-references` 隨時手動重跑（跟 `rebuild_typed_relations` 一樣有 admin 入口）。

### 5.2 重建期間不能刪

worker 在**重建進行中**看到的索引是不完整的（清空到掃完之間）。所以：**重建期間 worker 必須暫停**，
用一個記憶體內的旗標即可（`AtomicBool`），重建結束才放行。
⚠️ 而且 `mxc_gc_candidate` **不在重建範圍內** —— 它記的是「何時歸零」，重建重算不出來；留著即可，
worker 之後看到「其實還有人引用」就會把候選刪掉。

## 6. 只碰本地媒體

候選與刪除都只對**本地** mxc（`mxc.server_name == 我們的 server_name`，媒體服務已有 `is_local()`）。
遠端快取的媒體照舊走既有的 TTL 路徑（`mediaid_lazy` 等），不納入引用計數 —— 那些是快取，本來就會過期，
而且它們的「真正持有者」在另一台 server 上，我們算不準。📎 這回答了 [media-refcount.md](media-refcount.md) §4 待決 5。

## 7. 上線順序（每一步都可以停下來）

```
① 合併：預設 media_gc_enabled = false
        ↓  啟動時自動重建索引（一次），worker 只記錄「會刪什麼」
② 觀察：用 !admin media list-references 抽查幾筆舊資料，確認重建後看得到引用
        ↓  看 worker 的 dry-run 日誌，確認候選清單合理
③ 開啟：media_gc_enabled = true
        ↓  第一輪只會刪「已歸零且超過寬限期」的
④ 驗證：找一個被刪的 mxc，GET 應回 410 + 墓碑；list-references 印出墓碑
```

**dry-run 模式**（`media_gc_enabled = false` 時 worker 仍跑，但只 `info!` 不刪）是刻意的：
讓維護者在真的刪之前**看到它打算刪什麼**。這比「關著就完全不動」更有用，也更安全。

## 8. 待決（需要維護者決定）

1. **寬限期預設 7 天對嗎？** 以及要不要強制 `media_gc_grace_seconds ≤ redaction_retention_seconds`。
2. **墓碑 TTL 一年對嗎？** 還是永久（表會單調成長，但每列很小）。
3. **`410 Gone` 可以接受嗎？** 另一個選項是維持 404 只改 `error` 字串 —— 更保守，但客戶端完全分不出來。
4. **`delete_by_event` 讀源**（[media-refcount.md](media-refcount.md) §4 待決 4，前一階段留下的）：
   要不要在這一支順便改成讀 `retention.get_original_pdu()`？我傾向**分開**，它是獨立的行為修正。
5. **競態的殘餘風險**（§2 第三列）可以接受嗎？替代方案是等資料庫層有帶讀的交易再做，那會讓這整個
   階段延後到不知何時。

## 9. 測試計畫

| 層 | 內容 |
|---|---|
| 單元 | 候選判定的純函數（歸零＋過期 → 刪；歸零未過期 → 等；有引用 → 撤銷候選）；墓碑的序列化來回；410 的錯誤建構 |
| 端到端（真伺服器，抄 PR #5 那套腳本） | 送圖 → redact → 候選出現（dry-run 日誌） → 把寬限期設成 0 → 開 GC → bytes 消失 → GET 回 410 → list-references 印墓碑 → **再送一則引用同一 mxc 的訊息** → 應回 410 而非 200（確認墓碑不會被新引用「復活」）|
| 端到端（重建） | 用**索引上線前**的舊資料庫（PR #5 之前的 `testdb` 還在）啟動 → 確認 marker 觸發重建 → list-references 看得到舊事件 |
| 變異 | 只打候選判定那個純函數 |

⭐ 端到端裡「再送一則引用已刪 mxc 的訊息」那條很重要：它驗的是**墓碑是終局**——
被刪的媒體不能因為有人再貼一次 mxc 就變回「活的」。實作上 `add_event_refs` 不需要查墓碑（它不能讀），
但 GET 會先看墓碑，所以結果仍是 410。這條測試把這個推論釘死。
