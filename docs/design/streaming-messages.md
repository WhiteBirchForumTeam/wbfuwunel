# 流式訊息（Streaming Messages）設計草案，第三版

> **狀態：📄 草案，等維護者同意。** 第三版依維護者 2026-09-08 的方向重寫：草稿是**暫態**，只在記憶體與 TCP 上，可以丟；
> 一則草稿從頭到尾改同一個暫時 id；可以送**全文**也可以送 **delta**；最後不改了就送一則**正常訊息**持久化，取代草稿。
> 走 [wbf-event-push.md](wbf-event-push.md) 的訂閱與推送（工作 2），這份是工作 3。
> 第二版（2026-09-03，`Open`／`Fragment`／`Close`／`Abandon`、server 發 stream id）作廢：它要 server 發 id、記 stream 狀態、管 `next_seq`，
> 第三版全部拿掉——server 對草稿**零狀態**。

## 1. 這是什麼、不是什麼

「流式訊息」是一則**還在長**的訊息：LLM 逐字吐、長訊息分段打、或改自己剛送的東西。在線的人看著它變，講完了它變成一則正常訊息。

它跟分塊上傳共用通道與外框，語意相反：上傳的塊要落盤、要 seek；草稿不落盤、掉了就掉。太長的東西不是流，是檔案。
它也**不是** Matrix 的 `m.replace`（edit）：edit 每改一次一個永久事件；草稿一個都不進歷史。edit 不受影響、照舊。

## 2. 三個規則

1. **草稿是暫態**：server 不存、不重送、後加入者永遠看不到；只有當下訂閱著（[wbf-event-push.md](wbf-event-push.md)）的人收到。
2. **一則草稿一個暫時 id，從頭到尾不變**：`draft_id` 由發送者選（pack header 的 `id`），作用域是 `(sender, device)`；接收者用 `(sender, device, draft_id)` 認它。
3. **定案是一則正常訊息**：`Event/Send` 的 meta 多帶 `draft_id`，server 照常 append、照常推（`Push`），接收者把那個草稿的顯示換成這則事件。草稿到此結束。

## 3. pack

kind `0x02 Stream`（§3.3 早留的），兩個 subtype：

| subtype | 誰發 | header | meta（**明文**，server 要讀） | data |
|---|---|---|---|---|
| `0x01 Draft` | 發送者 → server → 訂閱者 | `id` = `draft_id`；`seq` 每片 +1（發送者自己遞增） | `{ "room_id", "full": bool }`；server 轉發時**加上** `"sender"`、`"device"` | **密文**（E2EE 房）或明文：`full: true` 是全文；`full: false` 是相對於**上一片**的 delta |
| `0x02 Abandon` | 發送者 → server → 訂閱者 | `id` = `draft_id` | `{ "room_id" }`；轉發時加 `sender`、`device` | 無 |

- **只走 WS**；HTTP 回 `Unsupported`（草稿沒有訂閱者就沒有意義）。
- **`Draft` 不回 Ack、不回 Error**（除了准入與大小關卡的錯）：它是「盡快」語意，跟 `Push` 一樣。發送者送完就送下一片。
- **定案**：`Event/Send` meta `{ ..., "draft_id": <u64> }`。server 把它寫進事件的 `unsigned["org.wbftw.wbfuwunel.draft_id"]`
  （`unsigned` 不進雜湊、不進聯邦、跟 `r_seq`／`g_seq` 同一個位置），所以連 `Push` 推來的事件都帶得到，接收者不需要另一個「Sealed」包。
- **`Abandon`** = 撤回草稿，不定案。發送者的連線斷了、沒 `Abandon` 也沒定案：接收者自己在 `wbf_stream_draft_ttl`（client 端建議 30 秒）沒新片就把草稿收掉。server 不管。

## 4. 全文與 delta：server 不懂、接收者自保

E2EE 房間裡 server 讀不到本文，所以 **delta 是 client 算的、client 套的**，server 只轉發。server 唯一讀的是明文 `full`：

- **第一片建議 `full: true`**（維護者：也可以是只有 + 的 delta，接收者從空字串套起）。
- **delta 是相對於上一片**（`seq − 1`）的。接收者只在「上一片有收到」時套 delta；漏了一片就把這則草稿標成「等全文」，收到下一個 `full: true` 再顯示。
- **發送者要定期送全文**（client 政策，建議每 2 秒或每 20 片一次），讓漏片的人與中途訂閱的人接得回來。server 不強制、不檢查。
- delta 的內部格式是 client 的事（在密文裡）。建議最簡單的 `{ "ops": [["=", n], ["+", "text"], ["-", n]] }`，server 永遠不看。
- 為什麼 delta 值得做：LLM 逐 token 吐、每 50 ms 一片，全文重送是 O(長度²) 的頻寬；delta 是 O(長度)。維護者要 delta，這裡就給 delta，但**全文永遠是後路**。

## 5. server 端：轉發，零狀態

```
Draft 進來（已登入、准入表過、meta 是 {room_id, full}）
   ├─ sender 是 room_id 的成員嗎？不是 → Error(Forbidden)
   ├─ 限速：每 (sender, device) 每秒 wbf_stream_drafts_per_second（預設 30，突發 60）→ 超過 Error(RateLimited)，這片丟
   ├─ 大小：data ≤ wbf_stream_max_draft_bytes（預設 64 KiB）→ 超過 Error(TooLarge)
   └─ push::relay(room_id, sender, device, pack')   ← pack' = 原 pack 加 sender/device 進 meta；對每個訂閱者 try_send，滿了就丟（不記 gap：草稿沒有洞的概念）
```

- **不記任何 stream 狀態**：沒有 `next_seq`、沒有 open 表、沒有 TTL 清理。第二版那張 `room → Vec<connection>` 表就是推送的 registry，不另建。
- **不驗 `seq`**：TCP 保證同一連線內的順序；接收者只接受比目前大的 `seq`，其餘丟。發送者換連線續發同一個 `draft_id`：允許，`seq` 自己接上就好。
- **發送者自己的其他裝置也收到**（跟 `Push` 一樣）；發送這片的那條連線也會收到自己的轉發——client 用 `(sender, device)` 是自己就忽略。不做特例。
- **不推給沒訂閱的**：沒訂閱的連線收不到草稿，也收不到 `Push`，一致。

## 6. 跟 `Push` 共用什麼

| 共用 | 在哪 |
|---|---|
| 訂閱 registry（`user → 連線`）、`try_send`、掉了就掉 | `Services.push`（[wbf-event-push.md](wbf-event-push.md) §3） |
| 發送佇列與發送 task | pipeline §1 |
| 「後加入者零殘影、歷史單一版本、容量有界、不污染 sync token、server 不碰明文」五條 | 第二版 §9，仍全部成立；容量有界現在是「零」 |

## 7. 上限（config）

| 名字 | 預設 | 管什麼 |
|---|---|---|
| `wbf_stream_drafts_per_second`／`wbf_stream_drafts_burst` | 30／60 | 每 (sender, device) 的 `Draft` 頻率（`IpTokenBuckets` 同款，key 換成 device） |
| `wbf_stream_max_draft_bytes` | 65536 | 一片的 data 上限（同時受 `wbf_data_max_bytes`） |

沒有「每房幾條 stream」的上限：server 不知道有幾條（零狀態）；頻率與大小限住了流量，這就夠。

## 8. 驗收（e2e11，接在推送的情境後面）

- alice 訂閱；bob 送 `Draft(seq 0, full)`、`Draft(1, delta)`、`Draft(2, delta)` → alice 依序收到三個 `Draft`，meta 多了 `sender`／`device`，data 原樣（server 不動 bytes）。
- bob 定案：`Event/Send` 帶 `draft_id` → alice 收到 `Push`，事件 `unsigned` 帶 `org.wbftw.wbfuwunel.draft_id` = 那個 id；bob 自己收到 `Ack` 與 `Push`。
- bob `Abandon` → alice 收到；沒有事件。
- 沒訂閱的連線收不到 `Draft`；非成員送 `Draft` → `Forbidden`；HTTP 送 → `Unsupported`；超過 64 KiB → `TooLarge`；連送 100 片 → 一部分 `RateLimited`。
- alice 停止讀 socket、bob 送 50 片 → bob 一片都沒被擋（沒有背壓回到發送者）、alice 恢復後收到的是後面的片、沒有 `gap`。
- 關機中連線收到 Close 1001。

## 9. 開放問題

1. **delta 的內部格式要不要定在 spec 裡**：server 不看，但兩個 client 實作要一致。建議定在 client 的 `wbf-client-convention`，server 這邊只定 `full` 旗標。
2. **`draft_id` 進 `unsigned`**：好處是一個包解決定案對應；代價是持久化一個只對「當時在線的人」有意義的數字（8 byte）。維護者若不要，改成 server 推一個 `Stream/Sealed { draft_id, event_id }`，多一個 subtype、多一次推。
3. **草稿要不要限成員數**：幾千人的房間一片草稿就是幾千次 `try_send`。`Push` 有同樣的問題（event-push §9），一起看。
4. **同一 (sender, device) 同時多則草稿**：允許（不同 `draft_id`），server 不限；client 自己決定要不要。

## 10. 這份文件的查證範圍

- `src/api/client/wbf/{mod,ws,send}.rs`（PR #33 的樣子）：發送佇列、准入表、`Event/Send` 的 meta 解析與 `send_message_event`。
- `src/service/rooms/typing/mod.rs`：第二版打算仿的記憶體狀態，第三版不需要了。
- `src/service/rooms/timeline/append.rs`、`src/core/matrix/pdu/seq.rs`：`unsigned` 裡放 server 欄位的既有做法（`r_seq`／`g_seq`）。
- 沒實測：delta 對 LLM token 頻率的實際節省、大房間轉發的成本。
