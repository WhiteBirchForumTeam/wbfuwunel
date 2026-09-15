# `0x14 Event`：單則訊息、房間狀態、收回、前後文

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 14 SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。
> ⚠️ 這個 kind 同時有**原生**的 pack：`Recent`（`0x01`）、`Send`（`0x02`）、`Batch`（`0x03`）、`Subscribe`（`0x04`）、`Unsubscribe`（`0x05`）、`Push`（`0x06`）。那些**不帶** bit4，規格在 [wbf-wire-format.md](../design/wbf-wire-format.md) §3.2。帶了 bit4 送原生的號碼會回 `UnknownKind`。

## `0x20` GetEvent —— 用 event id 拿單一則

`GET /_matrix/client/v3/rooms/{room_id}/event/{event_id}`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost","event_id":"$Zm9vYmFy"}` |
| 請求 data | 空 |
| 回覆 data | 那則事件的 JSON：`{"event_id":"$Zm9vYmFy","type":"m.room.message","sender":"@bob:localhost","content":{…},"origin_server_ts":…,"room_id":"!AbCdEf:localhost","unsigned":{…}}` |

📎 event id 裡的 `$`、`/`、`+` 照原樣寫在 meta，橋會編碼。被收回的事件照樣回，`content` 是 `{}`、`unsigned` 帶 `redacted_because`。
**會怎麼被拒**：不存在或看不到 → `NotFound`（404，`M_NOT_FOUND`）。

## `0x21` GetState —— 房間目前的全部狀態

`GET /_matrix/client/v3/rooms/{room_id}/state`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}` |
| 請求 data | 空 |
| 回覆 data | 狀態事件的陣列：`[{"type":"m.room.create","state_key":"",…},{"type":"m.room.member","state_key":"@alice:localhost",…},…]` |

⚠️ 大房間的狀態可能很大。回覆的 body 超過 `wbf_data_max_bytes`（2 MiB）會回 `TooLarge`，**不會切成好幾包** —— 那種情況改用 GetStateEvent 一項一項拿，或 `Recent`。
**會怎麼被拒**：不在房裡、也看不到 → `Forbidden`（403）。

## `0x22` GetStateEvent —— 房間的某一項狀態

`GET /_matrix/client/v3/rooms/{room_id}/state/{event_type}/{state_key}`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost","event_type":"m.room.topic","state_key":""}` |
| 請求 data | 空 |
| 回覆 data | **只有 content**：`{"topic":"大家好"}` |

⚠️ **`state_key` 最常是空字串，而空字串是一個值**：`m.room.name`、`m.room.topic`、`m.room.avatar`、`m.room.power_levels` 都是 `""`。**不能省略**，省略就是缺變數（`InvalidRequest`）。成員狀態的 `state_key` 是那個人的 user id：`{"event_type":"m.room.member","state_key":"@bob:localhost"}`。
**會怎麼被拒**：沒有這一項 → `NotFound`（404）。

## `0x23` SetStateEvent —— 改房間的某一項狀態

`PUT /_matrix/client/v3/rooms/{room_id}/state/{event_type}/{state_key}`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost","event_type":"m.room.topic","state_key":""}` |
| 請求 data | 那一項的 content：`{"topic":"大家好"}`；改名是 `m.room.name` ＋ `{"name":"週末聚餐"}` |
| 回覆 data | `{"event_id":"$t0p1c"}` |

完整的 bytes 範例在向量檔 `bridge_set_state_event` 與 `bridge_ack`（[wbf-vectors.json](../design/wbf-vectors.json)，由 server 的程式碼產生）。
**會怎麼被拒**：權限不夠 → `Forbidden`（403，`M_FORBIDDEN`）：
```
meta  {"code":"Forbidden","code_id":1302,"errcode":"M_FORBIDDEN","message":"You don't have permission to post that to the room.","status":403}
data  {"errcode":"M_FORBIDDEN","error":"You don't have permission to post that to the room."}
```
（向量 `bridge_error_forbidden`）

## `0x24` Redact —— 收回一則訊息

`PUT /_matrix/client/v3/rooms/{room_id}/redact/{event_id}/{txn_id}`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost","event_id":"$Zm9vYmFy","txn_id":"redact-7f3a"}` |
| 請求 data | `{}`，可帶 `{"reason":"打錯字"}` |
| 回覆 data | `{"event_id":"$r3d4ct"}` —— 這是**收回動作本身**那則事件的 id |

📎 `txn_id` 由 client 取，同一個 `txn_id` 重送只會收回一次（Matrix 的冪等規則）；網路斷掉重送時沿用同一個。
**會怎麼被拒**：收回別人的訊息而權限不夠 → `Forbidden`（403）。

## `0x25` Context —— 跳到某則訊息，連同它前後幾則

`GET /_matrix/client/v3/rooms/{room_id}/context/{event_id}`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost","event_id":"$Zm9vYmFy","limit":10}` |
| query 變數 | `limit`（數字，前後合計幾則，預設 10）、`filter`（**字串形式的 JSON**，例 `"{\"types\":[\"m.room.message\"]}"`） |
| 請求 data | 空 |
| 回覆 data | `{"event":{…},"events_before":[…],"events_after":[…],"state":[…],"start":"…","end":"…"}` |

⚠️ `filter` 在 Matrix 的 URL 裡就是一段 JSON **字串**，所以在 meta 裡也要是字串，🚫 不要放 JSON 物件（物件不是合法的 query 值，會 `InvalidRequest`）。
**會怎麼被拒**：事件不存在或看不到 → `NotFound`（404）。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了路徑變數（最常見：漏了 `state_key: ""`）、多了表上沒有的變數、變數型別不對 | `InvalidRequest`（1201） | 橋 |
| 回覆的 body 大於一個 pack | `TooLarge`（1103） | 橋 |
| 沒登入 | `Unauthorized`（1301） | 端點 |
