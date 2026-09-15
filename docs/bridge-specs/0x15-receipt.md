# `0x15 Receipt`：輸入中、已讀

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 15 SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。

## `0x20` Typing —— 「正在輸入…」開或關

`PUT /_matrix/client/v3/rooms/{room_id}/typing/{user_id}`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost","user_id":"@alice:localhost"}` |
| 請求 data | 開：`{"typing":true,"timeout":30000}`（毫秒，過了自動關）；關：`{"typing":false}` |
| 回覆 data | `{}` |

📎 `user_id` 必須是自己；房間裡其他人從 `/sync` 的 `m.typing` 收到（通道上的推送目前不含 typing）。
**會怎麼被拒**：`user_id` 不是自己 → `Forbidden`（403）；不在房裡 → `Forbidden`（403）。

## `0x21` ReadMarkers —— 設「讀到哪裡」的標記

`POST /_matrix/client/v3/rooms/{room_id}/read_markers`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}` |
| 請求 data | `{"m.fully_read":"$Zm9vYmFy","m.read":"$Zm9vYmFy"}`（兩個都可以省略其一；還有 `m.read.private`） |
| 回覆 data | `{}` |

📎 `m.fully_read` 存成**房間的 account data**，之後用 `0x11/0x27` GetRoomAccountData（`event_type` 為 `m.fully_read`）讀得回來；`m.read` 是公開的已讀回條，別人看得到。
📎 鍵名裡的 `.` 在 data 裡照寫 —— 它是 body，不是變數。

## `0x22` Receipt —— 送一個已讀回條

`POST /_matrix/client/v3/rooms/{room_id}/receipt/{receipt_type}/{event_id}`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost","receipt_type":"m.read","event_id":"$Zm9vYmFy"}`；`receipt_type` 例：`m.read`、`m.read.private`、`m.fully_read` |
| 請求 data | `{}`，thread 的回條帶 `{"thread_id":"$root"}` |
| 回覆 data | `{}` |

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了路徑變數、多了表上沒有的變數 | `InvalidRequest`（1201） | 橋 |
| 事件不存在 | `NotFound`（404） | 端點 |
| 沒登入 | `Unauthorized`（1301） | 端點 |
