# `0x16 Device`：裝置清單、名稱、刪除、發 to-device

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 16 SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。
> ⚠️ 這個 kind 同時有**原生**的 pack（to-device 佇列）：`Fetch`（`0x01`）、`Batch`（`0x02`）、`ItemsDestroy`（`0x03`）、`Subscribe`（`0x04`）、`Unsubscribe`（`0x05`）、`Push`（`0x06`）、`ItemsDestroyed`（`0x07`）。那些**不帶** bit4，規格在 [wbf-to-device.md](../design/wbf-to-device.md)。
> 刪裝置（`0x23`、`0x24`，批 2）要互動式認證（UIAA），兩輪怎麼走見 index §1.5。
> **發** to-device（`0x25`）是 E2EE 的 (A)（[wbf-e2ee.md](../design/wbf-e2ee.md) §2）；**收**是上面那組原生的 `Subscribe`／`Push`。

## `0x20` ListDevices —— 登入過的裝置清單

`GET /_matrix/client/v3/devices`

| | |
|---|---|
| 請求 meta | 空或 `{}` |
| 請求 data | 空 |
| 回覆 data | `{"devices":[{"device_id":"RJYKSTBOIE","display_name":"手機","last_seen_ip":"203.0.113.7","last_seen_ts":1757830000000},…]}` |

📎 `last_seen_ip` 是 server 看到的來源位址。走通道時，那是**這條 WebSocket 連線的對端位址**（橋把它帶給端點），不是 client 自己說的。

## `0x21` GetDevice —— 單一裝置的資訊

`GET /_matrix/client/v3/devices/{device_id}`

| | |
|---|---|
| 請求 meta | `{"device_id":"RJYKSTBOIE"}` |
| 請求 data | 空 |
| 回覆 data | `{"device_id":"RJYKSTBOIE","display_name":"手機","last_seen_ip":"203.0.113.7","last_seen_ts":1757830000000}` |

**會怎麼被拒**：不是自己的裝置、或不存在 → `NotFound`（404）。

## `0x22` UpdateDevice —— 改裝置名稱

`PUT /_matrix/client/v3/devices/{device_id}`

| | |
|---|---|
| 請求 meta | `{"device_id":"RJYKSTBOIE"}` |
| 請求 data | `{"display_name":"舊手機"}` |
| 回覆 data | `{}` |

**會怎麼被拒**：不是自己的裝置 → `NotFound`（404）。

## `0x23` DeleteDevice —— 刪一個裝置（批 2，要 UIAA）

`DELETE /_matrix/client/v3/devices/{device_id}`

| | |
|---|---|
| 請求 meta | `{"device_id":"Ar6fUHHCvy"}` |
| 請求 data | 第一輪 `{}`；第二輪 `{"auth":{"type":"m.login.password","session":"…","identifier":{"type":"m.id.user","user":"erin"},"password":"…"}}` |
| 第一輪回覆 | `Error`，`status` 401；data `{"flows":[{"stages":["m.login.password"]}],"params":{},"session":"lhcDfLGqNIz9OWTHt8X1SiuUce88tpVU"}` |
| 成功回覆 data | `{}` |

**會怎麼被拒**：密碼錯 → 還是 401，meta `{"code":"Unauthorized","code_id":1301,"errcode":"M_FORBIDDEN","message":"Invalid username or password.","status":401}`，data `{"flows":[{"stages":["m.login.password"]}],"params":{},"session":"lhcDfLGqNIz9OWTHt8X1SiuUce88tpVU","errcode":"M_FORBIDDEN","error":"Invalid username or password."}` —— `session` 不變，可以再試。

⚠️ **刪掉這條連線自己的裝置**：回覆照樣先到，**下一個 pack** 在橋之前被拒 —— `{"code":"Unauthorized","code_id":1301,"errcode":"M_UNKNOWN_TOKEN",…}`、沒有 bit4、關連線（index §1.2；e2e13 [2.13]）。

## `0x24` DeleteDevices —— 一次刪好幾個裝置（批 2，要 UIAA）

`POST /_matrix/client/v3/delete_devices`

| | |
|---|---|
| 請求 meta | 空 |
| 請求 data | 第一輪 `{"devices":["BcxRdLggcD","3LqNFK6Oed"]}`；第二輪加 `auth`，同上 |
| 第一輪回覆 | `Error`，`status` 401，data 帶 `flows` 與 `session` |
| 成功回覆 data | `{}`；被刪的裝置的 token 立刻失效（e2e13 [2.11]） |

## `0x25` SendToDevice —— 發 to-device（E2EE）

`PUT /_matrix/client/v3/sendToDevice/{event_type}/{txn_id}`

| | |
|---|---|
| 請求 meta | `{"event_type":"m.room_key.e2e13","txn_id":"bridge-txn-1"}` |
| 請求 data | `{"messages":{"@leo:localhost":{"<device_id 或 *>":{…事件的 content…}}}}` —— 加密房的金鑰通常是 `m.room.encrypted`，content 是 Olm 密文 |
| 回覆 data | `{}` |

- **對方怎麼收到**：寫進對方裝置的 to-device 佇列，那個裝置的持有連線（`Device/Subscribe` 的那條）立刻收到原生的 `Push`。e2e13 [3.7] 實跑：`Push` 的 meta `{"bc":1,"counts":[88],"gap":false,"nt":88,"ot":88}`，data 裡那一則是 `{"content":{"algorithm":"m.megolm.v1.aes-sha2","body":"via bridge"},"sender":"@kate:localhost","type":"m.room_key.e2e13"}`。對方沒連線就留在佇列，下次 `Subscribe`／`Fetch` 補（[wbf-to-device.md](../design/wbf-to-device.md)）。
- ⚠️ **`txn_id` 是冪等鍵**：同一個裝置用同一個 `txn_id` 再送一次，回 `Ack` `{}`、**什麼都不送**（e2e13 [3.8]）。所以重試要用同一個 `txn_id`，新的一則要換一個。
- 走橋與走 HTTP 送進的是同一個佇列（e2e13 [3.8]）。

**會怎麼被拒**：沒登入 → `Unauthorized`（1301），`errcode` `M_MISSING_TOKEN`（e2e13 [3.9]）；meta 少了 `event_type` 或 `txn_id` → `InvalidRequest`（橋）。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了 `device_id`、多了表上沒有的變數 | `InvalidRequest`（1201） | 橋 |
| 沒登入 | `Unauthorized`（1301） | 端點 |
