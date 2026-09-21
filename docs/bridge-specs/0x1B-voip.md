# `0x1B Voip`：通話用的 TURN 憑證

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 1B SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。

📎 批 4 開的 kind，**沒有原生的 subtype**，所以 `0x1B` 不帶 `IS_BRIDGED`（bit4）一律是 `UnknownKind`。
📎 這個 kind 現在只有一支。通話本身（信令）是房間裡的 `m.call.*` 事件，走既有的 `Event/Send` 與推送，不在這裡。

## `0x20` TurnServer —— 打洞用的 TURN 憑證

`GET /_matrix/client/v3/voip/turnServer`

| | |
|---|---|
| 請求 meta | 空的（長度 0）或 `{}` |
| 請求 data | — |
| 回覆 data | `{"username":"1789911855:@alice:localhost","password":"…","uris":["turn:turn.example.org?transport=udp"],"ttl":86400}` |

⚠️ **這台 server 沒設 TURN 的話是 `NotFound`（404）**，不是空清單（MSC4166）。client 要把 404 讀成「這台 server 沒有 TURN，通話只能靠直連」，**不是錯誤**。
⚠️ **憑證會過期**：`ttl` 是秒數，過了要重拿。不要在連線建立時拿一次就存著用一整天。
📎 訪客帳號要 server 打開 `turn_allow_guests` 才拿得到，否則 `Forbidden`（403）。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 出現表上沒有的變數 | `InvalidRequest`（1201） | 橋 |
| `0x1B` 的 pack 沒帶 bit4，或 subtype 不在表上 | `UnknownKind`（1101） | 橋 |
| 這台 server 沒設 TURN | `NotFound`（404） | 端點 |
| 訪客帳號、而 server 不准訪客用 | `Forbidden`（403） | 端點 |
| 沒登入 | `Unauthorized`（1301） | 端點 |
