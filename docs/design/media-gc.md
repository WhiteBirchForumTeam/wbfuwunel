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

## 3. 觸發刪除：立刻，不等

### 3.1 誰觸發

每個 −1 的地方，在交易 `execute()` 之後**立刻**把 mxc 丟給 worker（`tokio::sync::mpsc`），worker 收到就處理，
不是每小時掃。redact 到 bytes 消失是**毫秒級**。

程序若在 `execute()` 之後、送進 channel 之前 crash：計數已是 0 但沒人處理 → **媒體留著**（安全方向）。
§5 的 migrate 會把這種漏網的補掉。

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

### 3.3 唯一的競態，明寫

worker `read` 到 0 → `delete` 之間，若有人**新引用**同一 mxc（轉發一則舊訊息），bytes 會在事件寫入後消失 ——
「內容不見」的方向。窗口是毫秒級，沒有寬限期把它拉開。刪 bytes 前**再讀一次**可再壓縮，但 +1 的交易與 worker 的
delete 沒有共同的鎖，殘餘窗口存在。維護者選了「立刻生效」，這是那個選擇的已知代價；完全消除要資料庫層帶讀的交易，本階段不做。

📎 **副作用**：`save_unredacted_events` 會把被 redact 的原文留 60 天給管理員查證，媒體立刻刪掉後那份原文指向一張
已不存在的圖。這是「隱私與空間優先於事後查證」，維護者已知。

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
   對每個 mxc `merge(Add(+1))`。已 redact 的內容為空，自然不加。
4. **掃使用者**：`users.list_local_users()` × `profile.avatar_url()` → `merge(Add(+1))`。
5. **掃媒體**：`get_all_mxcs()` 逐個讀計數 —— **不存在或 0 ＝ 孤兒** → 刪 bytes、寫墓碑。
   ⚠️ 跳過「最近 N 分鐘內建立」的（上傳空窗；建立時間走既有的 `mtime_millis`）。
6. 恢復 worker；印摘要（掃了幾個事件／使用者／媒體，刪了幾個，跳過幾個）。

`--dry-run` 只做 1–4 ＋ 印**會刪哪些**，不刪。**第一次一定先 dry-run。**

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
| 端到端（真伺服器，抄 PR #5 的腳本） | 送圖 → `refcount`=1 → 轉發到第二房 → 2 → redact 一則 → 1（**bytes 還在**）→ redact 另一則 → **bytes 立刻消失**、GET 回 410、`refcount` 印墓碑 → 再送一則引用同一 mxc 的訊息 → 仍 410（墓碑是終局）|
| 端到端（哨兵） | 用 PR #5 之前的舊資料庫啟動 → 舊媒體 `refcount` 印「哨兵」→ redact 舊事件 → **不刪** |
| 端到端（migrate） | 同一舊資料庫 `--dry-run` 印孤兒清單 → 真跑 → 孤兒消失、被引用的留著、哨兵全變成真實數字 |
| 非冪等的守門 | 同一 PDU 兩次 `append_pdu_json` 會怎樣 —— 至少一個測試把「現況只呼叫一次」釘住，壞了會被看到 |
| 變異 | 只打合併函數 |
