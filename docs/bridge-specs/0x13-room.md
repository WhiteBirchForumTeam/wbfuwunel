# `0x13 Room`：開房、成員、別名

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 13 SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。
> 範例的 `meta` 是 JSON 文字、`data` 是 HTTP body（用 JSON 文字寫出來）；回覆的 `data` 是 Matrix 回應的 body，原樣。

## `0x20` CreateRoom —— 開房間

`POST /_matrix/client/v3/createRoom`

| | |
|---|---|
| 請求 meta | 空或 `{}`（這支沒有變數） |
| 請求 data | `{"name":"週末聚餐","preset":"private_chat","invite":["@bob:localhost"]}`；其他欄位照 Matrix 規格（`room_alias_name`、`topic`、`initial_state`、`is_direct`…）。加密房間放在 `initial_state` 裡的 `m.room.encryption` |
| 回覆 data | `{"room_id":"!AbCdEf:localhost"}` |

**會怎麼被拒**：**帳號被暫停** → `Forbidden`（403，`M_USER_SUSPENDED`）；別名已被使用 → `InvalidRequest`（400，`M_ROOM_IN_USE`）。

## `0x21` Join —— 用房間 id 或 `#別名` 加入

`POST /_matrix/client/v3/join/{room_id_or_alias}`

| | |
|---|---|
| 請求 meta | `{"room_id_or_alias":"#lobby:localhost"}`，或 `{"room_id_or_alias":"!AbCdEf:localhost","via":["localhost"]}` |
| query 變數 | `via`（陣列，要去哪幾台 server 找這個房）、`server_name`（陣列，舊名字，同義） |
| 請求 data | `{}`，可帶 `reason` |
| 回覆 data | `{"room_id":"!AbCdEf:localhost"}` |

📎 `#` 在 meta 裡照原樣寫，橋會 percent-encode 成 `%23`；client 🚫 不要自己先編碼（會變成 `%2523`）。
**會怎麼被拒**：沒被邀請的私人房 → `Forbidden`（403）；被封鎖 → `Forbidden`（403）；帳號被暫停 → `Forbidden`（403，`M_USER_SUSPENDED`）；別名不存在 → `NotFound`（404）。

## `0x22` Leave —— 離開房間

`POST /_matrix/client/v3/rooms/{room_id}/leave`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}` |
| 請求 data | `{}`，可帶 `{"reason":"bye"}` |
| 回覆 data | `{}` |

## `0x23` Forget —— 把離開的房從自己的清單移除

`POST /_matrix/client/v3/rooms/{room_id}/forget`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}` |
| 請求 data | `{}` |
| 回覆 data | `{}` |

**會怎麼被拒**：還在房裡 → `InvalidRequest`（400，`M_UNKNOWN`）—— 要先 Leave。

## `0x24` Invite —— 邀請某人

`POST /_matrix/client/v3/rooms/{room_id}/invite`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}` |
| 請求 data | `{"user_id":"@carol:localhost","reason":"來吃飯"}`（`reason` 可省略） |
| 回覆 data | `{}` |

📎 被邀請的人放在 **data** 的 `user_id`，不是 meta —— 它是 body 的欄位，不是路徑變數。放進 meta 會被當成**表上沒有的變數**拒絕。
**會怎麼被拒**：沒有邀請的權限 → `Forbidden`（403，`M_FORBIDDEN`）；帳號被暫停 → `Forbidden`（403，`M_USER_SUSPENDED`）。

## `0x25` Kick / `0x26` Ban / `0x27` Unban —— 踢人、封鎖、解除封鎖

`POST /_matrix/client/v3/rooms/{room_id}/kick`、`…/ban`、`…/unban`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}` |
| 請求 data | `{"user_id":"@bob:localhost","reason":"spam"}`（`reason` 可省略） |
| 回覆 data | `{}` |

**會怎麼被拒**：權限不夠 → `Forbidden`（403，`M_FORBIDDEN`）。e2e13 [1.14] 實跑出來的回覆（一般成員踢人）：
```
meta  {"code":"Forbidden","code_id":1302,"errcode":"M_FORBIDDEN","message":"Auth check failed: sender does not have enough power to kick target user","status":403}
data  {"errcode":"M_FORBIDDEN","error":"Auth check failed: sender does not have enough power to kick target user"}
```
帳號被暫停 → `Forbidden`（403，`M_USER_SUSPENDED`）。

## `0x28` JoinedRooms —— 我加入了哪些房間

`GET /_matrix/client/v3/joined_rooms`

| | |
|---|---|
| 請求 meta | 空或 `{}` |
| 請求 data | 空 |
| 回覆 data | `{"joined_rooms":["!AbCdEf:localhost","!GhIjKl:localhost"]}` |

## `0x29` Members —— 房間成員名單

`GET /_matrix/client/v3/rooms/{room_id}/members`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}`；只要加入中的：`{"room_id":"!AbCdEf:localhost","membership":"join"}` |
| query 變數 | `membership`、`not_membership`（`join`／`invite`／`leave`／`ban`／`knock`）、`at`（分頁 token，取某個時間點的名單） |
| 請求 data | 空 |
| 回覆 data | `{"chunk":[{"type":"m.room.member","state_key":"@alice:localhost","content":{"membership":"join","displayname":"Alice"},…},…]}` |

**會怎麼被拒**：不在房裡、也看不到歷史 → `Forbidden`（403）。

## `0x2A` GetAlias —— 用 `#別名` 查房間 id

`GET /_matrix/client/v3/directory/room/{room_alias}`

| | |
|---|---|
| 請求 meta | `{"room_alias":"#lobby:localhost"}` |
| 請求 data | 空 |
| 回覆 data | `{"room_id":"!AbCdEf:localhost","servers":["localhost"]}` |

**會怎麼被拒**：別名不存在 → `NotFound`（404，`M_NOT_FOUND`）。

## `0x2B` SetAlias —— 替房間設一個別名

`PUT /_matrix/client/v3/directory/room/{room_alias}`

| | |
|---|---|
| 請求 meta | `{"room_alias":"#lobby:localhost"}` |
| 請求 data | `{"room_id":"!AbCdEf:localhost"}` |
| 回覆 data | `{}` |

**會怎麼被拒**：別名已被使用 → `Conflict`（409，`M_UNKNOWN`）；沒有權限 → `Forbidden`（403）。

## `0x2C` DeleteAlias —— 刪別名

`DELETE /_matrix/client/v3/directory/room/{room_alias}`

| | |
|---|---|
| 請求 meta | `{"room_alias":"#lobby:localhost"}` |
| 請求 data | 空 |
| 回覆 data | `{}` |

**會怎麼被拒**：不存在 → `NotFound`（404）；不是設的人、也不是房間管理員 → `Forbidden`（403）。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了路徑變數、多了表上沒有的變數（例：把 Invite 的 `user_id` 放進 meta）、變數型別不對 | `InvalidRequest`（1201） | 橋（不會呼叫端點） |
| 沒登入 | `Unauthorized`（1301，`M_MISSING_TOKEN`） | 端點 |
| 帳號被暫停（MSC3823）：CreateRoom、Join、Invite、Kick、Ban、Unban | `Forbidden`（1302，`M_USER_SUSPENDED`，status 403，MSC3823）。📎 批 2 之前這個 server 回的是 400（狀態碼對應表漏列），批 2 修正 | 端點前的 HTTP 關卡 —— 橋走的就是那一道 |
