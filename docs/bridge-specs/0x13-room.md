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
| query 變數 | `membership`、`not_membership`（`join`／`invite`／`leave`／`ban`／`knock`） |
| 請求 data | 空 |
| 回覆 data | `{"chunk":[{"type":"m.room.member","state_key":"@alice:localhost","content":{"membership":"join","displayname":"Alice"},"unsigned":{"org.wbftw.device_version":"3-810b7c3be4",…},…},…],"org.wbftw.room_version":81234}` |

- **兩個 Matrix 沒有的欄位**（[wbf-room-device-version.md](../design/wbf-room-device-version.md) §5）：已加入的成員（`membership` 是 `join`）的 `unsigned["org.wbftw.device_version"]` 是他的裝置版本號；最外層的 `org.wbftw.room_version` 是房間版本號，跟清單是同一次讀到的房間狀態算的。不認得的 client 照 Matrix 的規則略過它們；HTTP 的回應一樣帶。
- 🚫 **沒有 `at`**：上游忽略它（`// TODO`），帶了也拿不到「某個時間點的名單」，所以橋的表不收；帶了是橋自己的 `InvalidRequest`，不會默默忽略。

**會怎麼被拒**：不在房裡、也看不到歷史 → `Forbidden`（403）；帶 `at` → `InvalidRequest`（1201，橋自己擋的，不會呼叫端點）。

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

## `0x2D` Upgrade —— 把房間換成新的房間版本（批 3）

`POST /_matrix/client/v3/rooms/{room_id}/upgrade`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}` |
| 請求 data | `{"new_version":"11"}` |
| 回覆 data | `{"replacement_room":"!NewRoom:localhost"}` |

📎 舊房間會被加上 `m.room.tombstone` 指向新房間，成員自己走過去；**舊房間不會消失**。
**會怎麼被拒**：沒有改狀態的權限 → `Forbidden`（403）；`new_version` 這台 server 不支援 → `InvalidRequest`（400 `M_UNSUPPORTED_ROOM_VERSION`）—— 支援哪些問 `0x1C/0x20` Capabilities。

## `0x2E` Knock —— 敲門（批 3）

`POST /_matrix/client/v3/knock/{room_id_or_alias}`

| | |
|---|---|
| 請求 meta | `{"room_id_or_alias":"!AbCdEf:localhost","via":["other.example"]}`；`via` 與 `server_name` 都是 query 陣列，跟 `0x21` Join 同一套 |
| 請求 data | `{}`，可帶 `{"reason":"我是 alice 的朋友"}` |
| 回覆 data | `{"room_id":"!AbCdEf:localhost"}` |

📎 敲完是**等房內的人放行**：成員狀態變 `knock`，有人 `Invite` 之後才進得去。
**會怎麼被拒**：房間的加入規則不是 `knock` → `Forbidden`（403）；被封鎖過 → `Forbidden`（403）；帳號被暫停 → `Forbidden`（403 `M_USER_SUSPENDED`）。

## `0x2F` JoinedMembers —— 只要已加入的成員（批 3）

`GET /_matrix/client/v3/rooms/{room_id}/joined_members`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}` |
| 請求 data | — |
| 回覆 data | `{"joined":{"@alice:localhost":{"display_name":"Alice","avatar_url":"mxc://…"}}}` |

📎 跟 `0x29` Members 的差別：Members 回的是**成員事件**（含離開、被邀請的），這支只回**已加入的人**與他們的顯示名、頭像，輕很多。
⚠️ 這支**沒有**房間版本號（那是 `0x29` Members 專有的，見 [wbf-room-device-version.md](../design/wbf-room-device-version.md) §5.2）。要送加密訊息前比對的 client 要打 Members，不是這支。
**會怎麼被拒**：自己不在房裡 → `Forbidden`（403）。

## `0x30` GetVisibility / `0x31` SetVisibility —— 房間在不在公開目錄上（批 3）

`GET` ／ `PUT /_matrix/client/v3/directory/list/room/{room_id}`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}` |
| 請求 data | 讀：無。寫：`{"visibility":"public"}` 或 `{"visibility":"private"}` |
| 回覆 data | 讀：`{"visibility":"public"}`。寫：`{}` |

📎 **讀那支端點不要求登入**（`NoAccessToken`），所以匿名連線也問得到 —— 跟匿名 HTTP 一樣。
**會怎麼被拒**：寫的時候沒有權限（server 設定只准管理員上架） → `Forbidden`（403）。

## `0x32` Summary —— 房間簡介（批 3）

`GET /_matrix/client/v1/room_summary/{room_id_or_alias}`

| | |
|---|---|
| 請求 meta | `{"room_id_or_alias":"#lobby:localhost","via":["other.example"]}` |
| 請求 data | — |
| 回覆 data | `{"room_id":"!AbCdEf:localhost","name":"Lobby","num_joined_members":12,"world_readable":true,"guest_can_join":false,"room_version":"11","membership":"join"}` |

⚠️ **摘要的欄位是攤平在最外層的，沒有 `summary` 那層外殼**（MSC3266 定的線上格式）。📎 ruma 的 `Response` 型別確實有一個叫 `summary` 的欄位，但它序列化時帶 `#[serde(flatten)]`，所以**讀 Rust 的欄位名會猜錯這個形狀** —— 以線上實際的 bytes 為準（e2e13 [5.7b] 釘住）。
📎 `membership` 只有帶 token 問的時候才有，匿名問就沒有這個欄位。

📎 **沒加入也看得到**（端點是 `AccessTokenOptional`）：這支就是給「點到一個連結，要不要進去」那一步用的。
⚠️ 橋上**只有這條穩定路徑**。MSC 還沒定案時的舊 URL（`/_matrix/client/unstable/im.nheko.summary/…`）在 HTTP 上照舊在，但不另給 subtype 號（橋的設計 §3 批 3-B 第 2 點，維護者 2026-09-20 定）。

## `0x33` Hierarchy —— space 底下的房間樹（批 3）

`GET /_matrix/client/v1/rooms/{room_id}/hierarchy`

| | |
|---|---|
| 請求 meta | `{"room_id":"!TheSpace:localhost","limit":20,"suggested_only":false}` |
| 請求 data | — |
| 回覆 data | `{"rooms":[{"room_id":"!child:localhost","children_state":[…],…}],"next_batch":"…"}` |

📎 翻頁用 `next_batch` 餵回 `from`；`max_depth` 限制往下幾層。

## `0x34` MutualRooms —— 我與某人共同在哪些房間（批 3）

`GET /_matrix/client/v1/mutual_rooms`

| | |
|---|---|
| 請求 meta | `{"user_id":"@bob:localhost"}` —— ⚠️ `user_id` 是 **query 變數，不是路徑的一段** |
| 請求 data | — |
| 回覆 data | `{"joined":["!AbCdEf:localhost"],"count":1,"next_batch":"…"}`；`count` 一定有（即使這頁被截斷，它算的是總數），`next_batch` 只有還有下一頁才出現 |

⚠️ 橋上**只有這條穩定路徑**（`uk.half-shot.msc2666` 那條舊 URL 同 `0x32` 的說明）。

## `0x35` PublicRooms / `0x36` PublicRoomsFiltered —— 公開房間目錄（批 4）

`GET` ／ `POST /_matrix/client/v3/publicRooms`

| | |
|---|---|
| 請求 meta | `0x35`：`{"limit":20}`，可加 `since`、`server`。`0x36`：只有 `{"server":"other.example"}`（其餘條件在 data 裡） |
| 請求 data | `0x35`：無。`0x36`：`{"filter":{"generic_search_term":"讀書"},"limit":20,"since":"…","room_types":[null,"m.space"]}` |
| 回覆 data | `{"chunk":[{"room_id":…,"name":…,"num_joined_members":12,"topic":…,"join_rule":"public",…}],"next_batch":"…","prev_batch":"…","total_room_count_estimate":3}` |

📎 兩支是**同一個目錄的兩種問法**：`GET` 只能翻頁，`POST` 可以帶搜尋字串與過濾條件。要搜尋就用 `0x36`。
📎 `server` 是「問哪一台的目錄」；這個 fork 預設不開聯邦，所以通常省略。
📎 房間要先被 `0x31` SetVisibility 設成 `public` 才會出現在這裡 —— **公開房間（join_rule）跟上架目錄（visibility）是兩件事**，房間可以是公開的但不上架。

## `0x37` RoomAliases —— 這個房間有哪些別名（批 4）

`GET /_matrix/client/v3/rooms/{room_id}/aliases`

| | |
|---|---|
| 請求 meta | `{"room_id":"!AbCdEf:localhost"}` |
| 請求 data | — |
| 回覆 data | `{"aliases":["#lobby:localhost","#main:localhost"]}` |

📎 跟 `0x2A` GetAlias 方向相反：那支是「別名 → 房間」，這支是「房間 → 它所有的別名」。
**會怎麼被拒**：房間不是世界可讀、而自己又不在裡面 → `Forbidden`（403）。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了路徑變數、多了表上沒有的變數（例：把 Invite 的 `user_id` 放進 meta）、變數型別不對 | `InvalidRequest`（1201） | 橋（不會呼叫端點） |
| 沒登入 | `Unauthorized`（1301，`M_MISSING_TOKEN`） | 端點 |
| 帳號被暫停（MSC3823）：CreateRoom、Join、Invite、Kick、Ban、Unban | `Forbidden`（1302，`M_USER_SUSPENDED`，status 403，MSC3823）。📎 批 2 之前這個 server 回的是 400（狀態碼對應表漏列），批 2 修正 | 端點前的 HTTP 關卡 —— 橋走的就是那一道 |
