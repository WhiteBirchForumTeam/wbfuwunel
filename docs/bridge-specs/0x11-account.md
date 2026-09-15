# `0x11 Account`：帳號、profile、帳號設定、房間標籤

> 號碼總表與共通規則在 [index.md](index.md)（怎麼讀 header、變數規則、回覆長什麼樣）。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 11 SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0，`seq` 是 client 的請求號。
> 範例裡的 `meta` 是 JSON 文字、`data` 是 HTTP body 的 bytes（這裡用 JSON 文字寫出來）。回覆的 `data` 就是 Matrix 回應的 body，原樣。

## `0x20` WhoAmI —— 查「我是誰」

`GET /_matrix/client/v3/account/whoami`

| | |
|---|---|
| 請求 meta | 空（長度 0）或 `{}` |
| 請求 data | 空 |
| 回覆 meta | `{"headers":{"content-type":"application/json"},"status":200}` |
| 回覆 data | `{"device_id":"RJYKSTBOIE","is_guest":false,"user_id":"@alice:localhost"}` |

**會怎麼被拒**：沒登入的連線 → `Error` meta `{"code":"Unauthorized","code_id":1301,"errcode":"M_MISSING_TOKEN","message":"Missing access token.","status":401}`。
📎 這個拒絕是**端點自己**給的（跟 HTTP 一樣），不是橋：橋只負責把 session 的 token 帶過去，沒有 token 就不帶。

## `0x21` GetProfile —— 拿某人的整份 profile

`GET /_matrix/client/v3/profile/{user_id}`

| | |
|---|---|
| 請求 meta | `{"user_id":"@bob:localhost"}` |
| 請求 data | 空 |
| 回覆 data | `{"avatar_url":"mxc://localhost/abc","displayname":"Bob"}`（沒設的欄位不出現） |

**會怎麼被拒**：不存在的使用者 → `NotFound`（404，`M_NOT_FOUND`）。

## `0x22` GetProfileField —— 拿 profile 的一個欄位

`GET /_matrix/client/v3/profile/{user_id}/{field}`

| | |
|---|---|
| 請求 meta | `{"user_id":"@bob:localhost","field":"displayname"}`；`field` 例：`displayname`、`avatar_url` |
| 請求 data | 空 |
| 回覆 data | `{"displayname":"Bob"}` |

**會怎麼被拒**：使用者不存在、或那個欄位沒設 → `NotFound`（404，`M_NOT_FOUND`）。

## `0x23` SetProfileField —— 改自己 profile 的一個欄位

`PUT /_matrix/client/v3/profile/{user_id}/{field}`

| | |
|---|---|
| 請求 meta | `{"user_id":"@alice:localhost","field":"displayname"}` |
| 請求 data | `{"displayname":"Alice"}` —— 鍵名跟 `field` 一樣 |
| 回覆 data | `{}` |

**會怎麼被拒**：改別人的 → `Forbidden`（403，`M_FORBIDDEN`）；**帳號被暫停** → `Forbidden`（403，`M_USER_SUSPENDED`）。

## `0x24` DeleteProfileField —— 刪自己 profile 的一個欄位

`DELETE /_matrix/client/v3/profile/{user_id}/{field}`

| | |
|---|---|
| 請求 meta | `{"user_id":"@alice:localhost","field":"displayname"}` |
| 請求 data | 空 |
| 回覆 data | `{}` |

**會怎麼被拒**：同 SetProfileField。

## `0x25` GetAccountData —— 讀帳號層的設定

`GET /_matrix/client/v3/user/{user_id}/account_data/{event_type}`

app 自己存在 server 上的東西（通知偏好、置頂清單…），`event_type` 由 app 自己取名，建議用反向網域（`com.example.settings`）。

| | |
|---|---|
| 請求 meta | `{"user_id":"@alice:localhost","event_type":"com.example.settings"}` |
| 請求 data | 空 |
| 回覆 data | 當初寫進去的內容，例 `{"answer":42}` |

**會怎麼被拒**：沒寫過 → `NotFound`（404，`M_NOT_FOUND`）；讀別人的 → `Forbidden`（403）。

## `0x26` SetAccountData —— 寫帳號層的設定

`PUT /_matrix/client/v3/user/{user_id}/account_data/{event_type}`

| | |
|---|---|
| 請求 meta | `{"user_id":"@alice:localhost","event_type":"com.example.settings"}` |
| 請求 data | 任意 JSON 物件，例 `{"answer":42}` |
| 回覆 data | `{}` |

📎 內容**鍵叫什麼都可以**（包括 `room_id`），因為它在 data、不在 meta —— 這正是 body 不跟變數共用命名空間的原因（[設計 §2.2](../design/wbf-api-bridge.md)）。

## `0x27` GetRoomAccountData —— 讀某個房間的設定

`GET /_matrix/client/v3/user/{user_id}/rooms/{room_id}/account_data/{event_type}`

| | |
|---|---|
| 請求 meta | `{"user_id":"@alice:localhost","room_id":"!r:localhost","event_type":"com.example.room"}` |
| 請求 data | 空 |
| 回覆 data | 例 `{"pinned":true}` |

**會怎麼被拒**：同 GetAccountData。

## `0x28` SetRoomAccountData —— 寫某個房間的設定

`PUT /_matrix/client/v3/user/{user_id}/rooms/{room_id}/account_data/{event_type}`

| | |
|---|---|
| 請求 meta | `{"user_id":"@alice:localhost","room_id":"!r:localhost","event_type":"com.example.room"}` |
| 請求 data | 任意 JSON 物件，例 `{"pinned":true}` |
| 回覆 data | `{}` |

## `0x29` GetTags —— 某個房間的標籤

`GET /_matrix/client/v3/user/{user_id}/rooms/{room_id}/tags`

| | |
|---|---|
| 請求 meta | `{"user_id":"@alice:localhost","room_id":"!r:localhost"}` |
| 請求 data | 空 |
| 回覆 data | `{"tags":{"m.favourite":{"order":0.1},"u.work":{"order":0.5}}}`；沒有標籤是 `{"tags":{}}` |

## `0x2A` SetTag —— 加或改一個標籤

`PUT /_matrix/client/v3/user/{user_id}/rooms/{room_id}/tags/{tag}`

| | |
|---|---|
| 請求 meta | `{"user_id":"@alice:localhost","room_id":"!r:localhost","tag":"u.work"}`；Matrix 定義的有 `m.favourite`、`m.lowpriority`，自訂的用 `u.` 開頭 |
| 請求 data | `{"order":0.5}`（`order` 可省略） |
| 回覆 data | `{}` |

## `0x2B` DeleteTag —— 拿掉一個標籤

`DELETE /_matrix/client/v3/user/{user_id}/rooms/{room_id}/tags/{tag}`

| | |
|---|---|
| 請求 meta | `{"user_id":"@alice:localhost","room_id":"!r:localhost","tag":"u.work"}` |
| 請求 data | 空 |
| 回覆 data | `{}` |

## `0x2C` ChangePassword —— 改密碼（批 2，要 UIAA）

`POST /_matrix/client/v3/account/password`

| | |
|---|---|
| 請求 meta | 空 |
| 請求 data | 第一輪 `{"new_password":"…","logout_devices":true}`；第二輪加 `"auth":{"type":"m.login.password","session":"…","identifier":{"type":"m.id.user","user":"erin"},"password":"<舊密碼>"}` |
| 第一輪回覆 | `Error`，`status` 401；data `{"flows":[{"stages":["m.login.password"]}],"params":{},"session":"…"}`（index §1.5） |
| 成功回覆 data | `{}` |

- `logout_devices: true`：**其他**裝置被登出，發請求的這個裝置保留 —— 所以這條連線照常可用（e2e13 [2.12]）。

**會怎麼被拒**：舊密碼錯 → `{"code":"Unauthorized","code_id":1301,"errcode":"M_FORBIDDEN","message":"Invalid username or password.","status":401}`，data 仍帶同一個 `session`，可以再試。

## `0x2D` Deactivate —— 停用帳號（批 2，要 UIAA）

`POST /_matrix/client/v3/account/deactivate`

| | |
|---|---|
| 請求 meta | 空 |
| 請求 data | 第一輪 `{}`（要抹除加 `"erase":true`）；第二輪加 `auth`，同上 |
| 第一輪回覆 | `Error`，`status` 401；data `{"flows":[{"stages":["m.login.password"]}],"params":{},"session":"…"}` |
| 成功回覆 data | `{"id_server_unbind_result":"no-support"}` |

⚠️ **停用之後這條連線就不算數了**：回覆照樣先到，**下一個 pack** 在橋之前被拒 —— `{"code":"Unauthorized","code_id":1301,"errcode":"M_UNKNOWN_TOKEN",…}`、沒有 bit4、關連線（index §1.2；e2e13 [2.14]）。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了路徑變數、多了表上沒有的變數、變數不是字串 | `InvalidRequest`（1201） | 橋（不會呼叫端點） |
| `id` 不是 0 | `InvalidRequest`（1201） | 橋 |
| 號碼不在總表 | `UnknownKind`（1101） | 橋 |
| 沒登入 | `Unauthorized`（1301，`M_MISSING_TOKEN`） | 端點 |
| 動別人的資料 | `Forbidden`（1302，`M_FORBIDDEN`） | 端點 |
| 太快 | `RateLimited`（1401，帶 `retry_after_ms`） | 端點 |
