# `0x1A Search`：搜尋訊息、找人

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 1A SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。

📎 批 4 開的 kind，**沒有原生的 subtype**，所以 `0x1A` 不帶 `IS_BRIDGED`（bit4）一律是 `UnknownKind`。

## `0x20` SearchEvents —— 全文搜尋訊息

`POST /_matrix/client/v3/search`

| | |
|---|---|
| 請求 meta | 空的（長度 0）或 `{}`；翻頁時 `{"next_batch":"…"}` |
| 請求 data | `{"search_categories":{"room_events":{"search_term":"下週","keys":["content.body"],"order_by":"recent","filter":{"limit":20}}}}` |
| 回覆 data | `{"search_categories":{"room_events":{"count":3,"results":[{"rank":0.8,"result":{…事件…}}],"highlights":["下週"],"next_batch":"…"}}}` |

🚨 **加密房間搜不到內容**：server 看到的是密文，`content.body` 不存在。這不是 bug，是 E2EE 的定義（核心設計 §5.5「明確不做 server 端內容過濾」是同一件事）。**搜尋只在明文房間有用**，client 的 UI 要講清楚，不要讓使用者以為「搜不到 = 沒有」。
📎 `next_batch` 在**請求是 query 變數、在回覆是 body 的欄位** —— 同一個名字兩個位置，翻頁時要從回覆的 body 拿出來、放進下一次請求的 meta。

## `0x21` SearchUsers —— 找人（使用者目錄）

`POST /_matrix/client/v3/user_directory/search`

| | |
|---|---|
| 請求 meta | 空的或 `{}` |
| 請求 data | `{"search_term":"alice","limit":10}` |
| 回覆 data | `{"results":[{"user_id":"@alice:localhost","display_name":"Alice","avatar_url":"mxc://…"}],"limited":false}` |

📎 `limited` 是 `true` 表示結果被 `limit` 截掉了，不是「搜尋失敗」。
📎 搜得到誰由 server 決定（通常是共同房間裡的人與公開房間的成員），**不是整台 server 的帳號清單** —— client 不要把它當成「這個人不存在」的依據。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 出現表上沒有的變數 | `InvalidRequest`（1201） | 橋 |
| `0x1A` 的 pack 沒帶 bit4，或 subtype 不在表上 | `UnknownKind`（1101） | 橋 |
| 查詢的 JSON 結構不合法 | `InvalidRequest`（400 `M_BAD_JSON`） | 端點 |
| 沒登入 | `Unauthorized`（1301） | 端點 |
| 搜太快 | `RateLimited`（429 `M_LIMIT_EXCEEDED`） | 端點 |
