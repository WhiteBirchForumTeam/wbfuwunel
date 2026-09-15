# 橋的規格：搬上通道的 Matrix 端點總表

> **這份文件回答：哪些 Matrix 端點已經（或這一批要）走橋、每個的 kind／subtype 是幾號、送出去的 pack 前幾個 byte 長什麼樣、對到哪個端點、要帶哪些變數。**
> 設計與規則在 [../design/wbf-api-bridge.md](../design/wbf-api-bridge.md)（下稱「橋的設計」），這裡只放**分配結果**。
> ⭐ **這張表是走橋的 subtype 號的唯一權威**：[wbf-wire-format.md](../design/wbf-wire-format.md) §3.2 只列原生的 subtype，走橋的一律指到這裡 —— 兩份表遲早漂移。
> 每個 kind 的完整範例（請求與回應的 meta、data、錯誤）在同一個目錄的 `KIND.md`，隨搬的那一批一起寫：[0x11-account.md](0x11-account.md)、[0x13-room.md](0x13-room.md)、[0x14-event.md](0x14-event.md)、[0x15-receipt.md](0x15-receipt.md)、[0x16-device.md](0x16-device.md)。
> 狀態：第一批（批 1）37 支**已實作**：server 的表（`src/api/client/wbf/bridge.rs` 的 `BRIDGED_ENDPOINTS`）逐列跟這份總表比對（單元測試 `the_specs_index_and_this_table_list_the_same_endpoints`，對不上就紅）；每支都在 e2e13 情境 1 跟原本的 HTTP 端點比對過結果。

## 1. 一個走橋的 pack 怎麼讀

### 1.1 請求

```
offset  bytes              意思
0       01                 version 1
1       KK                 kind（下表）
2       SS                 subtype（下表）
3       10                 flags：bit4 IS_BRIDGED（橋的設計 §2.3）。其他旗標照 wire-format §2 自由組合，例如要 Ack 就是 12
4       00 00 00 00 00 00 00 00   id：型別 0x00（無）—— 一個請求一個回應，不開會話
12      xx xx xx xx        seq：client 自己的請求號，回應抄回來，用來對表（wire-format §4 無序類）
16…     meta               JSON 物件：模板的變數（§1.3）
…       data               HTTP body 的原樣 bytes；沒有 body 的端點（GET、大部分 DELETE）data 長度 0
```

所以**前 4 個 byte 就決定了這是哪個操作**：`01 KK SS 10`。

### 1.2 回應

| 結果 | 前 4 byte | meta | data |
|---|---|---|---|
| 成功（HTTP 2xx） | `01 01 02 14`（`Control/Ack`，flags ＝ `IS_RESPONSE` ＋ `IS_BRIDGED`） | `{"status": 200, "headers": {"content-type": "application/json"}}` —— 只有表上宣告要轉的 header（橋的設計 §2.2） | Matrix 回應的 body，原樣 bytes |
| 失敗 | `01 01 03 14`（`Control/Error`，同樣帶 `IS_BRIDGED`） | `{"code_id", "code", "message", "status"}`，Matrix body 裡有的話再帶 `errcode`、`retry_after_ms`、`soft_logout`（橋的設計 §2.4；規則跟原生的錯誤共用，wire-format §3.4）。`message` 是 body 的 `error`，最多 1024 bytes，超過就在字元邊界截斷並以 `…` 結尾 —— 完整的文字在 data | Matrix 錯誤回應的 body，原樣 bytes（見 §4 待確認 1） |

`id`、`seq` 抄請求。

⚠️ **有一種拒絕比橋還早**：WebSocket 在**每個 pack 解碼之前**先問 session 還算不算數（登出、裝置被撤、token 過期、帳號被鎖，`ws.rs` 的 `revalidate`）。不算數就回通道自己的 `Error`（`01 01 03 04`：**沒有** `IS_BRIDGED`，因為那時 pack 還沒解；data 是空的）然後**關連線**。meta 一樣帶 Matrix 的欄位，例：`{"code":"Unauthorized","code_id":1301,"errcode":"M_USER_LOCKED","message":"M_USER_LOCKED: This account has been locked.","soft_logout":true,"status":401}`（向量 `error_session_locked`）。這條不分走不走橋，e2e13 [1.27] 驗過；欄位規則見 [wire-format §3.4](../design/wbf-wire-format.md)。

### 1.3 meta 的變數

- **變數名＝ ruma 路徑模板裡的名字**（`room_id`、`user_id`、`event_type`…），不另取一套（橋的設計 §2.2）。下表「變數」欄照抄。
- **path 變數**：必須是字串；server 會 percent-encode。缺了、或不是字串 → `InvalidRequest`。
  ⚠️ **空字串是合法的值**，不是「缺了」：`state_key` 最常是 `""`（`m.room.topic`、`m.room.name` 都是）。`{"state_key": ""}` 會組成 `/state/m.room.topic/`，server 本來就註冊了這個結尾有斜線的路徑。**沒給 `state_key` 才是缺**。
- **query 變數**：可以是字串、數字、布林，或它們的陣列（陣列會變成重複的參數，例如 `via`）。**沒給就整個省掉**，不送空值。
- **meta 出現下表沒列的變數 → `InvalidRequest`**，不默默忽略。
- 沒有任何變數的端點，meta 可以是空的（長度 0）或 `{}`。

### 1.4 subtype 號怎麼分

- **每個 kind 裡，`0x01`–`0x1F` 給原生的 pack，`0x20`–`0x9F` 給走橋的，`0xA0` 以上保留。** 現在所有原生的 subtype 都在 `0x1F` 以下（最大的是 `Stream/Demand` 的 `0x10`），所以同一個 kind 裡兩邊不會撞號（橋的設計 §2.3「號碼空間只有一個」）。
  📎 這個區間只是**分配慣例**，讓人一眼看出號碼是誰的；server 判斷走不走橋仍然只看 bit4 與各自的表，不看號碼大小。實作時會有一個單元測試守兩件事：橋的表每一列都落在 `0x20`–`0x9F`、兩張表的鍵沒有交集。
- 同一個 kind 裡照 Matrix 規格章節裡端點出現的順序排。**分配了就不改**（wire-format §3.3 的規矩）；端點下線就讓那個號碼空著。
- 一個 subtype 對一個 Matrix 端點（橋的設計 §2.2）。

## 2. 總表（批 1）

「前 4 bytes」是請求的 `version kind subtype flags`。method 與路徑省略 `/_matrix/client/v3` 前綴。

### `0x11 Account`（帳號）

| subtype | 前 4 bytes | 名稱 | 做什麼 | 端點 | 變數（path ／ query） | data |
|---|---|---|---|---|---|---|
| `0x20` | `01 11 20 10` | WhoAmI | 查「我是誰」：自己的 user id、device id | `GET /account/whoami` | — | — |
| `0x21` | `01 11 21 10` | GetProfile | 拿某人的整份 profile（暱稱、頭像與其他欄位） | `GET /profile/{user_id}` | `user_id` | — |
| `0x22` | `01 11 22 10` | GetProfileField | 拿 profile 的一個欄位（`displayname`、`avatar_url`…） | `GET /profile/{user_id}/{field}` | `user_id`、`field` | — |
| `0x23` | `01 11 23 10` | SetProfileField | 改自己 profile 的一個欄位 | `PUT /profile/{user_id}/{field}` | `user_id`、`field` | JSON，例 `{"displayname":"Alice"}` |
| `0x24` | `01 11 24 10` | DeleteProfileField | 刪自己 profile 的一個欄位 | `DELETE /profile/{user_id}/{field}` | `user_id`、`field` | — |
| `0x25` | `01 11 25 10` | GetAccountData | 讀 app 存在 server 上的帳號層設定 | `GET /user/{user_id}/account_data/{event_type}` | `user_id`、`event_type` | — |
| `0x26` | `01 11 26 10` | SetAccountData | 寫帳號層設定 | `PUT /user/{user_id}/account_data/{event_type}` | `user_id`、`event_type` | JSON，內容由 app 決定 |
| `0x27` | `01 11 27 10` | GetRoomAccountData | 讀某個房間的設定 | `GET /user/{user_id}/rooms/{room_id}/account_data/{event_type}` | `user_id`、`room_id`、`event_type` | — |
| `0x28` | `01 11 28 10` | SetRoomAccountData | 寫某個房間的設定 | `PUT /user/{user_id}/rooms/{room_id}/account_data/{event_type}` | `user_id`、`room_id`、`event_type` | JSON，內容由 app 決定 |
| `0x29` | `01 11 29 10` | GetTags | 某個房間的標籤（我的最愛、低優先…） | `GET /user/{user_id}/rooms/{room_id}/tags` | `user_id`、`room_id` | — |
| `0x2A` | `01 11 2A 10` | SetTag | 加或改一個標籤 | `PUT /user/{user_id}/rooms/{room_id}/tags/{tag}` | `user_id`、`room_id`、`tag` | JSON，例 `{"order":0.5}` |
| `0x2B` | `01 11 2B 10` | DeleteTag | 拿掉一個標籤 | `DELETE /user/{user_id}/rooms/{room_id}/tags/{tag}` | `user_id`、`room_id`、`tag` | — |

### `0x13 Room`（房間）

| subtype | 前 4 bytes | 名稱 | 做什麼 | 端點 | 變數（path ／ query） | data |
|---|---|---|---|---|---|---|
| `0x20` | `01 13 20 10` | CreateRoom | 開房間 | `POST /createRoom` | — | JSON：名稱、邀請誰、公開與否、加密… |
| `0x21` | `01 13 21 10` | Join | 用房間 id 或 `#別名` 加入 | `POST /join/{room_id_or_alias}` | `room_id_or_alias` ／ `via`（陣列）、`server_name`（陣列） | JSON，可為 `{}` |
| `0x22` | `01 13 22 10` | Leave | 離開房間 | `POST /rooms/{room_id}/leave` | `room_id` | JSON，可帶 `reason` |
| `0x23` | `01 13 23 10` | Forget | 把離開的房從自己的清單移除 | `POST /rooms/{room_id}/forget` | `room_id` | `{}` |
| `0x24` | `01 13 24 10` | Invite | 邀請某人 | `POST /rooms/{room_id}/invite` | `room_id` | JSON：`user_id`、`reason` |
| `0x25` | `01 13 25 10` | Kick | 踢人 | `POST /rooms/{room_id}/kick` | `room_id` | JSON：`user_id`、`reason` |
| `0x26` | `01 13 26 10` | Ban | 封鎖 | `POST /rooms/{room_id}/ban` | `room_id` | JSON：`user_id`、`reason` |
| `0x27` | `01 13 27 10` | Unban | 解除封鎖 | `POST /rooms/{room_id}/unban` | `room_id` | JSON：`user_id`、`reason` |
| `0x28` | `01 13 28 10` | JoinedRooms | 我加入了哪些房間 | `GET /joined_rooms` | — | — |
| `0x29` | `01 13 29 10` | Members | 房間成員名單 | `GET /rooms/{room_id}/members` | `room_id` ／ `at`、`membership`、`not_membership` | — |
| `0x2A` | `01 13 2A 10` | GetAlias | 用 `#別名` 查房間 id | `GET /directory/room/{room_alias}` | `room_alias` | — |
| `0x2B` | `01 13 2B 10` | SetAlias | 替房間設一個別名 | `PUT /directory/room/{room_alias}` | `room_alias` | JSON：`room_id` |
| `0x2C` | `01 13 2C 10` | DeleteAlias | 刪別名 | `DELETE /directory/room/{room_alias}` | `room_alias` | — |

📎 `POST /rooms/{room_id}/join`（只收房間 id 的那個 join）**不在這批**：`Join`（`0x21`）收 id 也收別名，一個入口就夠。

### `0x14 Event`（訊息與房間狀態）

原生的 `Recent`（`0x01`）、`Send`（`0x02`）、`Batch`（`0x03`）、`Subscribe`（`0x04`）、`Unsubscribe`（`0x05`）、`Push`（`0x06`）不變。

| subtype | 前 4 bytes | 名稱 | 做什麼 | 端點 | 變數（path ／ query） | data |
|---|---|---|---|---|---|---|
| `0x20` | `01 14 20 10` | GetEvent | 用 event id 拿單一則（例：點開回覆引用的那則） | `GET /rooms/{room_id}/event/{event_id}` | `room_id`、`event_id` | — |
| `0x21` | `01 14 21 10` | GetState | 房間目前的全部狀態（名稱、topic、成員、權限…） | `GET /rooms/{room_id}/state` | `room_id` | — |
| `0x22` | `01 14 22 10` | GetStateEvent | 房間的某一項狀態 | `GET /rooms/{room_id}/state/{event_type}/{state_key}` | `room_id`、`event_type`、`state_key`（常是 `""`） | — |
| `0x23` | `01 14 23 10` | SetStateEvent | 改房間的某一項狀態（改名、改 topic、改權限…） | `PUT /rooms/{room_id}/state/{event_type}/{state_key}` | `room_id`、`event_type`、`state_key`（常是 `""`） | JSON，該狀態的 content，例 `{"topic":"大家好"}` |
| `0x24` | `01 14 24 10` | Redact | 收回一則訊息 | `PUT /rooms/{room_id}/redact/{event_id}/{txn_id}` | `room_id`、`event_id`、`txn_id` | JSON，可帶 `reason` |
| `0x25` | `01 14 25 10` | Context | 跳到某則訊息，連同它前後幾則 | `GET /rooms/{room_id}/context/{event_id}` | `room_id`、`event_id` ／ `limit`、`filter`（字串形式的 JSON） | — |

### `0x15 Receipt`（輸入中與已讀）

| subtype | 前 4 bytes | 名稱 | 做什麼 | 端點 | 變數（path ／ query） | data |
|---|---|---|---|---|---|---|
| `0x20` | `01 15 20 10` | Typing | 「正在輸入…」開或關 | `PUT /rooms/{room_id}/typing/{user_id}` | `room_id`、`user_id` | JSON：`typing`、`timeout` |
| `0x21` | `01 15 21 10` | ReadMarkers | 設「讀到哪裡」的標記 | `POST /rooms/{room_id}/read_markers` | `room_id` | JSON：`m.fully_read`、`m.read`… |
| `0x22` | `01 15 22 10` | Receipt | 送一個已讀回條 | `POST /rooms/{room_id}/receipt/{receipt_type}/{event_id}` | `room_id`、`receipt_type`、`event_id` | JSON，可為 `{}` |

### `0x16 Device`（裝置）

原生的 `Fetch`（`0x01`）、`Batch`（`0x02`）、`ItemsDestroy`（`0x03`）、`Subscribe`（`0x04`）、`Unsubscribe`（`0x05`）、`Push`（`0x06`）、`ItemsDestroyed`（`0x07`）不變。

| subtype | 前 4 bytes | 名稱 | 做什麼 | 端點 | 變數（path ／ query） | data |
|---|---|---|---|---|---|---|
| `0x20` | `01 16 20 10` | ListDevices | 登入過的裝置清單 | `GET /devices` | — | — |
| `0x21` | `01 16 21 10` | GetDevice | 單一裝置的資訊 | `GET /devices/{device_id}` | `device_id` | — |
| `0x22` | `01 16 22 10` | UpdateDevice | 改裝置名稱 | `PUT /devices/{device_id}` | `device_id` | JSON：`display_name` |

**批 1 共 37 支**：Account 12、Room 13、Event 6、Receipt 3、Device 3。

## 3. 不在這批

| 什麼 | 在哪 |
|---|---|
| 註冊（會改變連線身份） | 批 2，手寫的 `Session/Register`，原生 pack，不走橋 |
| 停用帳號、改密碼、刪裝置（要 UIAA） | 批 3 |
| 送訊息、翻歷史、訂閱推送、to-device | 已經是原生 pack |
| `/sync`、SSO 等瀏覽器登入、舊的整檔媒體 | 不搬（橋的設計 §3） |

## 4. 寫程式時要確認、目前是提案的

1. **錯誤的 data 放 Matrix 錯誤回應的 body 原樣**：橋的設計只定了 `errcode` 進 meta。把整個 body 也放進 data，client 就拿得到 Matrix 錯誤裡的其他欄位 —— 最重要的是 UIAA 的 401 帶的 `flows`／`session`／`completed`，**批 3 因此不需要另外定格式**。
2. **`id` 填 0（型別 `0x00`）**：走橋的都是一問一答，用 `seq` 對表就夠，不開會話。
3. **成功回應 meta 裡要轉哪些 header**：第一版只轉 `content-type`。
