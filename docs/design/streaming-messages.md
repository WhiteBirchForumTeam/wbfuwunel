# 流式訊息（Streaming Messages）設計草案

> **狀態：草案。** 這份文件是「流式訊息」的設計草案，對應
> [why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md) 的 **§5.3 文字流**。
> 目的是把要做的東西、為什麼、有哪些取捨，寫成下一個讀的人只有 repo 也能接手的文件。
> 每個決定都還可以推翻；推翻時請連帶更新「為什麼」那一段。
>
> 撰寫日期：2026-09-01。**尚未實作** —— 依 [fork-overview.md](fork-overview.md) 的流程，
> 這份文件在維護者同意之前不動 `src/`。

**同一個目錄裡的其他文件**：[fork-overview.md](fork-overview.md)、
[why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md)、
[repo-structure.md](repo-structure.md)、[windows-build.md](windows-build.md)、
[media-refcount.md](media-refcount.md)。

---

## 1. 這是什麼、不是在講什麼

這裡講的「流式訊息」是**文字／token 的串流**：一段訊息的分片內容還在長，就先把已經
有的部分送給對方看，講完了才收成一個正式訊息。對標情境：**LLM 回覆逐字吐出**、長訊息
分段輸入、任何「內容產生需要時間」的對話。

⚠️ **不是** §5.2 的**媒體**串流（分塊 + Merkle + range）。那是檔案層，兩者不衝突、也不該
被綁在一起做。這份文件只談文字流層級的事件語意。

### 1.1 為什麼不值錢的那條路（現狀）不好

Matrix 對「內容會變的訊息」只有一招：`m.replace` 關聯 —— 每更新一次發一個**完整的新事件**
說「這個取代那個」。把 LLM 的 token 串流餵進去特別難看：

- 每吐幾個字，就產生一個完整的、不可變的、要進歷史的新事件。
- 歷史被反覆改寫；接收端要追一串 `relates_to` 才能收斂到最新版。
- 中間每個殘影都永久留在 DAG 裡（不可變 + 節點不可消失）。

我們已經決定不為此相容 Matrix 規格（核心文件 §1 非目標：不與 Matrix 規格相容）。
所以要的是**乾淨的流式語意**，不是 `m.replace` 的模擬。

---

## 2. 核心決定：分兩層，而不是一個新的事件型別

整份設計的支點是這句話：

> **串流中的分片不落盤、不進歷史；講完了才寫一個正式事件定案。**

分兩層：

| 層 | 性質 | 存哪 | 誰看到 |
|---|---|---|---|
| **即時分片（stream chunks）** | ephemeral，帶 `stream_id` + 序號 | 記憶體（同 typing） | 只有當下在線的 client |
| **定案事件（finalized event）** | 正式 timeline 事件 | RocksDB（既有事件路徑） | 所有人，含之後加入的 |

這跟核心文件 §5.3 完全一致，也跟現有 **typing** 的處理同構：
在線者秒出、離線者/後加入者拿到的是**乾淨的正式事件**，歷史零污染。

> ⭐ 中間加入的 client **永遠不會**收到分片殘影。這對同步程序而言是**特性不是缺陷**：
> 它不需要補齊任何碎片，等定案事件就好。歷史只有一個版本。

---

## 3. 程式碼落點（對照 repo 結構，2026-09-01）

研究後，這個功能在現有程式碼裡的接縫是清楚的。typing service 是**現成的同構範例**。

### 3.1 主體：`src/service/rooms/streaming/`（新增，仿 `typing/`）

`src/service/rooms/typing/mod.rs` 已經把這個形狀做過一遍，流式完全照搬再擴充：

| typing 現有 | 流式需要的擴充 |
|---|---|
| `RwLock<BTreeMap<RoomId, RoomTyping>>`（純記憶體） | `RwLock<BTreeMap<RoomId, BTreeMap<StreamId, Stream>>>`，Stream 帶 `seq`/`state`/`timeout` |
| `update: u64` = `globals.next_count()`（全域序號） | 同 —— chunk 也算一次 update |
| `typing_update_sender: broadcast`（喚醒 sync） | 同，多一個 stream sender，名稱區分 |
| `snapshot_for_user(room, user, select)`（帶 predicate） | 同 —— `select(token)` 決定要不要重拉 |
| `typings_maintain`（超時清理） | `streams_maintain`（超時 + 上限 + abandoned 清理） |

關鍵參考：`typing::Service::typing_snapshot_for_user`（mod.rs L302-333）—— predicate 在
同一個 lock 下評估 token、比對了才 clone user 清單，避免每次都整包複製。

### 3.2 sync 喚醒：`src/service/sync/watch.rs`

在 `watch` 函式裡，per-room 已經註冊了一堆 watcher（`pduid_pdu` / receipts / **typing
broadcast**）。流式就**再多註冊一個 broadcast subscription**，跟 typing 那段
（L138-154）完全同構：

- 註冊 `streaming_update_sender.subscribe()`，收到的是該 room 的 stream 有變。
- 這讓 sync 的長輪詢在「有 stream chunk 進來」時被喚醒去重拉該 room。

### 3.3 sync 注入：`src/api/client/sync/v3.rs`

`ephemeral.events` 目前由 read receipts + **`gather_typing_events`**（L1401）組成。
流式就新增 `gather_stream_events`，跟 `gather_typing_events` 同樣的 `since` 過濾邏輯，
把該 room 目前 in-flight 的 stream chunks 包成 ephemeral 事件 append 進去
（`JoinAggregates.typing_events` → 加一個平行欄位）。

### 3.4 定案：`src/service/rooms/timeline/append.rs`

定案時，組一個正式事件走既有 append 路徑（`append_incoming` / append），內容 = 組好的完整
字串 + 指向 `stream_id` 的關聯欄位。落地後被 `pduid_pdu` watcher 撈到，自然進 timeline、
推進 `next_batch`。

### 3.5 不需要動的

- **不新增 column family**（`src/database/maps.rs`）—— 分片永不落盤。定案事件走既有
  事件型別與欄位。
- **media/storage**（§5.2 的活）不碰。
- 非聯邦（Phase 1），**沒有 federation EDU 這條**要處理 —— 好消息，減掉一大塊。

---

## 4. 資料模型草案

```text
Stream {
    stream_id: 全域唯一（server 發，或 sender 隨機＋server 背書）
    room_id
    sender:    UserId
    device:    DeviceId      // 同使用者多裝置分開，避免兩裝置各發一半
    seq:       u64           // 起 0，每 chunk +1
    state:     open | closed | abandoned
    created_at / last_chunk_at
    length:    u64           // 累積位元組（僅供計量/上限，非內容）
}

Chunk { stream_id, seq, data }   // delta 或全量補；見 §7 待驗 4
```

事件型別草案（見 §7 待驗 3 定案）：

- **ephemeral**：`m.stream_chunk`（room-scoped ephemeral，帶 stream_id + seq + data）
- **timeline**：`m.room.message` 帶 `stream_id` 關聯（定案事件），或自訂型別

---

## 5. 傳輸選擇：走 sync 還是走專屬即時通道

這是本設計**最大的取捨**。分片要送到在線 client，有兩條路。

### 選項 A：分片走 sync ephemeral（全複用，可先做）

分片當作 ephemeral room 事件，跟著既有 sync 長輪詢送。優點是**零新傳輸**——事件型別、
認證、重連語意全部白拿。

**代價**：sync 的模型是「有變化就 wake，wake 後把該 room 的這批狀態重拉一次」。分片若以
**LLM token 頻率**（每 10–50 ms 一片）進來，會在短時間內觸發大量 wake + 重拉，sync 負荷
和用戶端解碼都難看。

**適用**：**粗粒度串流** —— 一段訊息分 3–5 片、或分片按 ~100–250ms 批合成包。

### 選項 B：分片走專屬即時通道（SSE / push）

新增一條 SSE（或等價的長連線 push）給 in-room 即時變更，分片只在那上面推；sync 只管定案
事件。token 級頻率可行，client 不必每次全量重拉。

**代價**：新傳輸、新認證/重連語意、要處理「連線斷了沒收到中間分片」的補償。

### 建議

> **先用 A 定模型，token 級壓測後再決定要不要 B。** 兩者的**事件型別共用**，換傳輸不動
> `stream_id` + 序號 + 定案事件這套模型。這符合核心文件 §6「先繞過、不是先取代」——
> A 的價值是先把正確的語意焊死，而不是押注在傳輸層。

---

## 6. 必須守住的語意（做成驗收條件）

1. **後加入者零殘影**：新 sync 只看到定案事件，永遠收不到分片。
2. **歷史單一版本**：定案前事件層沒有半成品；定案後只有一個正式事件。
3. **容量有界**：分片全記憶體、有上限、有超時（呼應核心 §1/§4）。
4. **不污染 `next_batch`**：分片只算 ephemeral update，**不得**推進 timeline 的
   `next_batch`；只有定案事件推進。需驗證現有 `next_batch` 是否只跟 timeline PDU 走
   （typing 正是如此——ephemeral 不推 timeline token；若是則沿用，不需改）。
5. **E2EE 下 server 不碰明文**：server 驗證的是密文分片的**順序與存在**，不是內容語意。

---

## 7. 開放問題與待驗（對齊核心文件 §7 的風格）

1. **分片頻率 / 批大小**。100ms? 250ms? —— 在「在線顯延遲」與「sync 負荷」之間取捨。
   這是選 A 或 B 的**實測依據**。
2. **同 room 多使用者並行串流**。多個 stream 交錯時，per-stream 序號各自唯一，跨 stream
   不保證全序，client 依 stream_id 分流。需確認這語意可接受。
3. **事件型別命名**。用現成 `m.room.message` + 關聯欄位，還是自訂型別？牽涉既有 client
   對陌生內容的退化行為（核心文件 §6 Phase 0 的 `body` 純文字連結退化策略可參考）。
4. **分片內容是全量還是 delta**。全量簡單、免組裝；delta 省頻寬但要處理亂序/遺失。初版
   建議全量（client 直接顯示最近一片），壓測再決定。
5. **E2EE 金鑰安排**（= 核心文件 §7 待驗 4）。一個 stream 一把短期 session 金鑰，還是沿用
   逐 event 金鑰？建議前者，但需先解決一次 `m.room.key_request` 交換成本。
6. **resource 上限實值**：每 room 並行 stream 數、每 stream 分片數/bytes、總記憶體臨界。
7. **跨裝置串流狀態同步**。目前設計**不同步**（device 分 stream），簡化；要不要跨裝置續看
   是 client 端決定。
8. **串流中可否部分撤回/編輯**。初版**不做**，定案即定案；要編輯走既有編輯語意。

---

## 8. 這份文件的查證範圍（誠實標註，呼應核心文件 §8）

**實際在程式碼裡讀過（2026-09-01，`main` == 上游 v1.9.0-91）**

- `src/service/rooms/typing/mod.rs`：ephemeral 在記憶體 + `next_count` + broadcast +
  `snapshot_for_user` predicate 的完整範例（L302-333）。
- `src/service/sync/watch.rs`：per-room watcher 註冊 + typing broadcast subscription
  （L138-154）——流式的落地處。
- `src/api/client/sync/v3.rs`：`ephemeral.events` 組成（receipts + `gather_typing_events`
  L1401）與 `JoinAggregates` 結構（L1363-1373）。
- `src/service/rooms/timeline/append.rs`：定案事件的 append 路徑入口。
- `src/database/maps.rs` 目錄規模與 `src/service/storage/` 的 provider 抽象（repo-structure.md）。

**純粹是判斷，不是事實**

- 「sync 在 LLM token 頻率下會是負擔」—— 理由是 sync 的 wake-and-repull 模型，這需要
  實測（待驗 1）佐證。
- 「選 A 用 `stream_id` + 序號 + 定案事件就夠把語意焊死」—— 這是設計假設，等第一份
  跨端實作驗證。