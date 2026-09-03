# 流式訊息（Streaming Messages）設計草案，第二版

> **狀態：草案，等維護者同意。** 對應核心設計 [why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md) §5.3。
> 第二版依維護者 2026-09-03 的指示改：**走 WebSocket over TLS 的二進位通道**，每一片是一個 pack
> （[wbf-wire-format.md](wbf-wire-format.md)：標頭＋密文 meta＋密文 data），不定長、不需要 seek；
> 預設**沒收到就丟**，需要時用標頭的 `WANT_ACK` 與 `seq` 做 Ack 與重送。第一版「分片走 sync ephemeral」降為**退化路徑**（§6）。
>
> 撰寫日期：2026-09-01；第二版 2026-09-03。**尚未實作**，同意前不動 `src/`。

## 1. 這是什麼、不是什麼

「流式訊息」是**文字／token 的串流**：一段訊息還在長，就把已經有的部分送給在線的人看，講完了才收成一個正式訊息。
對標情境：LLM 回覆逐字吐出、長訊息分段輸入。

它跟分塊上傳（[chunked-upload.md](chunked-upload.md)）**共用通道與外框**，但語意相反：
上傳的塊是定長、要落盤、要 seek；流的分片是不定長、不落盤、能用多少就多少。太長的東西就不是流，是檔案。

### 1.1 為什麼不用 Matrix 現成的那條

`m.replace` 每更新一次發一個完整的新事件進歷史；LLM 每吐幾個字就一個永久事件，歷史被殘影填滿。
我們不與 Matrix 規格相容（核心設計 §1），所以要的是乾淨的流式語意。

## 2. 核心決定：分兩層

> **分片不落盤、不進歷史；講完了才寫一個正式事件定案。**

| 層 | 性質 | 存哪 | 誰看到 |
|---|---|---|---|
| **分片（fragment）** | 短暫，帶 `stream_id` ＋ `seq` | 記憶體（同 typing） | 只有當下連著通道的 client |
| **定案事件** | 正式 timeline 事件 | RocksDB（既有事件路徑） | 所有人，含之後加入的 |

中途加入的 client 永遠收不到分片，等定案事件就好；歷史只有一個版本。

## 3. 傳輸：WebSocket 二進位通道

分片走 [wbf-wire-format.md](wbf-wire-format.md) 的通道，kind = `Stream`。**不走 sync**：sync 的模型是「有變化就 wake、wake 後重拉」，
token 頻率（每 10–50 ms 一片）會把它打爆；一條長連線推分片，開銷是 32 bytes 的外框。

**加密**：一片就是一個 pack。server 只讀明文標頭（`kind`、`subtype`、`id` = stream id、`seq`、`flags`）；
**meta 與 data 都是密文**（`META_ENCRYPTED = 1`），server 原樣轉給同房間、連著通道的其他 client。
它驗的是順序與存在，不是內容。一個欄位一個職權：meta 放這一片的語意（JSON），data 放本文本體。

## 4. 訊框（kind = `Stream`）

標頭：`id` = stream id（`Open` 時 0，server 在 `Ack` 的 meta 發），`seq` = 片序號（`Open` 是 0，`Fragment` 從 1 起遞增）。
除了 `Open` 的請求 meta（server 要知道房間），其餘 meta 與 data 都是密文。

| subtype | 誰發 | meta | data |
|---|---|---|---|
| `Open` | 發送者 | **明文** `{ "room": "!…", "device": "…", "ack": bool }`（server 要查房間成員） | 無。server 回 `Ack`，meta `{ "id": <stream id> }` |
| `Fragment` | 發送者 → server → 接收者 | 密文 JSON `{ "done": false }` —— 這一片的語意，有需要再加欄位 | 密文：到目前為止的**全文**（UTF-8） |
| `Close` | 發送者 | 密文 `{ "event_id": "$…" }`（定案事件，§5） | 無。接收者收到就把該 stream 的顯示換成正式事件 |
| `Abandon` | 發送者或 server | server 發的是明文 `{ "reason": "timeout" }`；發送者發的可密文 | 無 |

`Fragment` 的 meta 與 data 各自 AEAD（`key_stream`，`nonce = base ‖ seq ‖ 段號`），接收者解開就是 JSON 與文字。

- **`text` 是全量不是 delta**：接收者永遠只顯示最後一片，掉一片不會亂。代價是頻寬隨長度線性長，
  對一則訊息的長度來說可忽略；真的長到在乎，那是檔案（§1）。
- 不定長，能塞多少塞多少；外框有 `wbf_meta_max_bytes`、`wbf_data_max_bytes` 擋極端值。

## 5. 送達語意：預設丟，選用 Ack

- **預設（`ack: false`，也就是標頭不帶 `WANT_ACK`）**：server 收到 `Fragment` 就轉發，不存、不重送。接收者掉了一片，下一片是全量、自然補上。
  這跟 typing 一樣是「最新狀態」語意，不是「每一片都要到」。
- **選用（`Open` 時 `ack: true`，之後每片帶 `WANT_ACK`）**：server 對每片回 `Ack`（標頭抄回同一組 `id`、`seq`，不用讀 meta）；發送者沒在 `T_ack`（client 端自訂，建議 1 s）內收到就重送。
  server 端對接收者也一樣：接收者可以回 `Ack`，server 據此重送給沒收到的接收者（保留最後 N 片在記憶體，`N = wbf_stream_replay_depth`，預設 8）。
  這是給「一片都不能掉」的場合（例如流的內容不是純顯示、接收端要逐片處理）。
- **定案永遠可靠**：`Close` 帶 `event_id`，定案事件走既有 append 路徑進 RocksDB、進 sync。分片掉光了也沒關係，
  正式事件一定到。這就是為什麼預設可以丟：可靠性放在定案，不放在分片。

## 6. 退化路徑：沒有通道的 client

沒連 WebSocket 的 client（舊 client、或還沒實作通道的）**只會**在 sync 裡看到定案事件，分片一片都看不到。
這是設計上接受的：它們少的只是「看著它長出來」的體驗，內容一個字不少。

第一版提的「分片走 sync ephemeral」**不做**：它的價值是零新傳輸，但通道為了分塊上傳反正要做，沒理由再養一條。

## 7. 程式碼落點

| 東西 | 落點 | 參考 |
|---|---|---|
| 通道與外框 | `src/api/client/wbf/ws.rs`，axum `ws` feature（目前沒開） | [wbf-wire-format.md](wbf-wire-format.md) §5 |
| stream 狀態（記憶體） | `src/service/rooms/streaming/`（新增，仿 `typing/`） | `typing/mod.rs` 的 `RwLock<BTreeMap<RoomId, …>>`、超時清理 |
| 轉發 | streaming service 對每個房間維護「連著通道的 client」清單；`Fragment` 進來就對清單裡除發送者外的每個 sender 推 | 需要一張 `room → Vec<connection>` 表，連線斷就移除 |
| 定案 | `src/service/rooms/timeline/append.rs` 既有路徑，內容 = 最後一片的全文，欄位帶 `stream_id` | 落地後被 `pduid_pdu` watcher 撈到，自然進 sync |
| 誰有資格收 | 房間成員判斷走既有的 `state_cache.is_joined` | 加入通道時綁定 user，`Open` 時查房間成員 |

**不新增 column family**：分片永不落盤。

## 8. 資料模型

```text
Stream {
    id:         u64（server 發，連線內唯一即可；跨連線不需要）
    room_id, sender: UserId, device: DeviceId
    seq:        u64（起 0，每片 +1；接收者只接受遞增）
    ack:        bool
    state:      Open | Closed | Abandoned
    created_at / last_fragment_at
    recent:     VecDeque<Fragment>（只在 ack=true 時保留，長度 ≤ wbf_stream_replay_depth）
}
```

上限（config）：`wbf_stream_timeout`（預設 30 s 沒新片就 Abandon）、`wbf_stream_max_per_room`（預設 16）、
`wbf_stream_max_fragment_bytes`（預設 64 KiB，同時受 pack 的 `wbf_data_max_bytes` 管）。全部記憶體、全部有上限、全部有超時 —— 呼應核心設計 §1 的「容量有界」。

## 9. 必須守住的語意（驗收條件）

1. **後加入者零殘影**：只看到定案事件。
2. **歷史單一版本**：定案前事件層沒有半成品。
3. **容量有界**：分片全記憶體、有上限、有超時。
4. **不污染 `next_batch`**：分片不走 sync，自然不推進 timeline token；只有定案事件推進。
5. **E2EE 下 server 不碰明文**：server 只看 pack 的明文標頭，meta 與 data 原樣轉發。
6. **通道斷了不丟定案**：發送者重連後仍能 `Close`（stream 還在 `wbf_stream_timeout` 內）或重發定案事件。

## 10. 開放問題

1. **stream 的金鑰**（核心設計 §7 待驗 4）：一個 stream 一把短期金鑰（`Open` 時透過既有的 to-device 金鑰分發送給房間成員），
   還是沿用該房間當下的 Megolm session。建議**沿用 Megolm session**：分片本來就是「這則訊息的中間狀態」，
   跟定案事件同一把，接收者不用多一次交換。待驗的是 Megolm 對高頻小訊息的 ratchet 成本。
2. **同房間多 stream 交錯**：per-stream `seq` 各自遞增，跨 stream 不保證全序，client 依 `id` 分流。需確認接受。
3. **跨裝置**：同一使用者兩台裝置各自是獨立的接收者；發送者只能是一台。不做跨裝置續發。
4. **串流中撤回**：`Abandon` 就是撤回，不定案；已定案走既有 redact。不做部分編輯。
5. **分片頻率**：client 端決定（建議每 50–100 ms 或每 N 個 token 合併一片），server 不限制頻率、只限制大小與並行數。

## 11. 這份文件的查證範圍

**在程式碼裡讀過（2026-09-01，`main` == 上游 v1.9.0-91；2026-09-03 再確認）**

- `src/service/rooms/typing/mod.rs`：記憶體狀態 ＋ 超時清理的形狀，流式照搬。
- `src/service/sync/watch.rs`、`src/api/client/sync/v3.rs`：第一版曾打算掛在這裡；第二版不走 sync，只有定案事件經過它。
- `src/service/rooms/timeline/append.rs`：定案事件的入口。
- `src/router/`、`src/api/`：**沒有任何 WebSocket 或 SSE**，axum 的 `ws` feature 沒開；通道要從零加。

**純粹是判斷，不是事實**

- 「sync 在 token 頻率下會是負擔」—— 依 wake-and-repull 模型推的，沒實測；但第二版不靠這個判斷，通道反正要做。
- 「全量分片的頻寬可忽略」—— 對一則訊息的長度而言；要是有人拿它傳長文，那是用錯工具。
