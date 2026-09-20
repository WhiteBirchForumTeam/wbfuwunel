# `0x1C Misc`：其餘

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 1C SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。

📎 這個 kind 收的是分不進其他領域的那幾支（分配表 [wbf-wire-format.md](../design/wbf-wire-format.md) §3.3）。
它**沒有原生的 subtype**，所以 `0x1C` 不帶 `IS_BRIDGED`（bit4）一律是 `UnknownKind`。

## `0x20` Capabilities —— 這台 server 允許什麼

`GET /_matrix/client/v3/capabilities`

| | |
|---|---|
| 請求 meta | 空的（長度 0）或 `{}` |
| 請求 data | — |
| 回覆 data | `{"capabilities":{"m.room_versions":{"default":"11","available":{"11":"stable"}},"m.change_password":{"enabled":true},…}}` |

📎 **值是跟著設定走的**，不是固定的：`m.change_password` 看 `login_with_password`、`m.3pid_changes` 看寄信子系統開了沒、`m.get_login_token` 看 `login_via_existing_session`。所以 client 要**問過才知道**，不要寫死。
📎 回覆裡也會有 `org.matrix.msc*` 開頭的實驗鍵 —— 不認得的鍵一律忽略，不要因為多了鍵就當成壞掉。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 出現表上沒有的變數 | `InvalidRequest`（1201） | 橋 |
| `0x1C` 的 pack 沒帶 bit4，或 subtype 不在表上 | `UnknownKind`（1202） | 橋 |
| 沒登入 | `Unauthorized`（1301） | 端點 |
| 問太快 | `RateLimited`（429 `M_LIMIT_EXCEEDED`） | 端點 |
