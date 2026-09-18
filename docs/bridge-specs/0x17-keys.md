# `0x17 Keys`：端對端加密的金鑰

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 17 SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。
> 為什麼搬、搬哪些：[wbf-e2ee.md](../design/wbf-e2ee.md) §2（E2EE 的 (A)）。**發** to-device 不在這個 kind，在 `0x16` 的 `0x25`（[0x16-device.md](0x16-device.md)）。
> 這個 kind 裝的是裝置金鑰（`0x20`–`0x25`）與 server 端金鑰備份（`0x30`–`0x3D`），目前沒有原生的 pack。自己的 OTK 剩幾把、同房的人誰的裝置清單變了，之後由 `0x16` 的原生 `CryptoState` 推（wbf-e2ee.md §3），不靠這裡的端點輪詢。
> 下面的範例都是 e2e13 實跑的回覆（裝置金鑰是情境 3、備份是情境 4）。金鑰是假的（server 存金鑰時不驗簽章，驗簽章的只有 `0x25`）。

## `0x20` KeysUpload —— 上傳自己裝置的金鑰、補 OTK、換 fallback key

`POST /_matrix/client/v3/keys/upload`

| | |
|---|---|
| 請求 meta | 空 |
| 請求 data | `{"device_keys":{"user_id":"@kate:localhost","device_id":"M70FM5YkEz","algorithms":["m.olm.v1.curve25519-aes-sha2","m.megolm.v1.aes-sha2"],"keys":{"curve25519:M70FM5YkEz":"…","ed25519:M70FM5YkEz":"…"},"signatures":{"@kate:localhost":{"ed25519:M70FM5YkEz":"…"}}},"one_time_keys":{"signed_curve25519:AAAB1":{"key":"…","signatures":{…}},…},"fallback_keys":{"signed_curve25519:FALLBACK1":{"key":"…","fallback":true,"signatures":{…}}}}` —— 三個欄位都可以省略 |
| 回覆 data | `{"one_time_key_counts":{"signed_curve25519":3}}` |

- 📎 **data 送 `{}` 就是「讀回目前的 OTK 數量」**，不改任何東西。
- `device_keys` 跟 server 已經存的完全一樣時，server 不重寫（保留別人簽過的簽章）。

**會怎麼被拒**：`device_keys` 的 `user_id`／`device_id` 不是這個 session 的 → `status` 400，`errcode` `M_UNKNOWN`；JSON 形狀不對 → 400 `M_BAD_JSON`。

## `0x21` KeysQuery —— 查使用者有哪些裝置與金鑰

`POST /_matrix/client/v3/keys/query`

| | |
|---|---|
| 請求 meta | 空 |
| 請求 data | `{"device_keys":{"@kate:localhost":[]}}` —— 空陣列是「這個人的所有裝置」 |
| 回覆 data | `{"failures":{},"device_keys":{"@kate:localhost":{"M70FM5YkEz":{"algorithms":["m.olm.v1.curve25519-aes-sha2","m.megolm.v1.aes-sha2"],"device_id":"M70FM5YkEz","keys":{"curve25519:M70FM5YkEz":"…","ed25519:M70FM5YkEz":"…"},"signatures":{"@kate:localhost":{"ed25519:M70FM5YkEz":"…"}},"user_id":"@kate:localhost"}}}}`；對方上傳過交叉簽章金鑰的話，另外有 `master_keys`、`self_signing_keys`（查自己時還有 `user_signing_keys`） |

## `0x22` KeysClaim —— 拿對方裝置的一把 OTK

`POST /_matrix/client/v3/keys/claim`

| | |
|---|---|
| 請求 meta | 空 |
| 請求 data | `{"one_time_keys":{"@kate:localhost":{"M70FM5YkEz":"signed_curve25519"}}}` |
| 回覆 data | `{"failures":{},"one_time_keys":{"@kate:localhost":{"M70FM5YkEz":{"signed_curve25519:AAAB1":{"key":"…","signatures":{…}}}}}}` |

- ⚠️ **claim 走的那把就從對方的庫存消失**（e2e13 [3.3]：3 → 2，再用 HTTP claim 一次 → 1，而且拿到的是另一把）。OTK 用完之後回的是 fallback key。
- 被 claim 的那個裝置要知道自己少了一把才會補：之後由 `CryptoState` 推（wbf-e2ee.md §3.4），在那之前用 `0x20` 送 `{}` 讀回數量。

## `0x23` KeyChanges —— 兩個位置之間誰的金鑰變了

`GET /_matrix/client/v3/keys/changes`

| | |
|---|---|
| 請求 meta | `{"from":"0","to":"999999999999"}` —— 兩個都是 query 變數，**字串**形式的 server 位置（count） |
| 請求 data | 空 |
| 回覆 data | `{"changed":["@kate:localhost"],"left":[]}` |

- ⚠️ **`left` 永遠是空陣列**（上游的 `// TODO`），而且 `changed` 只看金鑰變動、**不含「有人新加入跟我同一個加密房」**。這跟上游一致，**不改**（wbf-e2ee.md §3，維護者 2026-09-17 重作 (B)：Matrix 原本的金鑰分發不動）。客製 client 判斷要不要重新查裝置，走 [wbf-room-device-version.md](../design/wbf-room-device-version.md)。
- `from`／`to` 是 server 的 count：HTTP 的 client 拿 `/sync` 的 `next_batch`。

**會怎麼被拒**：`from` 或 `to` 不是數字 → `InvalidRequest`（1201），meta `{"code":"InvalidRequest","code_id":1201,"errcode":"M_INVALID_PARAM","message":"M_INVALID_PARAM: Invalid `from`.","status":400}`（e2e13 [3.4]）。

## `0x24` SigningKeysUpload —— 上傳交叉簽章金鑰

`POST /_matrix/client/v3/keys/device_signing/upload`

| | |
|---|---|
| 請求 meta | 空 |
| 請求 data | `{"master_key":{"user_id":"@kate:localhost","usage":["master"],"keys":{"ed25519:bWFz…":"bWFz…"}},"self_signing_key":{…},"user_signing_key":{…}}`；要 UIAA 時多一個 `auth` |
| 回覆 data | `{}` |

**什麼時候要 UIAA**（兩輪怎麼走見 index §1.5）：

| 情況 | 結果 |
|---|---|
| **第一次**上傳（這個人還沒有 master key） | **不要 UIAA**，直接 `Ack`（MSC3967；e2e13 [3.5]） |
| 重送一模一樣的金鑰（例：斷線後重試） | 不要 UIAA，`Ack` |
| **換掉**既有的金鑰 | 第一輪 `Error`，`status` 401，data `{"flows":[{"stages":["m.login.password"]}],"params":{},"session":"U520STi8zjjer06UBGgwefBjmkXetoC5"}`；第二輪帶 `auth` → `Ack` |

📎 設計文件（wbf-e2ee.md §2）原本寫「要 UIAA」—— 實際只有**換掉**既有的才要，上面這張是 server 現在的行為。

## `0x25` SignaturesUpload —— 上傳簽章（驗證裝置）

`POST /_matrix/client/v3/keys/signatures/upload`

| | |
|---|---|
| 請求 meta | 空 |
| 請求 data | `{"@kate:localhost":{"<device_id 或 交叉簽章金鑰的 public key>":{…整份被簽的金鑰，signatures 裡多了自己的簽章…}}}` |
| 回覆 data | `{"failures":{}}`；有簽不進去的會列在 `failures`，例（e2e13 [3.6]，假金鑰）：`{"failures":{"@kate:localhost":{"M70FM5YkEz":{"errcode":"M_NOT_FOUND","error":"M_NOT_FOUND: Unknown device"}}}}` |

- ⚠️ **簽章會真的驗**（對 server 存的金鑰），而且**個別失敗不是整支失敗**：回覆一樣是 `Ack`（200），失敗在 data 的 `failures` 裡，client 要自己看。
- e2e 只驗到「走橋與走 HTTP 對同一份 body 回一樣的答案」，沒有造出真的 ed25519 簽章。

## `0x30`–`0x34` 金鑰備份的版本

server 端金鑰備份（[wbf-e2ee.md](../design/wbf-e2ee.md) §4 的 (C)）。一份備份有一個**版本**，金鑰都掛在版本底下。`0x32`–`0x34` 的 `version` 是 **path 變數**；`0x30`、`0x31` 沒有變數。

| subtype | 端點 | 請求 meta | 請求 data | 回覆 data |
|---|---|---|---|---|
| `0x30` CreateBackupVersion | `POST /room_keys/version` | 空 | `{"algorithm":"m.megolm_backup.v1.curve25519-aes-sha2","auth_data":{"public_key":"…","signatures":{}}}` | `{"version":"97"}` |
| `0x31` LatestBackupInfo | `GET /room_keys/version` | 空 | 空 | `{"algorithm":"m.megolm_backup.v1.curve25519-aes-sha2","auth_data":{"public_key":"…","signatures":{}},"count":0,"etag":"98","version":"97"}` |
| `0x32` GetBackupInfo | `GET /room_keys/version/{version}` | `{"version":"97"}` | 空 | 同上 |
| `0x33` UpdateBackupVersion | `PUT /room_keys/version/{version}` | `{"version":"97"}` | 同 `0x30` 的 body | `{}`；之後 `GetBackupInfo` 讀到新的 `auth_data`，`etag` 前進 |
| `0x34` DeleteBackupVersion | `DELETE /room_keys/version/{version}` | `{"version":"97"}` | 空 | `{}`；這版的金鑰跟著沒了 |

**會怎麼被拒**：版本不存在 → `NotFound`（1501），meta `{"code":"NotFound","code_id":1501,"errcode":"M_NOT_FOUND","message":"M_NOT_FOUND: Key backup does not exist at version \"97\"","status":404}`（e2e13 [4.9]）。

## `0x35`–`0x3D` 金鑰備份的金鑰

寫、讀、刪各三支：**整份**、**單一房間**、**單一 session**。

| subtype | 端點 | 請求 meta | 請求 data | 回覆 data |
|---|---|---|---|---|
| `0x35` AddBackupKeys | `PUT /room_keys/keys` | `{"version":"97"}` | `{"rooms":{"!r:localhost":{"sessions":{"session-three":{…}}}}}` | `{"etag":"102","count":3}` |
| `0x36` AddBackupKeysForRoom | `PUT /room_keys/keys/{room_id}` | `{"room_id":"!r:localhost","version":"97"}` | `{"sessions":{"session-two":{…}}}` | `{"etag":"101","count":2}` |
| `0x37` AddBackupKeysForSession | `PUT /room_keys/keys/{room_id}/{session_id}` | `{"room_id":"!r:localhost","session_id":"session-one","version":"97"}` | `{"first_message_index":0,"forwarded_count":0,"is_verified":false,"session_data":{"ciphertext":"…","ephemeral":"…","mac":"…"}}` | `{"etag":"100","count":1}` |
| `0x38` GetBackupKeys | `GET /room_keys/keys` | `{"version":"97"}` | 空 | `{"rooms":{"!r:localhost":{"sessions":{…三個…}}}}` |
| `0x39` GetBackupKeysForRoom | `GET /room_keys/keys/{room_id}` | `{"room_id":"!r:localhost","version":"97"}` | 空 | `{"sessions":{"session-one":{…},"session-two":{…}}}` |
| `0x3A` GetBackupKeysForSession | `GET /room_keys/keys/{room_id}/{session_id}` | `{"room_id":"!r:localhost","session_id":"session-one","version":"97"}` | 空 | 一筆 `KeyBackupData`，同 `0x37` 的 data |
| `0x3B` DeleteBackupKeys | `DELETE /room_keys/keys` | `{"version":"97"}` | 空 | `{"etag":"103","count":0}` |
| `0x3C` DeleteBackupKeysForRoom | `DELETE /room_keys/keys/{room_id}` | `{"room_id":"!r:localhost","version":"97"}` | 空 | `{"etag":"102","count":0}` |
| `0x3D` DeleteBackupKeysForSession | `DELETE /room_keys/keys/{room_id}/{session_id}` | `{"room_id":"!r:localhost","session_id":"session-one","version":"97"}` | 空 | `{"etag":"102","count":2}` |

- ⚠️ **`version` 在這九支是 query 變數**，在 `0x32`–`0x34` 是 path 變數。**client 兩邊都一樣寫進 meta**，橋照端點的模板自己決定放哪裡（wbf-e2ee.md §4）。
- **`count` 是這份備份現在總共有幾把金鑰**，`etag` 每次寫入前進；刪除也回這兩個。
- **`session_data` 是 client 加密過的東西**，server 不看內容。

**會怎麼被拒**：
- 金鑰不存在 → `NotFound`（1501），`M_NOT_FOUND: Backup key not found for this user's session.`（e2e13 [4.6]）。
- 📎 **版本不存在不是拒絕**：讀那一版的金鑰回 `Ack`、`{"rooms":{}}`，走 HTTP 也一樣（e2e13 [4.8]）。這是 server 現在的行為，不是橋加的。
- meta 少了 `version` → 橋自己的 `InvalidRequest`（1201），`message` 是 `` `version` is missing ``，data 是空的，端點沒有被呼叫（e2e13 [4.8]）。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了表上列的變數（`version`、`room_id`、`session_id`…）、或多了表上沒有的 | `InvalidRequest`（1201） | 橋 |
| 沒登入 | `Unauthorized`（1301），`errcode` `M_MISSING_TOKEN`（e2e13 [3.9]） | 端點 |
