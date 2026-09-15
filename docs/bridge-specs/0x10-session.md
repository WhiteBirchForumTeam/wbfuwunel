# `0x10 Session`：註冊與登入前

> 號碼總表與共通規則在 [index.md](index.md)；UIAA 的兩輪在 index §1.5。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 10 SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。
> ⚠️ 這個 kind 同時有**原生**的 pack：`Login`（`0x01`）、`Refresh`（`0x02`）、`Logout`（`0x03`）。那些**不帶** bit4、會改變這條連線的身份，規格在 [wbf-wire-format.md](../design/wbf-wire-format.md) §6.3。這份的四支**都不改變連線身份**。
> 📎 這四支通常在**沒登入的連線**上送（橋不帶 token，就像沒帶 `Authorization` 的 HTTP 請求）。沒登入的 WS 連線只活 `wbf_ws_unauthenticated_timeout`（預設 30 秒，從升級算起）。HTTP pack 在傳輸層就要 token，所以匿名只能走 WS。
> 範例裡的 JSON 是 e2e13 情境 2 實跑的輸出（`session`、token 這類值每次不同）。

## 註冊一個帳號，並讓這條連線變成它

```
→ 01 10 21 10  UsernameAvailable    meta {"username":"dave"}                            （可選）
← Ack          data {"available":true}

→ 01 10 20 10  Register             data {"username":"dave","password":"…","inhibit_login":true}
← Error        meta {"code":"Unauthorized","code_id":1301,"message":"the endpoint answered 401 Unauthorized","status":401}
               data {"flows":[{"stages":["m.login.dummy"]}],"session":"g1RY2IlOBIrHdI3K2UQxVa3JMBOzjDqx"}

→ 01 10 20 10  Register             data {"username":"dave","password":"…","inhibit_login":true,
                                          "auth":{"type":"m.login.dummy","session":"g1RY2IlOBIrHdI3K2UQxVa3JMBOzjDqx"}}
← Ack          data {"user_id":"@dave:localhost","device_id":null}

→ 01 10 01 00  Login（原生）         meta {"type":"m.login.password","identifier":{"type":"m.id.user","user":"dave"},"password":"…"}
← Ack          meta {"access_token":"…","device_id":"FCxXiM5N8c","user_id":"@dave:localhost"}
```

⭐ **一定要帶 `inhibit_login: true`，再送 `Login`**：`Login` 才會把這條連線變成那個帳號，而且會過裝置的連線名額（`wbf_ws_max_connections_per_device`）與登入限速。
⚠️ 沒帶 `inhibit_login`：server 照 HTTP 的行為建裝置、發 token，token 在回覆的 data 裡，但**這條連線不會變成那個帳號**（維護者 2026-09-15 定：照 HTTP 放行，橋不看 body）。訪客註冊（`kind: guest`）不能 `inhibit_login`，同上。

## `0x20` Register —— 註冊帳號

`POST /_matrix/client/v3/register`

| | |
|---|---|
| 請求 meta | 空；訪客註冊是 `{"kind":"guest"}`（query 變數，省略是 `user`） |
| 請求 data | `/register` 的 body：`username`、`password`、`inhibit_login: true`、`initial_device_display_name`?；第二輪加 `auth` |
| 第一輪回覆 | `Error`，`status` 401、沒有 `errcode`；data 是 `{"flows":[…],"session":"…"}`（index §1.5） |
| 成功回覆 data | `{"user_id":"@dave:localhost","device_id":null}`（`inhibit_login` 時沒有裝置、沒有 token） |

**`flows` 看 server 設定**：沒設註冊碼 → `[{"stages":["m.login.dummy"]}]`；設了 `registration_token` → `[{"stages":["m.login.registration_token"]}]`，第二輪的 `auth` 是 `{"type":"m.login.registration_token","token":"…","session":"…"}`。另外可能有同意條款（`m.login.terms`）、email（`m.login.email.identity`）。

**會怎麼被拒**（全是 HTTP 那一道本身，e2e13 逐條跟 HTTP 比過）：

| 情況 | 回覆 meta |
|---|---|
| 帳號名已被使用（包括伺服器帳號 `system`） | `{"code":"InvalidRequest","code_id":1201,"errcode":"M_USER_IN_USE","message":"M_USER_IN_USE: User ID is not available.","status":400}` —— **在 UIAA 之前**就拒 |
| 帳號名符合 `forbidden_usernames` | `{"code":"Forbidden","code_id":1302,"errcode":"M_FORBIDDEN","message":"M_FORBIDDEN: Username is forbidden","status":403}` |
| 註冊碼錯 | `{"code":"Unauthorized","code_id":1301,"errcode":"M_FORBIDDEN","message":"Invalid registration token.","status":401}`，data 仍帶 `flows` 與同一個 `session`，可以再試 |
| server 關閉註冊（`allow_registration = false`） | `{"code":"Forbidden","code_id":1302,"errcode":"M_FORBIDDEN","message":"M_FORBIDDEN: Registration has been disabled.","status":403}` |

## `0x21` UsernameAvailable —— 帳號名還能不能用

`GET /_matrix/client/v3/register/available`

| | |
|---|---|
| 請求 meta | `{"username":"dave"}`（query 變數） |
| 請求 data | 空 |
| 回覆 data | `{"available":true}` |

**會怎麼被拒**：已被使用 → 跟 Register 同一個 `M_USER_IN_USE`（400）—— 用不了是用 `Error` 回，不是 `{"available":false}`。

## `0x22` RegistrationTokenValidity —— 註冊碼有沒有效

`GET /_matrix/client/v1/register/m.login.registration_token/validity`

📎 **v1 路徑**：這支只有 v1（MSC3231 進規格時就是 v1）。橋在沒有 v3 時取最新的穩定版本（橋的設計 §3 批 2-C）。

| | |
|---|---|
| 請求 meta | `{"token":"…"}`（query 變數） |
| 請求 data | 空 |
| 回覆 data | `{"valid":true}` 或 `{"valid":false}` |

**會怎麼被拒**：server 沒有啟用註冊碼 → `{"code":"Forbidden","code_id":1302,"errcode":"M_FORBIDDEN","message":"M_FORBIDDEN: Server does not allow token registration","status":403}`。

## `0x23` LoginTypes —— server 支援哪些登入方式

`GET /_matrix/client/v3/login`

| | |
|---|---|
| 請求 meta | 空 |
| 請求 data | 空 |
| 回覆 data | `{"flows":[{"type":"m.login.application_service"},{"type":"m.login.token","get_login_token":true},{"type":"m.login.password"}]}` |

📎 通道上的原生 `Login` 只收 `m.login.password` 與 `m.login.token`；其他方式（SSO 等）要瀏覽器，走 HTTP。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 多了表上沒有的變數（例：把 `username` 放進 Register 的 meta —— 它是 body 的欄位） | `InvalidRequest`（1201） | 橋 |
| `id` 不是 0 | `InvalidRequest`（1201） | 橋 |
