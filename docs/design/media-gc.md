# 媒體的真正刪除：精確計數、哨兵、立即清理、migrate

> **狀態：提案，尚未實作。** 這份文件要先經維護者同意，才動 `src/`。
>
> 撰寫日期：2026-09-02（第三版：第一版「候選表」被推翻；第二版改成精確計數；第三版依維護者指示
> 把哨兵改成**懶惰植入**、migrate 定為離線作業）。
> 上位文件：[media-refcount.md](media-refcount.md)（PR #5 的列式索引）；
> 更上位：[why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md) §5.4。
>
> ⚠️ **這一版改變了計數的形狀**：PR #5 的「列就是計數」被**精確的有號整數**取代，理由在 §2。
> 列式索引（`mxc_holder`）在本階段退場。

---

## 1. 維護者的要求

1. **引用計數是精確的數字**：被引用 +1、除引用 −1。
2. **扣到 `MIN < 計數 ≤ 0` 就觸發刪除，立刻生效**，不要寬限期。
3. **既存資料不重建**：給哨兵值 `MIN`，哨兵**不遞增**、也永不觸發刪除。
4. 另給 **`migrate` 指令**（預設不跑）：全部歸零、掃房間與使用者重算、達到條件的直接清 —— 像一個 clean CLI。

第一版做不到 1 和 2：列式索引在 redact 當下答不出現在幾個，只能等 worker 去 seek。維護者的批評成立。

## 2. 純寫入、又精確：RocksDB merge operator

`Txn` 是純寫入的 WriteBatch，寫入端讀不到現值 —— 第一版因此放棄數字。但 RocksDB 有為此而生的原語：
**merge operator**。`merge(key, +1)` 是一筆**寫入**，排進 WriteBatch，讀取或 compaction 時才用我們給的函數合併。

已查證這條路是通的：

| 需要 | 現況 |
|---|---|
| `WriteBatch::merge_cf` | ✅ vendored rust-rocksdb（rev `9c0aad8`）有 |
| `Options::set_merge_operator_associative(name, fn)` | ✅ 有；callback 拿到現值與 operand 迭代器 |
| 有號整數的編碼 | ✅ `ser.rs` / `de.rs` 都有 `i64`（`i32` 的 de 是 stub，所以用 **i64**） |
| engine 掛 merge operator 的位置 | `src/database/engine/cf_opts.rs` 的 `descriptor_cf_options`；目前**沒有任何 CF 用 merge**，要新增 |
| `Txn::merge` | **不存在**，照 `put_raw` 加一個（`self.batch.merge_cf(&map.cf(), key, operand)`） |

### 2.1 新的 column family：`mxc_refcount`

鍵 `mxc`，值 **`i64`**（big-endian，走既有編碼）。合併函數（**只提供 full merge，不提供 partial merge**，
因為結果取決於「原本有沒有列」，不能在不知道基底值的情況下先把 operand 互相合併）：

```
fn merge(current: Option<i64>, operands: [Operand]) -> i64 {
    let mut state = current;                        // None = 從來沒有列
    for op in operands {
        state = match (state, op) {
            (None,      Init)   => Some(0),          // 媒體建立：開一列，從 0 開始
            (Some(c),   Init)   => Some(c),          // 已有列（例如縮圖後來才生）：不動
            (None,      Add(_)) => Some(i64::MIN),   // 沒列就被 ±1 → 既存資料，種哨兵
            (Some(MIN), Add(_)) => Some(i64::MIN),   // 哨兵吞掉一切
            (Some(c),   Add(d)) => Some(c.saturating_add(d)),
            (_,         Set(v)) => Some(v),          // migrate 用
        };
    }
    state.unwrap_or(i64::MIN)
}
```

- **媒體建立時寫 `Init`**：單一接縫是 `media/data.rs` 的 `create_file_metadata`（上傳、縮圖、預覽都經過它）。
  `Init` 對已存在的列是 no-op，所以同一 mxc 後來生縮圖不會把計數歸零。
- **沒有列就被 ±1 → 這就是既存資料**（它建立時還沒有 `Init` 這回事）→ 當場變成哨兵。
  **哨兵是懶惰植入的，不需要任何啟動掃描**（維護者指示：種哨兵要便宜，不然直接 migrate 就好）。
- **`i64::MIN` 是哨兵**：一旦是哨兵，`Add` 全部被吞掉。
- operand 是 9 bytes：1 byte tag（`Init` / `Add` / `Set`）＋ 8 bytes 值（`Init` 的值忽略）。
- **上傳不是引用**：上傳只開列（0）；引用是「事件內容指到它」與「個人頭像指到它」兩種，各 +1。
  上傳後從未送出的檔案停在 0，但 worker 只看被扣過的 mxc，所以不會碰它；它由 migrate 當孤兒清掉（跳過剛上傳的）。

⭐ **比列式索引好在哪**：寫入端仍然不讀，但 worker 讀到的是**一個精確的數字**，O(1)；而且 ±1 塞進事件既有的交易，
**原子性跟 PR #5 完全一樣**。

⚠️ **代價，明寫**：merge 加法**不冪等**。同一事件若被 append 兩次，計數會多 1（列式重插是無事）。現況下
`append_pdu_json` 每個 PDU id 只呼叫一次；redact 後內容已剝空、第二次 redact 讀不到 mxc 所以不會重複 −1。
**這仰賴呼叫端的性質，不是資料庫保證** —— §10 要求一個測試把它釘住。

### 2.2 ±1 的位置：跟 PR #5 一模一樣

PR #5 已把七個維護點接好（事件寫入、backfill、redact、歷史清除、房間清除、設頭像、清頭像）。本階段**只換底層呼叫**：
`add_event_refs` → `txn.merge(mxc_refcount, mxc, Add(+1))`，`del_*` → `Add(−1)`。`media_refs` 服務介面不動，呼叫端零改動。

## 3. 觸發刪除：原文備份被丟掉的那一刻

### 3.0 redact 不歸零 —— 備份才是持有者（維護者 2026-09-02 同步；已實作）

`save_unredacted_events`（預設開）會把被 redact 的原文存進 `eventid_originalpdu`，保留
`redaction_retention_seconds`（上游預設 60 天，維護者預計改成 7 天）。**那份備份就是引用的持有者**：
管理員還看得到訊息的期間，圖也還在。

規則一句話：**原文備份被丟掉的地方，就是 −1 的地方。**

| 路徑 | 誰 −1 |
|---|---|
| redact，備份有存 | **不扣**。`save_original_pdu` 回報「已保留」（現在回傳 `bool`），redact 只剝空事件 |
| retention worker 每小時 reap 過期備份 | **在這裡扣**：`drop_original` 從備份內容讀出 mxc → `Add(−1)`、刪備份、刪索引，同一交易 |
| `purge_history` 直接 `purge_original` 丟備份 | **同樣走 `drop_original`**（否則已 redact 又被 purge 的事件永遠不歸零） |
| redact，`save_unredacted_events = false` | 沒有備份可持有 → **redact 當下就扣** |
| 房間清除、換頭像、清頭像 | 照舊當場扣（這些沒有備份這回事） |

備份就在手上，所以**不需要另存 mxc 清單**；備份讀不出來就不扣（媒體多扣住一份可回收，少扣一份不可逆）。
📌 這也讓上一版標的副作用「原文留著但圖已不在」自然消失。

### 3.1 誰觸發（已實作）

每個 −1 的地方（`del_event_refs`、`set_avatar_ref` 的舊頭像）在交易 `execute()` **之後**把 mxc 丟給收集器
（`media_refs` 服務的 worker，`tokio::sync::mpsc::unbounded_channel`），收到就處理，不另外排程。
對一般的 redact 來說，這發生在 **reap 的那一刻**；對房間清除、換頭像則是當下。

📌 **「之後」怎麼保證**：`Txn::on_execute(closure)` —— 交易自己在寫入落地、通知 watcher 之後跑登記的閉包；空 batch 與被
丟掉的交易不跑。七個呼叫點都不用改，送 channel 這件事和 −1 這筆寫入綁在同一個地方，收集器不可能讀到還沒扣的計數。
它有單元測試（`txn_follow_up_runs_after_the_committed_write`）。

程序若在 `execute()` 之後、收集器處理之前 crash，或收集器還沒起來（啟動、關機中）：計數已是 0 但沒人處理 →
**媒體留著**（安全方向）。§5 的 migrate 會把這種漏網的補掉。

### 3.2 worker 對每個 mxc 做什麼

```
count = read(mxc_refcount, mxc)                      // merge 已合併，精確
None 或 i64::MIN  → skip（沒被算過 / 哨兵）
> 0               → skip（還有人用）
≤ 0               → delete_bytes → write_tombstone → del(mxc_refcount, mxc)
```

- **只碰本地媒體**（`media.is_local()`）；遠端快取照舊走既有 TTL。
- `delete_bytes` 就是既有的 `media.delete()`：`(mxc, Interfix)` 前綴，**縮圖一起刪**。
- 找不到任何檔案鍵（已被手動刪）→ 一樣寫墓碑、清計數。
- **負數照刪，但要出聲**（維護者 2026-09-02 定案：規則就是「MIN < 計數 ≤ 0 就刪」，負數在範圍內）。
  七個 ±1 的呼叫點配平時計數不會低於 0；讀到負數代表某處多扣了一次（或扣了從未加過的），是配平 bug 的訊號，
  所以 worker 刪之前先 `error!` 印出 mxc 與計數，讓人找得到那個呼叫點。`is_mxc_referenced` 現況已是這條規則。
  📎 PR #7 審查（salvia）曾建議負數改成 fail closed 不刪；維護者沒採納 —— 刪除規則要單純，配平錯誤靠日誌抓。

### 3.3 唯一的競態，明寫

只在**備份被 reap 歸零 → 刪 bytes** 之間，若剛好有人 **forward** 同一 mxc（+1），bytes 會在事件寫入後消失 ——
「內容不見」的方向。它不發生在 redact 當下（那時不扣），只發生在 7 天後那一次 reap 的毫秒窗口。
刪 bytes 前**再讀一次**可再壓縮，但 +1 的交易與 worker 的 `delete` 沒有共同的鎖，殘餘窗口存在。

📎 **之後可以改進（維護者已同意後做）**：這台是單一程序，一把「每個 mxc 一鎖」的 in-process 鎖罩住
「讀計數 → 刪 bytes」與 +1，就能關掉這個窗口，不用動資料庫層。本階段先不做。

**第二個窗口，在 `drop_original` 裡（PR #7 審查 salvia 指出、rumia 重審確認；已關閉）**：它先讀備份取 mxc，再在
**另一個**交易刪備份＋ −1。retention worker 與 `purge_history` 若同時處理同一事件，兩邊都在對方刪之前讀到備份，
那個事件的引用會被扣兩次 —— 方向是「多扣」，也就是可能提早刪。順序執行時靠「讀不到就扣零個」擋住；只有並發才穿過去。
**處置**：`retention::Service` 加一把 `tokio::sync::Mutex<()>`，`drop_original` 從讀備份到 `execute()` 全程持有。
一把全域鎖而不是每事件一鎖，因為兩個呼叫端都低頻（reap 每小時一次、purge 是管理指令），而臨界區只有一次點讀＋一筆小交易；
第二個進來的人讀到 not-found、扣零個。這條沒有單元測試（要 Services 才跑得動），靠讀碼確認。

## 4. 既存資料：哨兵懶惰植入，零掃描

**不跑任何啟動掃描。** 規則只有一條：建立時有 `Init` 的媒體才有列；**沒有列的就是既存資料**。
既存媒體第一次被碰到（redact 的 −1、轉發的 +1、換頭像的 ±1），合併函數看到 `None + Add` → 當場變成哨兵。

之後：舊媒體再被 redact → 哨兵吞掉 → **不刪**；再被轉發 → 也吞掉 → 仍是哨兵，**永遠不會被自動刪，直到 migrate**。
新建立的媒體在 `create_file_metadata` 就拿到 `Init`，從 0 開始正常計數。

⭐ 這正是維護者要的：「上傳時種下索引，剩下的一定是哨兵、可以便宜行事」。不枚舉媒體、不掃事件，
成本是零。代價是既存媒體在 migrate 之前永不回收 —— 那是 migrate 存在的理由。

📌 上一版提的「啟動時枚舉所有本地媒體種 `Set(MIN)`」已撤銷：它做的事懶惰植入免費就有。

## 5. `migrate`：重算 ＋ 清理，像一個 clean CLI

```
!admin media migrate-references [--dry-run]
```

**預設不跑，定位是離線維護作業**（維護視窗內執行；執行當下或中途才建立的房間沒掃到，不用管，
下次再掃）。一律**掃全部房間、全部使用者**找出關聯的媒體。跑的時候：

1. **暫停 worker**（記憶體旗標）。
2. **全部歸零**：`mxc_refcount.clear()`。
3. **掃事件**：走 `pduid_pdu` 全表（照 `rebuild_typed_relations` 的 `raw_stream`），每筆用 `list_content_mxc_uris(content)`
   在**記憶體裡**累加。已 redact 的內容為空，自然不加 —— 但它的**原文備份才是持有者**（§3.0），所以再走一遍
   `retention.retained_pdus_raw()` 把備份內容也算進去。漏了這一步，每一則已 redact 但備份還在的訊息的媒體都會被當孤兒刪掉。
4. **掃使用者**：`users.list_local_users()` × `profile.avatar_url()` → 累加。
5. **寫回**（非 dry-run）：`mxc_refcount.clear()` 後，對 `get_all_mxcs()` 的每一個媒體 `merge(Set(計數))`，沒被引用的寫 0
   （不是留空 —— 留空的列下次被 ±1 就變哨兵）；被引用但沒有檔案列的（遠端尚未快取）也寫。每 1000 列一筆交易。
6. **掃媒體**：`get_all_mxcs()` 裡**本地**且計數 ≤ 0 的 ＝ 孤兒 → `media.collect(Migrated)`（刪 bytes、寫墓碑、刪計數列）。
   ⚠️ 跳過「最近 N 秒內建立」的（上傳空窗；建立時間走既有的 `mtime_millis`，讀不到 mtime 也視為太新，不刪）。
7. 恢復 worker；印摘要（掃了幾個事件／備份／頭像／媒體，刪了哪些，跳過哪些）。

`--dry-run` 只做 1–4 ＋ 6 的判斷，印**會刪哪些**，不寫計數、不刪。**第一次一定先 dry-run。**
實作在 `src/service/media_refs/migrate.rs`（`rebuild`），admin 包裝在 `src/admin/media/migrate_references.rs`。

📎 **為什麼掃事件而不是「逐個 room」**：`pduid_pdu` 的鍵以 room 為前綴，全表順掃**就是**逐個 room，不必另外枚舉房間。
使用者那邊才需要 `list_local_users()`。跑完後哨兵全部消失（被 `clear()` 清掉、重算成真實數字），之後就是純自動模式。

## 6. 墓碑（維護者已同意）

新增 `mxc_tombstone`：鍵 `mxc`，值 `(刪除時間, 原因)`，原因 = `GarbageCollected | Migrated | AdminDeleted`。

- **擋點**：內容與縮圖的讀取都收束在 `media.db.search_file_metadata(mxc, dim)`，墓碑就查在它前面。
- **HTTP**：`410 Gone`，errcode 維持 `M_NOT_FOUND`（客戶端只認標準碼）。📎 `Err!(Request(...))` 巨集一律填
  `BAD_REQUEST` 當提示，`response::status_code` 只在提示是 `BAD_REQUEST` 時才依 kind 換算 —— 所以 410 要
  **直接建構** `Error::Request(NotFound, msg, StatusCode::GONE)`。
- **TTL**：CF 設 `ttl = 365 天`（descriptor 既有欄位）。
- `!admin media list-references` 改名 **`refcount`**：印計數，含「哨兵」與「已刪除 + 墓碑」兩種特殊狀態。
  列式索引退場後「誰引用」查不到了 —— 維護者要數字，這是明知的取捨。
- **實作**：值是 `Cbor(Tombstone { deleted_at_secs, reason })`；寫墓碑與刪 `mxc_refcount` 列同一交易（`media.collect()`），
  **交易後強制 `engine.flush()`**。⚠️ 理由（e2e 抓到的）：引擎是 `manual_wal_flush`，每筆寫入後才手動 flush WAL，而別處持有
  cork 時那次 flush 會被略過；程序若在那之後被強制結束，墓碑就留在程序內的緩衝裡不見了 —— bytes 已刪、墓碑沒了，GET 變 404、
  計數列停在 0 沒人再碰。所有寫入都有這個窗口（正常關機會 flush），但「檔案已經不在了」這件事不該是被丟掉的那一半，所以墓碑多付一次 flush。
  擋點在 `Data::search_file_metadata` 開頭；`get`、`get_stored`、`get_thumbnail` 及縮圖的遠端回退都把 410 **原樣傳出**，
  不再落到 lazy preview 或「not found」。`!admin media delete --mxc` 也寫墓碑（`AdminDeleted`）；其他批次刪除指令不寫（它們大多是遠端快取）。

## 7. 設定

| 設定 | 預設 | 意義 |
|---|---|---|
| `media_gc_enabled` | **`true`** | 主開關；`false` 時 worker 只 `info!` 會刪什麼，不刪 |
| `media_gc_migrate_skip_recent_seconds` | `600` | migrate 掃媒體時，跳過多新的上傳 |

沒有寬限期設定 —— 維護者要立刻生效。「刪錯能救」在 Matrix 的答案本來就是重新上傳。

## 8. 從 PR #5 到這一版：要拆掉什麼

| PR #5 的東西 | 處置 |
|---|---|
| `mxc_holder` column family | 標記 `DROPPED`（descriptor 既有做法），資料丟棄 |
| `media_refs` 服務與七個維護點 | **保留介面**，底層換成 merge |
| `Holder` 種類碼、`describe_holder`、鍵編碼測試 | 刪除（沒有列了） |
| `list-references` admin 指令 | 改成 `refcount` |
| `CHANGELOG-fork.md` 那一筆 | 補一段「計數形狀改變」 |

## 9. 已決（2026-09-02，維護者）

1. **哨兵：懶惰植入，零掃描**（§4）。維護者的原話：「上傳時種下索引，剩下的一定是哨兵，可以便宜行事；
   掃事件沒效率，不然直接 migrate 就好。」—— 比前一版的兩個選項都好。
2. **migrate 是離線作業**，一律掃全部房間與使用者；執行中途才建立的房間不用管。「跳過最近 10 分鐘上傳」保留，
   它保護的是中途才上傳、還沒送出的檔案（房間漏掃無害，孤兒誤判會刪檔）。
3. **「非冪等」不是決定項，撤銷。** 它只是一個要用測試釘住的性質（§10）：同一事件不會被 append 兩次、
   redact 後內容為空不會重複 −1。現況兩者都成立。
4. `delete_by_event` 讀源**分開修**。

## 10. 測試計畫

| 層 | 內容 |
|---|---|
| 單元 | 合併函數：`None+Init=0`、`Some(c)+Init=c`（縮圖後生不歸零）、**`None+Add=MIN`（懶惰哨兵）**、`0+1=1`、`1−1=0`、**哨兵吞 ±1**、`Set` 覆蓋、飽和；i64 與 operand 的編碼來回 |
| DB 級（真 RocksDB，`engine/tests.rs`） | 透過 descriptor 的掛點註冊 operator，真的 `merge_cf` 後 `get_cf` 折出數字；flush＋compact 之後讀到的一樣、再 merge 仍疊在存下來的值上。這是單元＋型別＋build 都抓不到、兩次咬到我們的那條接縫（PR #5 的 u8 round-trip、PR #7 的 WriteBatch walker）|
| 🔲 尚缺（PR #7 審查 rumia 🟢3） | 「備份存在時 purge 只釋放一次」的雙路徑互斥：`purge.rs` 從 raw 內容扣 vs `drop_original` 從備份扣，靠「redact 後內容為空」互斥。純函數那半（`a_redacted_event_names_nothing`）已釘住；整條路徑要 Services 才跑得動，目前只有 e2e 情境 1 涵蓋 |
| 端到端（真伺服器，抄 PR #5 的腳本） | 送圖 → `refcount`=1 → 轉發到第二房 → 2 → redact 一則 → 1（**bytes 還在**）→ redact 另一則 → **bytes 立刻消失**、GET 回 410、`refcount` 印墓碑 → 再送一則引用同一 mxc 的訊息 → 仍 410（墓碑是終局）|
| 端到端（哨兵） | 用 PR #5 之前的舊資料庫啟動 → 舊媒體 `refcount` 印「哨兵」→ redact 舊事件 → **不刪** |
| 端到端（migrate） | 同一舊資料庫 `--dry-run` 印孤兒清單 → 真跑 → 孤兒消失、被引用的留著、哨兵全變成真實數字 |
| 非冪等的守門 | 同一 PDU 兩次 `append_pdu_json` 會怎樣 —— 至少一個測試把「現況只呼叫一次」釘住，壞了會被看到 |
| 變異 | 只打合併函數 |
