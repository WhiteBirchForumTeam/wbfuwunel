# `0x12 Sync`：過濾器

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 12 SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。

📎 **`/sync` 本身不走橋**：long-poll 在通道上的形狀是 server 推送（`Event/Subscribe`＋`Push`），那是另一件事（橋的設計 §3「不搬」）。
這個 kind 這一批只用來放**過濾器**——它們是獨立的端點，`/messages`、`/context` 這些也吃 `filter` 參數。
📎 這個 kind **沒有原生的 subtype**，所以 `0x12` 不帶 `IS_BRIDGED`（bit4）一律是 `UnknownKind`。

## `0x20` GetFilter —— 讀一個存過的過濾器

`GET /_matrix/client/v3/user/{user_id}/filter/{filter_id}`

| | |
|---|---|
| 請求 meta | `{"user_id":"@alice:localhost","filter_id":"1"}` |
| 請求 data | — |
| 回覆 data | 當初存進去的那份過濾器，例 `{"room":{"timeline":{"limit":20}}}` |

**會怎麼被拒**：沒有這個 id → `NotFound`（404）；`user_id` 不是自己 → `Forbidden`（403）。

## `0x21` CreateFilter —— 存一個過濾器，拿到它的 id

`POST /_matrix/client/v3/user/{user_id}/filter`

| | |
|---|---|
| 請求 meta | `{"user_id":"@alice:localhost"}` |
| 請求 data | 過濾器本身，例 `{"room":{"timeline":{"limit":20},"state":{"lazy_load_members":true}}}` |
| 回覆 data | `{"filter_id":"1"}` |

📎 `filter_id` 是字串，不要當數字用。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了路徑變數、多了表上沒有的變數 | `InvalidRequest`（1201） | 橋 |
| `0x12` 的 pack 沒帶 bit4，或 subtype 不在表上 | `UnknownKind`（1202） | 橋 |
| 過濾器不是合法的 JSON 結構 | `InvalidRequest`（400 `M_BAD_JSON`） | 端點 |
| 沒登入 | `Unauthorized`（1301） | 端點 |
