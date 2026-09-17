# E2EE 全走通道：金鑰端點上橋、發 to-device、自己的金鑰存量

> **這份文件回答：client 要讓 E2EE 完全不靠 `/sync`，server 要補哪幾件事、各自長什麼樣、分幾步做。**
> 狀態：✅ 維護者 2026-09-16 同意（§6 五條的決定記在各條後面）。(A) 已合併（PR #67），(C) 已實作（分支 `wbf/e2ee-backup`）；每支的範例在 [0x17-keys.md](../bridge-specs/0x17-keys.md)、[0x16-device.md](../bridge-specs/0x16-device.md)。起因是 issue #65（client 的需求）。
> 維護者 2026-09-15：「原本第三批次要加入的 api，可以先 defer，先處理金鑰的部分。」
> 上位文件：[wbf-api-bridge.md](wbf-api-bridge.md)（橋）、[wbf-to-device.md](wbf-to-device.md)（`0x16 Device`）、[../bridge-specs/index.md](../bridge-specs/index.md)（號碼總表）。

## 0. 一句話

分三步：**(A)** 金鑰端點與「發 to-device」全部走橋，只加表的列；**(B)** `0x16` 多一個 server → client 的原生 pack **`0x08 CryptoState`**，把 `/sync` 順便帶的 OTK 數量與 unused fallback key 推給持有這個裝置佇列的那條連線（裝置清單變動不在這裡，見 §3 開頭）；**(C)** server 端金鑰備份（`/room_keys`）走橋，client 說可以晚一點。

## 1. `/sync` 在 E2EE 裡帶了什麼，通道上現在有什麼

client 用 `matrix-sdk-crypto` 的 `OlmMachine`（沒有網路 IO 的狀態機：server 給的推進去、它要送的請求拉出來）。它吃 `/sync` 的四樣東西，一次餵進 `receive_sync_changes`：

| `/sync` 帶的 | 用途 | 通道上現在 | 這份提案 |
|---|---|---|---|
| `to_device.events` | 收房間金鑰、金鑰請求與轉發、驗證 | ✅ `0x16` 的 `Fetch`／`Batch`／`Subscribe`／`Push` | 不動 |
| `device_one_time_keys_count` | 自己的 OTK 剩幾把 → 決定要不要補 | ❌ | (B) |
| `device_unused_fallback_key_types` | fallback key 被用掉了沒 → 決定要不要換 | ❌ | (B) |
| `device_lists.changed`／`left` | 同房的人裡誰的裝置清單變了、誰不再同房 → 下次發房間金鑰前要不要重新 `/keys/query` | ❌ | 🔁 **不在這份**：客製 client 改用 [wbf-room-device-version.md](wbf-room-device-version.md) |

`OlmMachine` 要 client 代送的請求是 `/keys/upload`、`/keys/query`、`/keys/claim`、**`/sendToDevice`**、`/keys/signatures/upload`、`/room_keys/…` —— 這些都是普通的 Matrix 端點，走橋就好，(A)(C)。

## 2. (A) 金鑰端點與發 to-device：走橋

| kind | subtype | 名稱 | 端點 | 變數 | data | 備註 |
|---|---|---|---|---|---|---|
| `0x16 Device` | `0x25` | SendToDevice | `PUT /sendToDevice/{event_type}/{txn_id}` | `event_type`、`txn_id` | JSON：`messages` | **發** to-device；收是既有的原生 `Push` |
| `0x17 Keys` | `0x20` | KeysUpload | `POST /keys/upload` | — | JSON：`device_keys`、`one_time_keys`、`fallback_keys` | 回覆帶 `one_time_key_counts` |
| | `0x21` | KeysQuery | `POST /keys/query` | — | JSON：`device_keys` | |
| | `0x22` | KeysClaim | `POST /keys/claim` | — | JSON：`one_time_keys` | |
| | `0x23` | KeyChanges | `GET /keys/changes` | query `from`、`to` | — | ⚠️ 見下 |
| | `0x24` | SigningKeysUpload | `POST /keys/device_signing/upload` | — | JSON：`master_key` 等、`auth` | **換掉既有的金鑰要 UIAA**，照批 2 的規則（bridge-specs §1.5）。📎 實作時查到：**第一次上傳不要**（MSC3967），這格原本寫「要 UIAA」 |
| | `0x25` | SignaturesUpload | `POST /keys/signatures/upload` | — | JSON：簽章 | |

- ⭐ **為什麼「發」走橋、不做原生 subtype**：`/sendToDevice` 是一問一答、帶 `txn_id` 冪等、關卡（限速、暫停、ignore）都在 HTTP 那一道。收的那一側已經有原生的佇列與推送，server 的 `add_to_device_event` 寫進佇列時就會推給對方的持有連線（wbf-to-device §4），**發的那一側不需要任何新東西**。
- ⚠️ **`/keys/changes` 的 `left` 是空的**：`src/api/client/keys/get_key_changes.rs` 寫著 `left: Vec::new(), // TODO`，而且它只查金鑰變動的索引，不含「有人新加入加密房」那種變動。所以**它不能拿來當裝置清單的正確來源**，(B) 不靠它。上橋是為了相容（client 已經在用 HTTP 版本的話可以直接換過來）。→ **決定 5**：要不要列、要不要順便補 `left`。🔁 2026-09-17：不補，`left` 維持上游的樣子（§6 決定 5）。
- 號碼：`0x17 Keys` 是 wire-format §3.3 早就分給 E2EE 的 kind，橋的號碼照慣例從 `0x20` 起；`SendToDevice` 放 `0x16`，因為它是 to-device 的另一半。
- e2e 照批 1、批 2 的規矩：每支橋打一次、HTTP 打一次、結果一致；`SigningKeysUpload` 驗 UIAA 兩輪；`SendToDevice` 另外驗「對方裝置的持有連線收到原生 `Push`」。

## 3. (B) `0x16 Device` 的 `0x08 CryptoState`：只帶自己的金鑰存量

> 🔁 **維護者 2026-09-17 重作 (B)**：「既然 Matrix server 已經存在正常的金鑰分發行為，而且可以讓官方 client work，那我們沒必要動。我們需要補的是加速我們自己開發的客戶端的部分。」
> 所以 (B) **只留客製 client 要的**：自己剩幾把一次性金鑰、fallback key 用掉沒。原本的「別人的裝置清單變動」（`device_lists`、`dl_seq`、成員變動與金鑰變動的推送、補窗）與動到 Matrix 原本行為的部分（`/sync` 的共用層、自己離開也報 `left`、`/keys/changes` 補 `left`）**全部拿掉**，`/sync` 與 `/keys/changes` 回到上游的樣子。
> 客製 client 怎麼知道「誰的裝置變了」，由另一份提案處理：[wbf-room-device-version.md](wbf-room-device-version.md)（成員清單帶裝置版本號、房間版本號、送出時比對、裝置變動廣播）。
> 被拿掉的那一版做過什麼、為什麼不要了，留在 git 歷史（PR #68 的前幾個 commit）與 [wbf-device-index-notes.md](wbf-device-index-notes.md)。

### 3.1 為什麼是一個新的 pack，不是在 `Batch`／`Push` 加欄位

別人 `/keys/claim` 走我一把 OTK 時，**沒有**任何 to-device 事件。併進 `Push` 就得推一個 `bc: 0` 的空 `Push`，借用了「佇列裡有新東西」的語意。獨立的 `CryptoState` 意思就是「我的金鑰存量變了」。→ 決定 1。

### 3.2 形狀

`0x16` 的 `0x08`，server → client，**不帶 bit4**（原生）。

⭐ **共用 to-device 的訂閱，沒有新的訂閱**（維護者 2026-09-16 同意）：client 照舊送 `Device/Subscribe{device_id, cd_seq?}`，server 對這個訂閱送兩種 pack：佇列裡的事件是 `Push`，存量變了是 `CryptoState`。兩者的 `id` 都抄 `Subscribe`、**共用同一個 `seq` 序列與 `gap` 旗標**，收的是同一條連線（這個裝置佇列的持有者，wbf-to-device §4）。`Unsubscribe` 或被接手（`Superseded`）時兩種一起停。

```json
{ "otk_counts": { "signed_curve25519": 42 }, "unused_fallback_key_types": ["signed_curve25519"], "gap": false }
```

| 欄位 | 什麼時候有 | 語意 |
|---|---|---|
| `otk_counts` | **一定有** | 這個裝置目前每種演算法剩幾把 OTK，跟 `/sync` 的 `device_one_time_keys_count` 同一個數字（`users::count_one_time_keys`） |
| `unused_fallback_key_types` | **一定有**，可以是 `[]` | 還沒被用掉的 fallback key 演算法。⚠️ **`[]` 與不出現意思不同**：`OlmMachine` 把「沒給」當成 server 不支援、把 `[]` 當成「都用掉了，該換」 |
| `gap` | **一定有**，bool | `true`：**這個訂閱**上一個推送之後有 pack 因為發送佇列滿被丟掉（可能是 `Push` 也可能是 `CryptoState`，旗標由下一個送得出去的帶走）。client 在任一種 pack 看到 `gap: true`，就帶 `cd_seq` 重新 `Subscribe`：to-device 補窗，並拿到一個當下的 `CryptoState` |

欄位名照 `/sync` 的語意取短名 → 決定 2。

### 3.3 什麼時候推

| 時機 | 推給誰 |
|---|---|
| `Device/Subscribe` 成功之後（`Ack` 與 to-device 補窗的 `Push` 之後），**一定送一個** | 這條連線 |
| `take_one_time_key`（別人 claim、聯邦 claim 都經過它） | 被 claim 的那個裝置的持有連線 |
| `add_one_time_keys`、`add_fallback_keys` | 那個裝置的持有連線 |
| `take_fallback_key`（fallback 被用掉） | 那個裝置的持有連線 |

- 數字是「推的當下讀到的」。沒有連線持有這個裝置的佇列時，只多一次查找，不讀資料庫。
- 📎 兩次變動幾乎同時發生時，兩個 `CryptoState` 到達的順序不保證跟讀的順序一樣，client 可能短暫拿到舊數字；下一次變動或重新訂閱就會更正。`OlmMachine` 多補幾把或晚一點補，都不會讓金鑰送不到。
- ⚠️ **每個 `Subscribe` 之後一定跟一個 `CryptoState`**：client 的收包迴圈要認得或略過 `0x16 0x08`，不能把它當成下一個請求的回覆。

## 4. (C) server 端金鑰備份：走橋（client 說可以晚一點）

`/room_keys/version`（建立、查、改、刪、最新）與 `/room_keys/keys`（全部、單房、單 session 的讀寫刪），ruma 有 14 支，全部已經掛在 Router 上（`src/api/router.rs` 的 `register_client_keys_and_backup_routes`）。都是普通的一問一答，走橋只加表的列，`0x17` 的 `0x30`–`0x3D`。

✅ **已實作**：號碼與每支的形狀在 [0x17-keys.md](../bridge-specs/0x17-keys.md)，順序是先版本（`0x30`–`0x34`）再金鑰（`0x35`–`0x3D`，寫、讀、刪各三支：整份／單房／單 session）。`/room_keys/keys` 那九支的 `version` 是 **query 變數**，`/room_keys/version/{version}` 那四支的是 **path 變數** —— 同一個名字兩種位置，橋照 ruma 的模板自己分，client 兩邊都寫在 meta 裡就好。e2e13 情境 4。

## 5. 不做的

- **原生的「發 to-device」subtype**：§2，走橋就夠。
- **在 `Batch`／`Push` 加欄位**：§3.1（決定 1 定了獨立的 `CryptoState`）。
- **拿 `/keys/changes` 當裝置清單的來源**：§2，`left` 是 TODO，而且漏了「新同房」。
- **改 Matrix 原本的金鑰分發行為**（`/sync` 的 `device_lists`、`/keys/changes`）：維護者 2026-09-17，官方 client 靠它已經能用，不動。
- **通道上的「別人的裝置清單變動」**：改由 [wbf-room-device-version.md](wbf-room-device-version.md) 處理。
- **批 3 的候選端點**（推播規則、目錄、房間升級與敲門、搜尋…）：維護者 2026-09-15 defer，[wbf-api-bridge.md](wbf-api-bridge.md) §3 的清單保留。

## 6. 要維護者決定的

1. **OTK 數量與裝置清單用獨立的 `0x08 CryptoState`，還是併進 `Batch`／`Push`？** 建議 **獨立**（§3.1）。
   ✅ 維護者 2026-09-16：獨立。
2. **欄位名**：`otk_counts`、`unused_fallback_key_types`、`device_lists{changed,left}`、`dl_seq`？建議照這組（取 `/sync` 的語意、去掉 `device_` 前綴，因為整個 kind 就是 Device）。
   ✅ 維護者 2026-09-16：取 `/sync` 的語意，這組名字。🔁 2026-09-17 重作 (B)：`device_lists`、`dl_seq` 拿掉，剩 `otk_counts`、`unused_fallback_key_types`、`gap`。
3. **補窗的算法搬到 service 層，`/sync` 與 `CryptoState` 共用**？建議照做（§3.4）。代價是動到 `/sync` 的程式（行為不變，由 e2e 比對兩邊守住）。
   ✅ 維護者 2026-09-16 同意「`CryptoState` **共用 to-device 的訂閱**」（§3.2 寫明）。
   🔁 **2026-09-17 取消**（重作 (B)：不動 Matrix 原本的行為，沒有補窗要共用）。原本的決定：
   ✅ **這條原本問的是另一件事，維護者 2026-09-16 選 (a)**：server **內部的程式**怎麼算「從 `dl_seq` 到現在，誰的清單變了、誰不再同房」。`/sync` 現在已經有一份（`src/api/client/sync/v3.rs` 的 `gather_device_list_updates`、`collect_device_list_left`），兩個選擇：(a) 把那份搬到 `src/service/`，`/sync`、`CryptoState` 的補窗、`/keys/changes` 三處呼叫同一份；(b) 替通道另寫一份。建議 **(a)**：兩份實作遲早對不上，對不上的那天就是金鑰送不到；代價是會動到 `/sync` 的程式（只搬位置、行為不變）。這是 server 內部的決定，wire 上看不到差別。
4. **分三個 PR**：(A) 走橋 7 列 → (B) `CryptoState` → (C) 備份 14 列？建議照這個順序；(A) 很小、client 馬上能用，(B) 是這份提案真正的工作量。
   ✅ 維護者 2026-09-16：同意。
5. **`/keys/changes`**：(a) 照樣上橋、文件註明 `left` 是空的；(b) 上橋並順便補 `left`（用決定 3 那份共用的算法）；(c) 不上橋。建議 **(b)**，而且放在 (B) 那支 PR —— 共用算法做好之後補 `left` 是幾行的事，也修掉上游的 TODO。
   ✅ 維護者 2026-09-16：(b)。
   🔁 **2026-09-17 改成 (a)**：上橋了（PR #67），但不補 `left` —— 不動 Matrix 原本的行為。
6. **補窗的「誰加入、誰離開」從哪來**（§3.4.1，實作 (B) 前查到、決定 3 沒有問到的一層）：
   - (i) **三層全共用**：`/sync` 也改成掃成員索引。真正只有一份；代價是**每次 `/sync`** 都要掃一次我所有房間的成員索引，在大房間多的帳號上會明顯變慢，而 `/sync` 手上其實已經有這份材料。
   - (ii) **共用 ① 金鑰變動與 ② 判斷**，搬到 service；③ 兩條路各自來：`/sync` 用它已經算好的，補窗與 `/keys/changes` 掃成員索引。e2e 拿同一個 `since` 比 `/sync` 與 `CryptoState` 的補窗，守住漂移。
   建議 **(ii)**：會漂移的是「怎麼算」，那兩層只有一份；材料的來源不同是因為成本不同，由 e2e 比對兩邊的結果守。
   ✅ 維護者 2026-09-16：照建議，(ii)。🔁 **2026-09-17 取消**（沒有補窗了）。
7. **我自己離開房間時，那個房間裡「從此不同房」的人要不要進 `left`**（§3.4.1）？`/sync` 現在不報。
   - (a) `CryptoState` 與 `/keys/changes` 報，`/sync` 也順便補上。Matrix 規格的 `left` 就是「不再共有任何加密房」，不分誰走的；兩邊一致，e2e 比對不必排除這個情況。代價是動到 `/sync` 的行為（多報，不是少報）。
   - (b) 只有通道這邊報，`/sync` 不動；e2e 比對排除這個情況。
   - (c) 都不報，跟 `/sync` 一樣。
   建議 **(a)**。
   ✅ 維護者 2026-09-16：應該報。→ 照 (a)：`CryptoState`、`/keys/changes`、`/sync` 三處都報。🔁 **2026-09-17 取消**：`/sync` 與 `/keys/changes` 回到上游的樣子，`CryptoState` 不再帶清單。

## 7. 落點

| 做什麼 | 落點 |
|---|---|
| (A) 表的 7 列、範例 | `src/api/client/wbf/bridge.rs`；`docs/bridge-specs/index.md`、新 `0x17-keys.md`、`0x16-device.md`；e2e |
| (B) `0x08 CryptoState` 的形狀 | `docs/design/wbf-to-device.md` §3（第八個 subtype）、`wbf-wire-format.md`；向量（欄位出現與空陣列各一） |
| (B) 推送 | `src/service/streams/devices.rs`（推給持有連線）；掛鉤：`users/keys.rs` 的 `add_one_time_keys`、`add_fallback_keys`、`take_one_time_key`、`take_fallback_key`；`Device/Subscribe` 的 handler（`api/client/wbf/device.rs`） |
| (C) 備份 14 列 | `bridge.rs`、`0x17-keys.md` |
| client 同步 | 合併後開 client repo 的 issue（跟 #65 對應） |
