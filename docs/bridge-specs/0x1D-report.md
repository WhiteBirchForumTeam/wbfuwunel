# `0x1D Report`：檢舉事件、房間、使用者

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 1D SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。

📎 **這個 kind 是批 3 開的**（維護者 2026-09-20 決定：三支檢舉放一起，而不是跟著被檢舉的東西散到三個 kind）。
它**沒有原生的 subtype**，所以 `0x1D` 不帶 `IS_BRIDGED`（bit4）一律是 `UnknownKind`。

📎 **三支都只回 `{}`**：檢舉送到的是這台 server 的管理員房間，成功與否跟「有沒有人處理」無關。
📎 **`reason` 最長 2000 字元**（`REASON_MAX_LEN`），超過是 `InvalidRequest`（400 `M_INVALID_PARAM`）—— 這是端點的規則，不是橋的。

## `0x20` ReportEvent —— 檢舉一則事件

`POST /_matrix/client/v3/rooms/{room_id}/report/{event_id}`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost","event_id":"$Zm9vYmFy"}` |
| 請求 data | `{"reason":"垃圾訊息"}`；`reason` 可以省略 |
| 回覆 data | `{}` |

**會怎麼被拒**：事件不存在、或不在那個房間裡 → `NotFound`（404）；看不到那個房間的人檢舉 → `Forbidden`（403）。

## `0x21` ReportRoom —— 檢舉一個房間

`POST /_matrix/client/v3/rooms/{room_id}/report`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}` |
| 請求 data | `{"reason":"整個房間都在洗版"}` |
| 回覆 data | `{}` |

**會怎麼被拒**：這台 server 沒有任何人加入過那個房間 → `NotFound`（404）。

## `0x22` ReportUser —— 檢舉一個使用者

`POST /_matrix/client/v3/users/{user_id}/report`

| | |
|---|---|
| 請求 meta | `{"user_id":"@mallory:localhost"}` |
| 請求 data | `{"reason":"冒充別人"}` |
| 回覆 data | `{}` |

⚠️ **檢舉不存在的帳號也回 `{}`**（MSC4277）：讓回覆分辨得出帳號在不在，等於送一支查帳號的工具給人。這是端點本來的行為，橋照轉。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了路徑變數、多了表上沒有的變數 | `InvalidRequest`（1201） | 橋 |
| `0x1D` 的 pack 沒帶 bit4，或 subtype 不在表上 | `UnknownKind`（1101） | 橋 |
| `reason` 超過 2000 字元 | `InvalidRequest`（400 `M_INVALID_PARAM`） | 端點 |
| 沒登入 | `Unauthorized`（1301） | 端點 |
| 送太快 | `RateLimited`（429 `M_LIMIT_EXCEEDED`） | 端點 |
