# `0x18 Push`：推播規則、pusher、通知

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 18 SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。

📎 這個 kind 是批 4 開的，**沒有原生的 subtype**，所以 `0x18` 不帶 `IS_BRIDGED`（bit4）一律是 `UnknownKind`。

⚠️ **`scope` 不是變數。** 規格的路徑是 `/pushrules/{scope}/…`，但只有 `global` 有意義，這版 ruma 因此把它釘死了 —— 模板裡只有 `kind` 與 `rule_id`。
📎 **`kind` 這個字在這裡是「規則的種類」**（`override`／`content`／`room`／`sender`／`underride`），**不是 pack 的 kind**。同一個字兩個意思，讀的時候看它在哪一層。

## `0x20` GetPushRules / `0x21` GetGlobalPushRules —— 讀規則

`GET /_matrix/client/v3/pushrules/` ／ `GET /_matrix/client/v3/pushrules/global/`

| | |
|---|---|
| 請求 meta | 空的（長度 0）或 `{}` |
| 請求 data | — |
| 回覆 data | `0x20`：`{"global":{"override":[…],"underride":[…]}}`。`0x21`：**只有** `global` 底下那層 `{"override":[…],"underride":[…]}` |

📎 兩支的差別只是外面那層 `global`。`0x20` 是「全部 scope」，但實際上只有 `global` 一個。
⚠️ **一條規則都沒有的那幾類，鍵會整個不出現，不是空陣列。** 全新的帳號只看得到 `override` 與 `underride`（server 的預設規則在那兩類裡）；`content`／`room`／`sender` 要等你建了第一條才長出來。client 讀的時候當成「可能沒有這個鍵」，不要直接取陣列長度。
📎 每一類都是**陣列，順序就是優先序**，第一條命中的規則決定結果 —— 所以 `0x23` SetPushRule 的 `before`／`after` 才有意義。
📎 回覆裡會有 server 預設的規則（`.m.rule.master`、`.m.rule.contains_display_name`…），**開頭是 `.` 的是 server 的**，client 不要當成自己建的。

## `0x22` GetPushRule / `0x24` DeletePushRule —— 讀一條、刪一條

`GET` ／ `DELETE /_matrix/client/v3/pushrules/global/{kind}/{rule_id}`

| | |
|---|---|
| 請求 meta | `{"kind":"room","rule_id":"!AbCdEf:localhost"}` |
| 請求 data | — |
| 回覆 data | 讀：`{"rule_id":"!AbCdEf:localhost","default":false,"enabled":true,"actions":["notify"]}`（欄位隨 `kind` 不同，`content` 型多一個 `pattern`、`override`／`underride` 多 `conditions`）。刪：`{}` |

**會怎麼被拒**：沒有這條規則 → `NotFound`（404 `M_NOT_FOUND`）；`kind` 不是那五個字之一 → `InvalidRequest`（400）。

## `0x23` SetPushRule —— 新增或改一條規則

`PUT /_matrix/client/v3/pushrules/global/{kind}/{rule_id}`

| | |
|---|---|
| 請求 meta | `{"kind":"room","rule_id":"!AbCdEf:localhost"}`；要插在某條之前用 `{"kind":"room","rule_id":"…","before":"!Other:localhost"}` |
| 請求 data | 規則本身，例 `{"actions":["notify"]}`；`content` 型要 `pattern`，`override`／`underride` 要 `conditions` |
| 回覆 data | `{}` |

⚠️ **`before`／`after` 是 query 變數，不是 body 的一部分**（規格就是這樣定的）。它們決定新規則插在優先序的哪裡 —— 這是這兩個變數唯一看得出來的效果，所以 e2e 驗的是**插進去之後讀回來的順序**。
**會怎麼被拒**：`before`／`after` 指的規則不存在 → `NotFound`（404）；規則內容不合法 → `InvalidRequest`（400 `M_BAD_JSON`）。

## `0x25`–`0x28` 規則的開關與動作

`GET` ／ `PUT /_matrix/client/v3/pushrules/global/{kind}/{rule_id}/enabled`（`0x25`／`0x26`）
`GET` ／ `PUT /_matrix/client/v3/pushrules/global/{kind}/{rule_id}/actions`（`0x27`／`0x28`）

| | |
|---|---|
| 請求 meta | `{"kind":"room","rule_id":"!AbCdEf:localhost"}` |
| 請求 data | 讀：無。寫 enabled：`{"enabled":false}`。寫 actions：`{"actions":["notify",{"set_tweak":"sound","value":"default"}]}` |
| 回覆 data | 讀：`{"enabled":false}` ／ `{"actions":[…]}`。寫：`{}` |

📎 **靜音一個房間**就是這兩支：`kind` 用 `room`、`rule_id` 用房間 id，把 `actions` 設成 `[]`，或把 `enabled` 關掉。
📎 server 預設的規則（`.` 開頭）**開關得了、但刪不掉**。

## `0x29` GetPushers / `0x2A` SetPusher —— 推播目的地

`GET /_matrix/client/v3/pushers` ／ `POST /_matrix/client/v3/pushers/set`

| | |
|---|---|
| 請求 meta | 空的或 `{}` |
| 請求 data | 讀：無。寫：`{"pushkey":"<裝置的推播 token>","kind":"http","app_id":"cc.zooy.wbf","app_display_name":"WBF","device_display_name":"Pixel","lang":"zh-TW","data":{"url":"https://push.example/_matrix/push/v1/notify"}}` |
| 回覆 data | 讀：`{"pushers":[…]}`。寫：`{}` |

⚠️ **`kind` 設成 `null` 是「移除這個 pusher」**，不是「建一個 kind 為 null 的」。這是規格的形狀，橋照轉。
📎 `pushkey` ＋ `app_id` 是它的身分：同一組再送一次是**覆蓋**，不是新增一個。

## `0x2B` Notifications —— 已經產生的通知

`GET /_matrix/client/v3/notifications`

| | |
|---|---|
| 請求 meta | `{"limit":20,"only":"highlight"}`，三個都可以省略 |
| 請求 data | — |
| 回覆 data | `{"notifications":[{"room_id":…,"event":{…},"actions":[…],"read":false,"ts":…}],"next_token":"…"}` |

📎 翻頁把 `next_token` 餵回 `from`；`only` 目前只有 `highlight` 一個值。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了路徑變數、多了表上沒有的變數 | `InvalidRequest`（1201） | 橋 |
| `0x18` 的 pack 沒帶 bit4，或 subtype 不在表上 | `UnknownKind`（1101） | 橋 |
| 規則不存在 | `NotFound`（404 `M_NOT_FOUND`） | 端點 |
| 沒登入 | `Unauthorized`（1301） | 端點 |
