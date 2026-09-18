# 提案：裝置版本號、房間版本號，與送出時比對

> **這份文件回答：server 要新增哪幾樣東西，才能讓客製 client「只在要發的時候、只對那個房間」確認房間金鑰該發給誰，並且在它看到的名單過期時被擋下來、而不是靜默送出別人解不開的訊息。**
> 狀態：✅ 維護者同意（PR #72 合併）。F1＋F2＋F4 在 PR（分支 `wbf/room-device-version`）實作，F3 是下一支；實作時跟這份不一樣的地方寫在各節的 📌。討論的全過程（走過哪些路、為什麼不走）在 [wbf-device-index-notes.md](wbf-device-index-notes.md)；它回答的問題與達標條件在 [e2ee-send-guard-problem.md](e2ee-send-guard-problem.md)。
> 前提：[wbf-e2ee.md](wbf-e2ee.md) 的 (A) 已合併；(B) `CryptoState`（PR #68）與這份不衝突，§9 講兩者的關係（已定案：(B) 只帶自己的金鑰存量）。
> 🚫 **HTTP 與一般 Matrix client 的行為不變**：這裡加的東西全是擴展，沒有約定的 client 不會被檢查、不會收到陌生的 pack。

## 0. 一句話

每個帳號有一個**裝置版本號**（`序號-雜湊`），成員清單把它帶給 client；房間有一個**房間版本號**（全域位置，由成員事件與成員的裝置版本推出來）；有約定的 client 送加密訊息時帶上房間版本號，對不上就回 `1506 RoomDevicesChanged`。裝置一變，就把新版本廣播給這個帳號所在房間的線上訂閱者。

## 1. 為什麼是這個形狀（摘要，詳見討論紀錄）

- **只有「發」需要知道別人有哪些裝置**；「收」不需要。所以不必持續追蹤，只在**點進房間**與**送出前**、只對**那個房間**確認。
- **成員變動已經是房間事件**：已被記錄、推送、有補窗，不必加任何東西。
- **server 唯一非給不可的是「某人的裝置變了」**，而且只需要「每人一個版本號」，不需要歷史、不需要逐房扇出、不需要成對表。
- 成員、交集、`left`、這一輪房間金鑰發給過誰、有人退房時換房間金鑰 —— 都在 client 本地（`matrix-sdk-crypto` 內部已經有這兩張表，成員名單本來就是 client 傳進去的）。
- 客製 client 發房間金鑰用 **`IdentityBasedStrategy`**（MSC4153 推薦：只發給被擁有者自己簽過的裝置）。

## 2. 四個功能

| # | 功能 | 一句話 |
|---|---|---|
| **F1** | 每個帳號的裝置版本號 | `序號-雜湊`，存資料庫，每次裝置集合變動 +1 |
| **F2** | 成員清單帶版本號 | 現成的 `GET /rooms/{room_id}/members` 在每個已加入成員身上帶他的版本號，最外層帶房間版本號 |
| **F3** | 裝置變動廣播到房間（擴展） | 短暫的房間訊號，記憶體扇出給在 `Hello.features` 宣告過的線上訂閱者 |
| **F4** | 送出時比對房間版本號 | `Event/Send` 帶房間版本號，對不上回 `1506 RoomDevicesChanged` |

## 3. F1：每個帳號的裝置版本號

### 3.1 形狀

```
3-abcdef1234
│ └────────── 雜湊：這個人「任何人都看得到的」金鑰集合的指紋（10 個十六進位字元）
└──────────── 序號：這個人的裝置集合變動了幾次（從 1 起）
```

資料庫一人一列（新 map `userid_wbfdeviceversion`。📌 不叫 `userid_deviceversion`：上游已經有一張 `userid_devicelistversion`，是聯邦的裝置清單流水號，跟這個無關，名字太像會被混用）：

| 欄位 | 型別 | 意思 |
|---|---|---|
| `seq` | u64 | 序號 |
| `hash` | 10 個十六進位字元 | 雜湊 |
| `pos` | u64 | 最後一次變動的**全域位置**（跟 `g_seq` 同一個計數器），給房間版本號用（§4） |

### 3.2 什麼時候變

**只有一個入口：`users::mark_device_key_update`**。現有會經過它的路徑正好就是「要讓版本號變」的全部：

| 路徑 | 位置 |
|---|---|
| 上傳 device keys | `users/keys.rs` `add_device_keys` |
| 上傳交叉簽章金鑰 | `users/keys.rs` `add_cross_signing_keys` |
| 上傳簽章（自簽，或簽別人） | `users/keys.rs` `sign_key` |
| 刪裝置 | `users/device.rs` |
| 聯邦來的 `m.device_list_update` | `api/server/send.rs` |

每次：`seq += 1`、重算 `hash`、`pos = 這次變動取得的全域位置`。

- 換金鑰、換交叉簽章金鑰、簽章都算（維護者 2026-09-17）。所以**序號數的是「裝置集合的變動次數」**，不只是換了幾台裝置。
- **同一個人的兩次變動同時發生**：用一把以帳號為鍵的鎖（`MutexMap`）把「讀舊值 → 寫新值」包起來，序號不會少加。
- 📌 **版本號一定前進**：金鑰讀不出來或算不出雜湊時，序號照樣 +1，雜湊寫明確的佔位字 `unhashable`（不是空字串）。一個沒動的版本號會讓看過舊金鑰的 client 比對通過；前進了只是讓它多查一次。舊值讀不出來（不是「沒有」，是壞掉）時，序號用這次的 `pos`：`pos` 每次任何帳號變動都前進，一定大於這個帳號用過的任何序號。
  - 🚨 **「沒有這把金鑰」只認 `NotFound`**（PR #73 審查）：主金鑰、自簽金鑰、某台裝置的金鑰讀取失敗（不是不存在），整個雜湊就算失敗 → `unhashable`。不能把讀不到的當成「沒有」然後對讀得到的那幾把算雜湊 —— 那看起來像這個帳號的指紋，其實是另一組金鑰的。
  - 沒有那一列時的當場補算（§3.3）也一樣：算不出雜湊就寫 `unhashable`，不回 500（否則這個人所在的每個房間 `/members` 與送出比對都會一直 500，直到他下次換金鑰）。已存的那一列**讀取失敗**則是錯誤、不重算：重算會從序號 1 起、蓋掉原本的，序號可能退回別人看過的值。

### 3.3 沒有那一列時（既有帳號、剛升級）

**讀的時候當場算**：`seq = 1`，`hash` 照 §3.4 現算，`pos` 取這個人**既有的金鑰變動紀錄的最後一列**（`keychangeid_userid` 以 `(使用者, 位置)` 為鍵，反向找最後一筆）；沒有任何紀錄就是 `0`。算完寫回。

- 📎 `pos` 不用「現在」：用現在會讓他所在的每個房間版本號平白跳一次，client 多被擋一輪。用既有紀錄的最後位置，值就是「他上次真的變動的時候」。
- 不需要 migration。

### 3.4 雜湊算什麼

**只放任何人查詢時都看得到的東西**，否則別的 client 算不出同一個值：

| 放進去 | 不放 | 為什麼 |
|---|---|---|
| 主金鑰（master key） | 使用者簽章金鑰（user-signing key） | 只回給本人 |
| 自簽金鑰（self-signing key） | **別人**簽的簽章（例：Alice 驗證 Bob 時留在 Bob 主金鑰上的那筆） | 只回給簽的人自己；Carol 查 Bob 時看不到 |
| 每台裝置的 device keys，依裝置 ID 排序 | device keys 的 `unsigned`（顯示名稱等） | 不屬於金鑰，也沒被簽 |

每一項只保留 `signatures` 裡**擁有者自己**（`user_id` 那一層）的簽章。

**演算法**：每一項轉成 Matrix 的規範化 JSON（canonical JSON），依「主金鑰、自簽金鑰、各裝置（依裝置 ID）」的順序，每項前面加 4 byte 大端長度，串起來取 SHA-256，**十六進位小寫的前 10 個字元**。

- 📌 **沒有主金鑰或自簽金鑰時，那一項是空的（長度 `00 00 00 00`、沒有內容），不是跳過**：跳過的話「沒有主金鑰、一台裝置」跟「有主金鑰、沒有裝置」會排成同一串。裝置只算上傳過金鑰的（跟 `/keys/query` 回的一樣）。
- 📌 **黃金向量**（client 拿來驗自己的實作；`src/service/device_versions/keys_hash.rs` 的 `the_documented_vector`，另外在 Rust 之外照這段文字算過一次）：`@bob:localhost`，主金鑰 `{"keys":{"ed25519:MASTER":"bWFzdGVy"},"signatures":{"@bob:localhost":{"ed25519:DEV1":"c2VsZg"}},"usage":["master"],"user_id":"@bob:localhost"}`、自簽金鑰 `{"keys":{"ed25519:SSK":"c3Nr"},"signatures":{"@bob:localhost":{"ed25519:MASTER":"bXNr"}},"usage":["self_signing"],"user_id":"@bob:localhost"}`、兩台裝置 `DEV1`（金鑰 `b25l`）與 `DEV2`（金鑰 `dHdv`），各是 `{"algorithms":["m.olm.v1.curve25519-aes-sha2","m.megolm.v1.aes-sha2"],"device_id":"DEV1","keys":{"curve25519:DEV1":"b25l","ed25519:DEV1":"b25l"},"signatures":{"@bob:localhost":{"ed25519:DEV1":"ZGV2"}},"user_id":"@bob:localhost"}` 這個形狀 → **`810b7c3be4`**。
- 📌 **應用服務（appservice）代答的金鑰不在裡面**，遠端帳號的金鑰也不是從這台 server 讀的：這個 fork 預設不開聯邦，開之前要重新檢查。

- 雜湊只用來核對；「變沒變」靠序號，所以 40 bit 碰撞不影響正確性。
- 別人的簽章不進雜湊，但序號照樣 +1（它經過 `mark_device_key_update`）。

## 4. 房間版本號

### 4.1 公式

> **房間版本號 ＝ max(房間目前狀態裡 `membership` 是 `join`、`leave` 或 `ban` 的成員事件的位置, 目前已加入成員各自的 F1 `pos`)**

- 型別：u64，跟 `g_seq` 同一個計數器。
- **房間版本號只為了分發房間金鑰**，所以只看「會改變誰拿金鑰」的事（維護者 2026-09-17）：
  - **`invite`、`knock` 不算**：沒有權限看訊息的人不必管；他真的加入時，那則 `join` 自然讓版本號前進。
  - **`join` 算**：多一個人要拿金鑰。
  - **`leave`、`ban`（含被踢，踢出就是別人發的 `leave`）算**：少一個人。這一條也是**只增不減**的保證：離開的人的 F1 `pos` 不再算進去，但他那則離開事件一定比他的 `pos` 新，最大值不會掉。
  - 📎 被邀請後拒絕（`invite` → `leave`）的那則 `leave` 也會算進去，版本號多跳一次、線上送出者多被擋一次。要排除它得去讀事件的上一個狀態（`prev_content`），規則會變複雜；多跳一次是安全側，**不為它加條件**。
- **「已加入成員」才算 `pos`**：被邀請但還沒加入的人不算（維護者 2026-09-17）。

### 4.2 🚨 必須只增不減

成員事件的位置要從**房間目前的狀態**讀（`state_accessor::room_state_full` 裡的 `m.room.member`），**不能**從 `state_cache` 的成員計數索引讀。

**理由**：`forget` 會刪掉那份索引裡「離開」的那一列，最大值會掉回去，形成一個洩漏：

1. Weil 離開，房間版本號從 V1 升到 V2。
2. Weil forget，索引那一列被刪，最大值掉回 V1。
3. 一個還停在 V1、不知道 Weil 已經離開的 client 送出，**比對通過**。
4. 它的名單裡還有 Weil，**把新的房間金鑰發給了已經離開的人**。

離開事件會一直留在房間狀態裡，不受 forget 影響。

### 4.3 快取

- **放記憶體，不存資料庫**：`房間 → (算它時的房間狀態 shortstatehash, 版本號)`。它隨時可以重算，所以重啟、失效都不怕，沒有 migration、不會漂移。
- 📌 **成員那一半不掛失效點，改成比房間狀態**（實作時改的，原本寫「成員事件寫入時失效」）：讀的時候先拿房間目前的 `shortstatehash`，跟快取記的不同就重算。任何成員事件都會換掉房間狀態，所以不必在 `update_membership` 裡找對的時間點失效 —— 失效點掛得比狀態寫入早，就會把還沒換的舊狀態算進快取。
- **F1 那一半**：某人的 F1 版本號變了 → 刪掉他加入的每個房間的快取。
  - ⚠️ 光刪不夠：一個正在算的房間可能已經讀到他的舊 `pos`，算完才寫回。所以再加一個「裝置變動次數」計數器：算之前記下、寫回之前再看一次，**中間動過就不寫回**（這次的結果照樣回，只是不留）。順序是「寫 F1 → 計數器 +1 → 刪快取」。
  - 這一步之後也是 F3 扇出的起點，**不多一條路**。
- **沒命中就當場算**：讀一次房間狀態的成員事件，加上已加入成員的 F1 `pos`。成本 O(成員數)。

### 4.4 剩下的競態（寫明，接受）

- **成員變動**跟送出在同一把房間鎖底下（`send_message_event` 持有 `state.mutex`；本地的成員事件也是在同一把鎖下 `build_and_append_pdu`）⏳ 實作時要確認聯邦進來的成員事件也經過這把鎖，所以 F4 在鎖內比對時，成員這一半是精準的。
- **換裝置**（`mark_device_key_update`）**不在房間鎖底下**：「Bob 在比對完之後幾毫秒加了裝置」這個窗口關不掉。它比 Matrix 原本「查完金鑰到送出」的窗口小很多，F3 也會在幾毫秒後通知；那一則 Bob 的新裝置解不開，之後的會補上。
- **聯邦的狀態重設**理論上可能讓成員事件換成較舊的一則；這個 fork 預設不開聯邦，開聯邦之前要重新檢查這條。

## 5. F2：成員清單帶版本號

沿用現成的成員 API（維護者 2026-09-17）：`GET /_matrix/client/v3/rooms/{room_id}/members`，橋的 `0x13 0x29 Members`。

### 5.1 回應多出的兩個欄位

```json
{
  "chunk": [
    { "type": "m.room.member", "state_key": "@bob:example.org", "content": { "membership": "join" },
      "unsigned": { "org.wbftw.device_version": "3-abcdef1234" }, "…": "…" },
    { "type": "m.room.member", "state_key": "@weil:example.org", "content": { "membership": "leave" }, "…": "…" }
  ],
  "org.wbftw.room_version": 81234
}
```

- **一律帶，不另設開關**（維護者：官方 client 多拿一個值也無所謂）。Matrix client 忽略不認得的欄位。
- `org.wbftw.device_version` **只加在 `membership` 是 `join` 的事件上**。
- `org.wbftw.room_version` 在最外層，照 §4.1 算。**它跟 `chunk` 用同一次讀到的房間狀態算**，不會出現「清單是這一刻、版本號是下一刻」的錯位。
- **不改 Matrix 的預設**（維護者 2026-09-17）：不帶參數時照規格回所有成員事件。客製 client 帶 `membership=join`，或自己只看 `join`。
- **清單變了就刷新，是約定**，client 不必區分為什麼變。

### 5.2 實作要動的地方

- `api/client/membership/members.rs`：ruma 的 `get_member_events::v3::Response` 只有 `chunk`，**沒有地方放最外層的欄位**。要改成回自己組的 JSON（仍是 200、仍是 `application/json`）。⚠️ 這是動到上游檔案的地方，跟上游 merge 時是衝突點。
  - 📌 所以這條路由**不走 `ruma_route`**（它要求回 ruma 的型別），在 `router.rs` 用兩行 `.route(...)` 手動掛 r0 與 v3 兩條路徑（`MEMBER_EVENTS_PATHS`）。有一條單元測試比對這兩條跟 ruma 列的路徑一樣，ruma 哪天加了一條，測試會紅。
  - 每則事件照舊轉成 ruma 原本的格式（`Raw<StateEvent<RoomMemberEventContent>>`），只多加那一個 `unsigned` 欄位。
- 橋那邊不用動程式：它回的就是 HTTP 的 body 原樣。

### 5.3 順手清掉 `at`

上游忽略 `at`（程式註解 `TODO: at a specific point in time`），帶了沒作用，但 [0x13-room.md](../bridge-specs/0x13-room.md) 寫成「取某個時間點的名單」好像能用。

- 從橋的表拿掉（`bridge.rs` 的 `Members` 那一列只剩 `membership`、`not_membership`），通道上帶 `at` 會被橋回 `InvalidRequest`，不再默默忽略。
- 同步改 `bridge-specs/index.md`、`0x13-room.md`。
- HTTP 那條是 ruma 的型別，不動。

## 6. F3：裝置變動廣播到房間（擴展）

### 6.1 形狀

維護者提的「把裝置變動廣播到這個帳號所在的每個房間」：對 client 來說它就像一種成員變動，走同一套處理 —— 這個人的裝置過期了，下次發之前重查。

- **短暫的訊號，不寫時間線**：只在記憶體裡扇出給那些房間**正連著**的訂閱者。不寫資料庫、不進歷史、不走聯邦、不需要任何人的權限。
- **新的原生 subtype：`0x14 Event` 的 `0x07 DeviceChanged`**，server → client（維護者 2026-09-17 同意）。
  - ⚠️ **不能塞進 `Push` 的 data**：那裡每一則都帶 `g_seq`、推進 client 的水位，混進沒有 `g_seq` 的東西會弄壞水位。
- **`id` 抄這條連線的 `Event/Subscribe`、跟 `Push` 共用 `seq` 與 `gap`**（跟 `CryptoState` 共用 `Device/Subscribe` 的做法一樣）。
- meta：

```json
{
  "user_id": "@bob:example.org",
  "device_version": "4-0123456789",
  "rooms": { "!r1:example.org": 81240, "!r2:example.org": 81240 },
  "gap": false
}
```

  `rooms` 是「這條連線訂閱中、而且 Bob 在裡面的房間 → 各自新的房間版本號」。**一條連線一次變動只收一個 pack**，不管 Bob 跟它有幾個共同房間。

- **client 怎麼用**：`device_version` 跟快取的不同 → 標記 Bob 要重查；各房間的房間版本號直接更新，下次送出就帶新的。
- **推送丟了**：`gap` 提醒 client 重拿那些房間的 F2。最後一道是 F4：最壞被擋一次。

### 6.2 🚨 看不懂的 client 不受影響（維護者 2026-09-17 指定）

兩層保證：

1. **只推給宣告過的連線**：client 在 `Control/Hello` 的 `features` 裡帶 `"org.wbftw.device_versions"`，server 記在這條連線上，才推 `DeviceChanged`。**沒宣告的連線永遠收不到**，舊 client 看不到任何陌生的 pack。
   - 📎 這是 server **第一次讀 client 的 `features`**。[wire-format §3.2](wbf-wire-format.md) 那一格目前寫「client 的 `features` server 目前不讀」，要一起改。
   - server 的 `Hello` 回應 `features` 加上 `"org.wbftw.device_versions"`，client 據此知道這台 server 支援。
2. **名字帶命名空間**：`org.wbftw.` 字首，不佔 Matrix 的 `m.`。

### 6.3 扇出的成本

某人的 F1 版本號變了 → 他加入的房間（`rooms_joined`）→ 每個房間正連著、且宣告過的訂閱者（通道的房間登記表，已經在記憶體）→ 以連線合併 → 每條連線一個 pack。**零寫入**，只在真的有變動時發生。

## 7. F4：送出時比對房間版本號

### 7.1 規則

`Event/Send`（`0x14 0x02`）的 meta 多一個可選欄位 `room_version`（u64）：

| 連線宣告過 `org.wbftw.device_versions`？ | meta 帶 `room_version`？ | 結果 |
|---|---|---|
| 否 | 否 | **不檢查**（沒有約定） |
| 否 | 是 | 檢查（這一則自己選擇了約定） |
| 是 | 是 | 檢查 |
| **是** | **否，而且事件是加密的** | **`InvalidRequest`**：有約定卻漏帶，不能靜默繞過 |

- 「加密的」＝事件類型是 `m.room.encrypted`。明文事件不檢查：📌 帶了 `room_version` 也不檢查（明文不發房間金鑰，擋它沒有意義），上表四列都只講加密的。
- 📌 **比對在「重複的 `txn_id`」檢查之後**：同一個 `txn_id` 已經送出過，就回原本那則的 `event_id`，不管帶的號碼是不是舊的 —— 重試不能因為號碼在中間變了而被擋。
- 📌 宣告記在**連線**上，下一個 `Hello` 覆蓋（沒帶就收回）；HTTP 沒有連線（`connection_id` 是 0），永遠不算宣告過。
- 檢查放在 `send_message_event` **持有房間鎖之後、寫入之前**（`api/client/send.rs`；函式多一個「預期的房間版本號」參數，HTTP 路由傳 `None`）。
- **對不上** → 不寫入、不扇出，回：

| `code_id` | `code` | 額外欄位 |
|---|---|---|
| **1506** | **`RoomDevicesChanged`** | `room_version`：目前的號碼 |

  1500 狀態家族；號碼與名字維護者 2026-09-17 同意。實作時加進 [wire-format §3.4](wbf-wire-format.md) 的錯誤表。

### 7.2 client 收到 1506

拉最新的 F2 → 比出版本號變了的人與成員進出 → 只重查那幾個人的金鑰 → 補發房間金鑰（有人離開就換一把）→ 帶新號碼重送。**要不要重試、重試幾次是 client 的政策，server 不規定**（維護者：client 重試 1 次就夠，而且不關 server 的事）。

### 7.3 HTTP

HTTP 送訊息不帶這個欄位，所以**不檢查**。這條路的正確性維持跟上游一樣的水準（推播鏈＋事後要金鑰、金鑰備份）。

## 8. 分幾支 PR：看改動量

維護者 2026-09-17 的規則：**改動小就一次做完；大一點分兩支；很大才分三支以上。**

### 8.1 估計

| 塊 | 要動的 | 估計（含單元測試，不含 e2e 與文件） |
|---|---|---|
| F1 | 新 map、版本號的讀寫與當場補算、雜湊（規範化 JSON）、`MutexMap`、掛進 `mark_device_key_update` | 約 250–350 行 |
| F2 | `members.rs` 改自組 JSON、房間版本號的計算與記憶體快取＋兩個失效點、橋的表拿掉 `at` | 約 200–300 行 |
| F4 | `Event/Send` 的 `room_version`、`send_message_event` 多一個參數並在鎖內比對、連線記住 `Hello.features`、`RejectCode` 加 `1506`、向量 | 約 200–300 行 |
| F3 | 新 subtype、依房間登記表以連線合併扇出、只推宣告過的連線、跟 `Push` 共用 `seq`／`gap` | 約 200–300 行 |
| e2e | §10 的 12 條（新腳本一支） | 約 400–500 行 |

合計約 1300–1800 行，屬於「大一點」。

### 8.2 建議：分兩支

| 順序 | 內容 | 為什麼這樣切 |
|---|---|---|
| **1** | **F1 ＋ F2 ＋ F4**（＋清掉 `at`、`1506`、`Hello.features` 開始被讀） | 三塊綁在一起才有用：F4 比的就是 F1／F2 算出來的房間版本號，拆開的中間狀態沒有任何可驗的行為。**這一支交出來，正確性就到位了**：沒有即時通知，最壞也只是被擋一次 |
| **2** | **F3** `DeviceChanged` | 加速，跟前三塊只接在「房間版本號快取失效」一個點上，可以獨立審 |

📎 實作時若第一支實際比估計小很多，就把 F3 併進去一次做完，PR 描述裡寫明實際行數。

## 9. 跟 (B) `CryptoState` 的關係（✅ 已定案，見 9.3）

(B) 還沒合併時寫的，9.1、9.2 保留當時的選項；維護者的決定在 9.3。

### 9.1 (B) 現在送的東西，逐項對照

| (B) 的東西 | 做什麼 | 這份提案之後，客製 client 還需要嗎 |
|---|---|---|
| `CryptoState.otk_counts`、`unused_fallback_key_types` | 我自己的一次性金鑰還剩幾把、fallback key 用掉沒 | **需要**。別人要跟我建通道時用的是我的金鑰，這份提案不處理 |
| 上傳／被 claim 一次性金鑰時推 `CryptoState`（只有數量） | 同上 | **需要** |
| `CryptoState.device_lists`（`changed`／`left`）＋ `dl_seq` | 別人的裝置清單變了、誰不再同房 | **不需要**：成員變動看房間事件，裝置變動看 F2／F3，過期由 F4 擋 |
| 某人加入／離開房間時，對房裡每個線上成員判斷「還有沒有共同加密房」再推（`push_membership_change`，背景 task） | 產生上面的 `changed`／`left` | **不需要**。這是 (B) 最貴的一段：每次加入離開都要做「成員數 × 共同房間判斷」 |
| 某人金鑰變動時，推 `changed: [他]` 給同房的人（`push_key_change`，背景 task） | 同上 | **不需要**：F3 取代 |
| `Device/Subscribe{dl_seq}` 的補窗（掃成員索引＋共同房間判斷） | 重連時補上面那兩個清單 | **不需要** |
| HTTP：`/sync` 共用的兩層、`left` 補上自己離開的房間（wbf-e2ee.md 決定 7）、`/keys/changes` 的 `left` 不再是空的 | 給**一般 Matrix client** | **跟客製 client 無關，但一般 client 受惠**，應該保留 |

### 9.2 兩個選項

- **(a) (B) 照原樣合併**，等這份提案實作完，再讓宣告過 `org.wbftw.device_versions` 的連線跳過上表標「不需要」的那幾項。
  - 好處：(B) 現在就能合併，client 馬上有一套能用的裝置清單訊號。
  - 壞處：同一件事在通道上會有**兩套機制並存**（`device_lists` 與 F2／F3／F4），server 要依連線判斷走哪一套；`push_membership_change` 那段高成本的判斷也會一直在。
- **(b) (B) 合併前先拿掉通道上的「別人的裝置清單」**：
  - `CryptoState` 只剩 `otk_counts`、`unused_fallback_key_types`、`gap`；拿掉 `device_lists`、`dl_seq`，`Device/Subscribe` 不再收 `dl_seq`。
  - 拿掉 `push_membership_change`、`push_key_change` 兩個背景推送。
  - **保留 HTTP 那半**（`/sync` 的共用層與 wbf-e2ee.md 決定 7、`/keys/changes` 的 `left`）。
  - 好處：通道上只有一套機制；server 不做那段高成本的判斷。
  - 壞處：在這份提案實作完之前，客製 client 在通道上**沒有**裝置清單變動的訊號（要的話只能用橋的 `KeyChanges`）。📎 issue #65 的 client 計畫裡，「自己加密送訊息」這一步本來就排在等 server 的後面，還沒上線。

建議 **(b)**：不為同一個 client 蓋兩套會漂移的機制。但這會讓 (B) 在審查時再改一輪（`e2e15` 要跟著改），由維護者在 #68 決定。

### 9.3 ✅ 定案：(B) 重作（維護者 2026-09-17）

> 保留要的 custom 功能就好，如果既然 matrix server 已經存在正常金鑰分發行為 而且可以讓官方 client work，那我們沒必要動。我們需要補的是加速我們自己開發客戶端的部分。

結果比 (b) 再多拿掉一半：
- 通道上照 (b)：`CryptoState` 只剩 `otk_counts`、`unused_fallback_key_types`、`gap`；兩個背景推送、`dl_seq` 與補窗都拿掉。
- **HTTP 那半也不保留**：`/sync` 的共用層、決定 7、`/keys/changes` 的 `left` 全部回到上游。9.1 表最後一列說「一般 client 受惠」，但官方 client 靠上游行為已經能用，所以不動。
- 客製 client 的裝置清單訊號只靠這份提案（F1–F4）。

## 10. 達標條件（對應問題書 §6）

| # | 條件 | 怎麼驗 |
|---|---|---|
| 1 | **重現靜默失敗再修好**：Bob 登入新裝置 → Alice 帶舊房間版本號送加密訊息 → `1506`（帶目前號碼）→ Alice 重拿 F2、重查、帶新號碼重送 → 接受 | e2e |
| 2 | **成員變動也算**：加入、離開、踢出、封鎖後，舊號碼被擋；**邀請後舊號碼仍然通過** | e2e |
| 3 | 🚨 **離開＋forget 之後號碼不會掉回去**（§4.2 的洩漏） | e2e ＋ 單元 |
| 4 | **每條 F1 路徑都讓序號 +1**（上傳 device keys、交叉簽章、簽章、刪裝置、聯邦）；**別人的簽章讓序號 +1 但雜湊不變** | 單元，每條做變異驗證 |
| 5 | **雜湊可重算**：client 用 `/keys/query` 拿到的東西照 §3.4 算，等於 server 給的 | 單元（黃金向量） |
| 6 | **沒有那一列時當場算、不讓房間版本號平白跳動** | 單元 |
| 7 | **沒約定不擋**：HTTP 送、沒宣告的連線不帶號碼 → 照常接受 | e2e |
| 8 | **有約定漏帶要擋**：宣告過的連線送加密訊息不帶號碼 → `InvalidRequest` | e2e |
| 9 | **F3 只推給宣告過的連線**；沒宣告的連線收不到任何 `0x14 0x07` | e2e |
| 10 | **F2 對官方 client 無害**：回應仍是合法的 Matrix 成員回應，多的欄位只在 `unsigned` 與最外層 | e2e（比對 HTTP 與橋） |
| 11 | **重啟後快取重算結果不變** | e2e |
| 12 | **橋帶 `at` 被拒** | 單元（表）＋ e2e |

## 11. 要維護者決定的

1. ✅ **邀請不算進房間版本號**（維護者 2026-09-17：版本號只為了分發金鑰，無權看的人不必管；他加入時自然會前進）。公式見 §4.1：只算 `join`、`leave`、`ban`。
2. ✅ **F3 的 subtype `0x14 0x07 DeviceChanged`**（維護者 2026-09-17）。
3. ✅ **依改動量分**（維護者 2026-09-17：小就一次、大一點兩支、很大再三支以上）。估計與建議（兩支：F1＋F2＋F4 → F3）見 §8。
4. ✅ **(B) `CryptoState` 瘦身**：維護者 2026-09-17 決定 (B) 重作，只留客製 client 要的自己金鑰存量，Matrix 原本的金鑰分發（HTTP）也回到上游（§9.3）。
5. ✅ **F2 改 `members.rs` 回自組的 JSON，房間版本號只放最外層一份**（維護者 2026-09-17：該動就動，每份成員都重複房間版本號太浪費）。

## 12. 不做的（連同理由，詳見討論紀錄 §2）

- 成對表、只含加密房的成員索引、每房**存**一個版本號、逐房扇出變動列、全域變動日誌、server 持續替 client 追蹤。
- 把裝置變動寫成時間線上的真事件（權限過不了、以使用者名義寫入、比逐房列更重的扇出、外流到聯邦、擠壓 `Recent`）。
- UI 手動補發金鑰（把正確性交給 UI）。
- 改 `/members` 的 Matrix 預設（破壞相容）。
