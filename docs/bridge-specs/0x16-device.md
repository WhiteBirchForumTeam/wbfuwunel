# `0x16 Device`：裝置清單、名稱、刪除

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 16 SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。
> ⚠️ 這個 kind 同時有**原生**的 pack（to-device 佇列）：`Fetch`（`0x01`）、`Batch`（`0x02`）、`ItemsDestroy`（`0x03`）、`Subscribe`（`0x04`）、`Unsubscribe`（`0x05`）、`Push`（`0x06`）、`ItemsDestroyed`（`0x07`）。那些**不帶** bit4，規格在 [wbf-to-device.md](../design/wbf-to-device.md)。
> 刪裝置（`0x23`、`0x24`，批 2）要互動式認證（UIAA），兩輪怎麼走見 index §1.5。

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

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了 `device_id`、多了表上沒有的變數 | `InvalidRequest`（1201） | 橋 |
| 沒登入 | `Unauthorized`（1301） | 端點 |
