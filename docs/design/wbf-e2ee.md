# E2EE 全走通道：金鑰端點上橋、發 to-device、OTK 數量與裝置清單變動

> **這份文件回答：client 要讓 E2EE 完全不靠 `/sync`，server 要補哪幾件事、各自長什麼樣、分幾步做。**
> 狀態：✅ 維護者 2026-09-16 同意（§6 五條的決定記在各條後面）。起因是 issue #65（client 的需求）。
> 維護者 2026-09-15：「原本第三批次要加入的 api，可以先 defer，先處理金鑰的部分。」
> 上位文件：[wbf-api-bridge.md](wbf-api-bridge.md)（橋）、[wbf-to-device.md](wbf-to-device.md)（`0x16 Device`）、[../bridge-specs/index.md](../bridge-specs/index.md)（號碼總表）。

## 0. 一句話

分三步：**(A)** 金鑰端點與「發 to-device」全部走橋，只加表的列；**(B)** `0x16` 多一個 server → client 的原生 pack **`0x08 CryptoState`**，把 `/sync` 順便帶的 OTK 數量、unused fallback key、**裝置清單變動**推給持有這個裝置佇列的那條連線；**(C)** server 端金鑰備份（`/room_keys`）走橋，client 說可以晚一點。

## 1. `/sync` 在 E2EE 裡帶了什麼，通道上現在有什麼

client 用 `matrix-sdk-crypto` 的 `OlmMachine`（沒有網路 IO 的狀態機：server 給的推進去、它要送的請求拉出來）。它吃 `/sync` 的四樣東西，一次餵進 `receive_sync_changes`：

| `/sync` 帶的 | 用途 | 通道上現在 | 這份提案 |
|---|---|---|---|
| `to_device.events` | 收房間金鑰、金鑰請求與轉發、驗證 | ✅ `0x16` 的 `Fetch`／`Batch`／`Subscribe`／`Push` | 不動 |
| `device_one_time_keys_count` | 自己的 OTK 剩幾把 → 決定要不要補 | ❌ | (B) |
| `device_unused_fallback_key_types` | fallback key 被用掉了沒 → 決定要不要換 | ❌ | (B) |
| `device_lists.changed`／`left` | 同房的人裡誰的裝置清單變了、誰不再同房 → 下次發房間金鑰前要不要重新 `/keys/query` | ❌ | (B) |

`OlmMachine` 要 client 代送的請求是 `/keys/upload`、`/keys/query`、`/keys/claim`、**`/sendToDevice`**、`/keys/signatures/upload`、`/room_keys/…` —— 這些都是普通的 Matrix 端點，走橋就好，(A)(C)。

## 2. (A) 金鑰端點與發 to-device：走橋

| kind | subtype | 名稱 | 端點 | 變數 | data | 備註 |
|---|---|---|---|---|---|---|
| `0x16 Device` | `0x25` | SendToDevice | `PUT /sendToDevice/{event_type}/{txn_id}` | `event_type`、`txn_id` | JSON：`messages` | **發** to-device；收是既有的原生 `Push` |
| `0x17 Keys` | `0x20` | KeysUpload | `POST /keys/upload` | — | JSON：`device_keys`、`one_time_keys`、`fallback_keys` | 回覆帶 `one_time_key_counts` |
| | `0x21` | KeysQuery | `POST /keys/query` | — | JSON：`device_keys` | |
| | `0x22` | KeysClaim | `POST /keys/claim` | — | JSON：`one_time_keys` | |
| | `0x23` | KeyChanges | `GET /keys/changes` | query `from`、`to` | — | ⚠️ 見下 |
| | `0x24` | SigningKeysUpload | `POST /keys/device_signing/upload` | — | JSON：`master_key` 等、`auth` | **要 UIAA**，照批 2 的規則（bridge-specs §1.5） |
| | `0x25` | SignaturesUpload | `POST /keys/signatures/upload` | — | JSON：簽章 | |

- ⭐ **為什麼「發」走橋、不做原生 subtype**：`/sendToDevice` 是一問一答、帶 `txn_id` 冪等、關卡（限速、暫停、ignore）都在 HTTP 那一道。收的那一側已經有原生的佇列與推送，server 的 `add_to_device_event` 寫進佇列時就會推給對方的持有連線（wbf-to-device §4），**發的那一側不需要任何新東西**。
- ⚠️ **`/keys/changes` 的 `left` 是空的**：`src/api/client/keys/get_key_changes.rs` 寫著 `left: Vec::new(), // TODO`，而且它只查金鑰變動的索引，不含「有人新加入加密房」那種變動。所以**它不能拿來當裝置清單的正確來源**，(B) 不靠它。上橋是為了相容（client 已經在用 HTTP 版本的話可以直接換過來）。→ **決定 5**：要不要列、要不要順便補 `left`。
- 號碼：`0x17 Keys` 是 wire-format §3.3 早就分給 E2EE 的 kind，橋的號碼照慣例從 `0x20` 起；`SendToDevice` 放 `0x16`，因為它是 to-device 的另一半。
- e2e 照批 1、批 2 的規矩：每支橋打一次、HTTP 打一次、結果一致；`SigningKeysUpload` 驗 UIAA 兩輪；`SendToDevice` 另外驗「對方裝置的持有連線收到原生 `Push`」。

## 3. (B) `0x16 Device` 的 `0x08 CryptoState`

### 3.1 為什麼是一個新的 pack，不是在 `Batch`／`Push` 加欄位

issue 提的是在 `Batch`／`Push` 的 meta 多帶 `otk_counts` 等欄位。我建議另開一個 subtype，理由是這三樣東西**跟佇列裡的事件無關**：

| 情境 | 併進 `Push` | 獨立的 `CryptoState` |
|---|---|---|
| 別人 `/keys/claim` 走我一把 OTK（**沒有**任何 to-device 事件） | 要推一個 `bc: 0` 的空 `Push`，`counts` 是空陣列 —— `Push` 的語意（「佇列裡有新東西」）被借用 | 推一個 `CryptoState`，意思就是「狀態變了」 |
| Bob 換了新裝置（裝置清單變動，**沒有**事件） | 同上 | 同上 |
| 重新連上、補離線期間的變動 | `Subscribe{cd_seq}` 的補窗是事件窗，裝置清單的補法跟它完全不同（§3.4） | 訂閱成功後推一個 `CryptoState`，帶補齊的變動 |
| client 怎麼餵 `OlmMachine` | 每個 `Push` 都要拆出這幾個欄位 | `CryptoState` 直接對到 `receive_sync_changes` 的那幾個參數 |

→ **決定 1**。

### 3.2 形狀

`0x16` 的 `0x08`，server → client，**不帶 bit4**（原生）。

⭐ **共用 to-device 的訂閱，沒有新的訂閱**（維護者 2026-09-16 同意）：client 照舊送 `Device/Subscribe{device_id, cd_seq?}`（多一個可選的 `dl_seq`，§3.4），server 對這個訂閱送兩種 pack：佇列裡的事件是 `Push`，狀態變了是 `CryptoState`。兩者的 `id` 都抄 `Subscribe`、**共用同一個 `seq` 序列**（每推一個不論哪種都 +1），收的也是同一條連線：這個裝置佇列的持有者（wbf-to-device §4，後來的接手）。`Unsubscribe` 或被接手（`Superseded`）時，兩種一起停。

```json
{
  "otk_counts": { "signed_curve25519": 42 },
  "unused_fallback_key_types": ["signed_curve25519"],
  "device_lists": { "changed": ["@bob:example.org"], "left": ["@carol:example.org"] },
  "dl_seq": 81234,
  "gap": false
}
```

| 欄位 | 什麼時候有 | 語意 |
|---|---|---|
| `otk_counts` | **一定有** | 這個裝置目前每種演算法剩幾把 OTK，跟 `/sync` 的 `device_one_time_keys_count` 同一個數字（`users::count_one_time_keys`） |
| `unused_fallback_key_types` | **一定有**，可以是 `[]` | 還沒被用掉的 fallback key 演算法。⚠️ **`[]` 與不出現意思不同**：`OlmMachine` 把「沒給」當成 server 不支援、把 `[]` 當成「都用掉了，該換」。所以這一欄**不套**「沒有就不出現」的慣例 |
| `device_lists` | **一定有**，兩個陣列可以是空的 | 補窗：從 client 帶來的 `dl_seq` 到現在的變動；即時推送：觸發這次推送的那一個變動。語意照 `/sync`（§3.3）。清單超過一個 pack 的 meta 上限（`wbf_meta_max_bytes`）時**拆成好幾個 `CryptoState`**，每個都帶完整的數量欄位；client 把它們的清單依序餵進去就好 |
| `dl_seq` | **一定有** | **這個訂閱**的補窗從哪裡算起—— 整個訂閱期間是同一個數字，跟 `Subscribe` 回的 `latest_cd_seq` 相同。client 存下來，重新訂閱時帶回來（§3.4） |
| `gap` | **一定有**，bool | `true`：**這個訂閱**上一個推送之後有 pack 因為發送佇列滿被丟掉。⚠️ `Push` 與 `CryptoState` 共用一個訂閱（§3.2），所以也共用一個 `gap` 旗標：被丟的可能是其中任一種，而旗標由**下一個送得出去的**帶走（不論哪種）。client 在**任一種** pack 看到 `gap: true`，都帶 `cd_seq` 與 `dl_seq` 兩個水位重新 `Subscribe`，兩邊一起補（wbf-event-push §4 的同一套） |

欄位名照 `/sync` 的語意取短名 → **決定 2**。

### 3.3 什麼算「變動」（跟 `/sync` 一樣，`src/api/client/sync/v3.rs`）

- **`changed`** 是下面兩者的聯集：
  1. **金鑰變了**：`keychangeid_userid` 裡，在我加入的房間（`room_keys_changed`）或我自己（`keys_changed`）底下、count 落在區間內的使用者。寫入點是 `users::mark_device_key_update`（上傳 device keys、交叉簽章、簽章、刪裝置、聯邦來的 `m.device_list_update`）。
  2. **新同房**：某人加入我在的房間，而加入之前他跟我**沒有**共同的加密房。
- **`left`**：某人離開一個房間之後，跟我**不再有任何**共同的加密房。
- 🚨 **正確性，不是效能**（issue #65 需求 2）：Alice 沒收到「Bob 的清單變了」，就一直只把房間金鑰發給 Bob 的舊裝置，Bob 的新裝置永遠解不開，**而 Alice 那邊看不出來**。所以寧可多報不可少報：不確定就放進 `changed`，代價只是 client 多查一次 `/keys/query`。

### 3.4 什麼時候推、怎麼補

| 時機 | 推給誰 | `device_lists` |
|---|---|---|
| `Device/Subscribe` 成功之後（`Ack` 與補窗的 `Push` 之後），**一定送一個** | 這條連線 | 帶 `dl_seq` 訂閱：**從那裡補到現在**；沒帶：兩個陣列都空（第一次，client 本來就要自己追蹤房間成員）。📎 `dl_seq: 0` 跟初始 `/sync` 一樣，只報金鑰變動、不報成員變動 |
| `take_one_time_key`（別人 claim、聯邦 claim 都經過它） | 被 claim 的那個裝置的持有連線 | 空 |
| `add_one_time_keys`、上傳 fallback key | 那個裝置的持有連線 | 空 |
| `mark_device_key_update(bob)` | 跟 Bob 同房的**每個本地使用者**的每個有持有連線的裝置 | `changed: [bob]` |
| 某人加入／離開房間（`state_cache` 那組寫入點，channel 的 `follow`／`evict` 已經掛在那裡） | 房裡**本地**成員中，同房關係因此改變的人 | `changed: [x]` 或 `left: [x]` |

- **補法（`dl_seq` → 現在）**：把 `/sync` 算這兩個陣列的程式（`gather_device_list_updates`、`collect_device_list_left`）**搬到 service 層共用**，`/sync` 與 `CryptoState` 呼叫同一份 —— 兩份實作遲早漂移，而漂移的那天是金鑰送不到。e2e 直接拿同一個 `since` 比 `/sync` 的 `device_lists` 與 `CryptoState` 的補窗。→ **決定 3**。
- `dl_seq` 跟 `cd_seq`、`g_seq` **同一個號碼空間**（都是 `globals.next_count()`），但是**不同的水位**：`cd_seq` 是「to-device 處理完到哪」（會被銷毀），`dl_seq` 是「裝置清單算到哪」（隨時可以重算）。client 分開存，跟 wbf-to-device §2 的理由一樣。
- **即時推送的 `dl_seq`**：跟補窗那個一樣，**整個訂閱是同一個數字**。📎 **實作時改的**（原本寫「就是觸發它的那個寫入的 count」）：變動是**先寫入、再推**，而推送不照 count 的順序送達 —— 100 還在路上、101 先到，client 存下 101 後斷線，重連從 101 補就永遠沒有 100。改成：`Subscribe` 把這條連線登記成持有者之後讀 `globals.current_count()`（之前的寫入都已提交）當 `dl_seq`，補窗從 client 的 `dl_seq` 讀到**索引的尾巴**（不是到 `dl_seq` 為止）。一個變動要嗎寫在補窗讀之前（補窗讀得到），要嗎寫在之後（它的推送一定在登記之後，送得到）。代價是重連會重報這個訂閱期間的變動，多查幾次 `/keys/query`（安全側）。還沒補過窗的連線不推：它的補窗會讀到這個變動。推送因為佇列滿被丟掉時，下一個送得出去的 pack 帶 `gap: true`，client 用重新訂閱補（wbf-event-push §4）。
- 📎 **OTK 數量是「推的當下讀到的」**：兩次上傳或 claim 幾乎同時發生時，兩個 `CryptoState` 到達的順序不保證跟讀的順序一樣，client 可能短暫拿到舊數字。下一次變動或重新訂閱就會更正；`OlmMachine` 多補幾把或晚一點補都不會讓金鑰送不到。
- ⚠️ **只推給「持有這個裝置佇列」的那條連線**：`OlmMachine` 是每個裝置一台，佇列的持有者（wbf-to-device §4，後來的接手）就是在跑它的那條。同一裝置的其他連線不收。

### 3.4.1 補窗怎麼算：共用哪一層、輸入從哪來（實作前查到的，2026-09-16）

決定 3 選了 (a)「把 `/sync` 那份搬到 service 層共用」。動手前讀完 `src/api/client/sync/v3.rs`，那份程式**不是搬過去就能給補窗用**—— 它分成三層，只有兩層跟「怎麼算」有關，第三層是 `/sync` 順手拿的材料：

| 層 | `/sync` 現在 | 補窗（`CryptoState`、`/keys/changes`）能不能直接用 |
|---|---|---|
| ① **金鑰變動** | `keys_changed(我)` 加上每個我在的房間的 `room_keys_changed`（`keychangeid_userid` 索引，區間 `(since, next_batch]`） | ✅ 只吃 `(使用者, 區間)`，原樣共用 |
| ② **判斷** | 有人**加入** → 除了這個房間以外跟我沒有共同的加密房就放進 `changed`；有人**離開** → 離開後跟我沒有任何共同的加密房就放進 `left`（`share_encrypted_room`） | ✅ 只吃 `(我, 這個人, 哪個房間, 加入或離開)`，原樣共用 |
| ③ **「誰加入、誰離開」這份材料** | 從 `/sync` 為了回房間狀態而**已經算好**的逐房狀態差異與 timeline 裡挑成員事件 | ❌ 補窗沒有這份材料。為了它去跑一次 `/sync` 的逐房計算不合理 |

📎 **① 是什麼，舉個例子**：Bob 在新手機登入、上傳了新裝置的金鑰，server 的 `mark_device_key_update(bob)` 當下拿一個 count（例 500），在 `keychangeid_userid` 寫幾列：`(@bob, 500) → @bob`，以及 Bob 在的每個房間各一列 `(!room1, 500) → @bob`、`(!room2, 500) → @bob`。之後 Alice 問「從 400 到現在，誰的金鑰變了」，就是查「前綴是 @alice 自己、或是 Alice 在的任一個房間，count 落在 400 之後」的列 → 拿到 @bob。這一步只需要「Alice、從哪個位置開始」兩個輸入，跟有沒有人加入或離開房間無關，所以 `/sync`、`CryptoState` 的補窗、`/keys/changes` 三處呼叫**同一個函式**就好。它答的是 `changed` 裡「金鑰變了」那一半；另一半「新同房」與 `left` 是 ② 拿 ③ 的材料算出來的。

**補窗的 ③ 改從現成的成員索引拿**：`state_cache` 對每個（房間, 成員）記著加入時的 count（`roomuserid_joinedcount`）與離開時的 count（`roomuserid_leftcount`），跟 `dl_seq` 同一個號碼空間。對我在的（與我在區間內離開的）每個房間，挑 count 落在 `(dl_seq, 現在]` 的成員，交給 ②。

- **我自己在區間內加入的房間**：裡面每一個成員都當作「加入」交給 ②（原本不同房的人變成同房）。
- **我自己在區間內離開的房間**：裡面每一個成員都當作「離開」交給 ②。⚠️ **`/sync` 現在不做這件事**：它的 `left` 只從「我還在的房間」裡的離開事件算（`collect_joined_rooms` 才收 `left_encrypted_users`，`collect_left_rooms` 不收）。→ **決定 7**。
- ⚠️ **被 forget 的離開查不到**：`forget`（含 `forget_forced_upon_leave` 與被封鎖的房間）會刪掉那一列 `roomuserid_leftcount`。漏的只會是 `left`，不會是 `changed`（加入的 count 不被 forget 刪）。漏 `left` 的後果是 client 繼續追蹤一個已經不同房的人（多查幾次 `/keys/query`），房間金鑰該發給誰是看房間成員、不看這份清單，所以**不會把金鑰發給不該拿的人**。這是「寧可多報」（§3.3）那一側的誤差。
- 📎 **區間內加入又離開的人**：成員索引只記最後的狀態（離開時加入那一列就刪了），所以補窗只把他放進 `left`；`/sync` 從 timeline 看得到加入與離開兩個事件，可能 `changed`、`left` 都放。兩者都表示「現在不同房」，e2e 比對時 `changed` 先扣掉 `left` 裡的人再比（e2e15 [1.8]）。
- **成本**：補窗按我在的房間掃成員索引，是「我所有房間的成員數加總」的量級。`CryptoState` 每次 `Subscribe` 才做一次；`/keys/changes` 是 client 要才做。→ **決定 6**：`/sync` 要不要也改吃這份。

### 3.5 規模

- `mark_device_key_update` 的推送要列出「跟 Bob 同房的本地使用者的持有連線」。Bob 在大房間時這是房間成員數量級；但金鑰變動很少（新裝置、刪裝置、交叉簽章），而 `mark_device_key_update` 本來就逐房寫索引、還要算聯邦目的地，推送不會是這條路上最貴的一步。
- 成員變動的推送只在**加密房**、而且只看「同房關係是否改變」，跟 `/sync` 的 `share_encrypted_room` 同一個判斷。
- 📎 `device_key_update_encrypted_rooms_only`（設定，預設 false）照樣作用：金鑰變動只記在加密房時，推送也只看加密房。

## 4. (C) server 端金鑰備份：走橋（client 說可以晚一點）

`/room_keys/version`（建立、查、改、刪、最新）與 `/room_keys/keys`（全部、單房、單 session 的讀寫刪），ruma 有 14 支，全部已經掛在 Router 上（`src/api/router.rs` 的 `register_client_keys_and_backup_routes`）。都是普通的一問一答，走橋只加表的列，`0x17` 的 `0x30`–`0x3D`。放在 (A)(B) 之後。

## 5. 不做的

- **原生的「發 to-device」subtype**：§2，走橋就夠。
- **在 `Batch`／`Push` 加欄位**：§3.1（決定 1 定了獨立的 `CryptoState`）。
- **拿 `/keys/changes` 當裝置清單的來源**：§2，`left` 是 TODO，而且漏了「新同房」。
- **批 3 的候選端點**（推播規則、目錄、房間升級與敲門、搜尋…）：維護者 2026-09-15 defer，[wbf-api-bridge.md](wbf-api-bridge.md) §3 的清單保留。

## 6. 要維護者決定的

1. **OTK 數量與裝置清單用獨立的 `0x08 CryptoState`，還是併進 `Batch`／`Push`？** 建議 **獨立**（§3.1）。
   ✅ 維護者 2026-09-16：獨立。
2. **欄位名**：`otk_counts`、`unused_fallback_key_types`、`device_lists{changed,left}`、`dl_seq`？建議照這組（取 `/sync` 的語意、去掉 `device_` 前綴，因為整個 kind 就是 Device）。
   ✅ 維護者 2026-09-16：取 `/sync` 的語意，這組名字。
3. **補窗的算法搬到 service 層，`/sync` 與 `CryptoState` 共用**？建議照做（§3.4）。代價是動到 `/sync` 的程式（行為不變，由 e2e 比對兩邊守住）。
   ✅ 維護者 2026-09-16 同意「`CryptoState` **共用 to-device 的訂閱**」（§3.2 寫明）。
   ✅ **這條原本問的是另一件事，維護者 2026-09-16 選 (a)**：server **內部的程式**怎麼算「從 `dl_seq` 到現在，誰的清單變了、誰不再同房」。`/sync` 現在已經有一份（`src/api/client/sync/v3.rs` 的 `gather_device_list_updates`、`collect_device_list_left`），兩個選擇：(a) 把那份搬到 `src/service/`，`/sync`、`CryptoState` 的補窗、`/keys/changes` 三處呼叫同一份；(b) 替通道另寫一份。建議 **(a)**：兩份實作遲早對不上，對不上的那天就是金鑰送不到；代價是會動到 `/sync` 的程式（只搬位置、行為不變）。這是 server 內部的決定，wire 上看不到差別。
4. **分三個 PR**：(A) 走橋 7 列 → (B) `CryptoState` → (C) 備份 14 列？建議照這個順序；(A) 很小、client 馬上能用，(B) 是這份提案真正的工作量。
   ✅ 維護者 2026-09-16：同意。
5. **`/keys/changes`**：(a) 照樣上橋、文件註明 `left` 是空的；(b) 上橋並順便補 `left`（用決定 3 那份共用的算法）；(c) 不上橋。建議 **(b)**，而且放在 (B) 那支 PR —— 共用算法做好之後補 `left` 是幾行的事，也修掉上游的 TODO。
   ✅ 維護者 2026-09-16：(b)。
6. **補窗的「誰加入、誰離開」從哪來**（§3.4.1，實作 (B) 前查到、決定 3 沒有問到的一層）：
   - (i) **三層全共用**：`/sync` 也改成掃成員索引。真正只有一份；代價是**每次 `/sync`** 都要掃一次我所有房間的成員索引，在大房間多的帳號上會明顯變慢，而 `/sync` 手上其實已經有這份材料。
   - (ii) **共用 ① 金鑰變動與 ② 判斷**，搬到 service；③ 兩條路各自來：`/sync` 用它已經算好的，補窗與 `/keys/changes` 掃成員索引。e2e 拿同一個 `since` 比 `/sync` 與 `CryptoState` 的補窗，守住漂移。
   建議 **(ii)**：會漂移的是「怎麼算」，那兩層只有一份；材料的來源不同是因為成本不同，由 e2e 比對兩邊的結果守。
   ✅ 維護者 2026-09-16：照建議，(ii)。
7. **我自己離開房間時，那個房間裡「從此不同房」的人要不要進 `left`**（§3.4.1）？`/sync` 現在不報。
   - (a) `CryptoState` 與 `/keys/changes` 報，`/sync` 也順便補上。Matrix 規格的 `left` 就是「不再共有任何加密房」，不分誰走的；兩邊一致，e2e 比對不必排除這個情況。代價是動到 `/sync` 的行為（多報，不是少報）。
   - (b) 只有通道這邊報，`/sync` 不動；e2e 比對排除這個情況。
   - (c) 都不報，跟 `/sync` 一樣。
   建議 **(a)**。
   ✅ 維護者 2026-09-16：應該報。→ 照 (a)：`CryptoState`、`/keys/changes`、`/sync` 三處都報。

## 7. 落點

| 做什麼 | 落點 |
|---|---|
| (A) 表的 7 列、範例 | `src/api/client/wbf/bridge.rs`；`docs/bridge-specs/index.md`、新 `0x17-keys.md`、`0x16-device.md`；e2e |
| (B) `0x08 CryptoState` 的形狀 | `docs/design/wbf-to-device.md` §3（第八個 subtype）、`wbf-wire-format.md`；向量（欄位出現與空陣列各一） |
| (B) 推送 | `src/service/streams/devices.rs`（推給持有連線）；掛鉤：`users/keys.rs` 的 `take_one_time_key`、`add_one_time_keys`、`mark_device_key_update`，`state_cache` 的成員寫入點 |
| (B) 補窗與 `/sync` 共用的算法 | 從 `src/api/client/sync/v3.rs` 搬到 `src/service/`；`Device/Subscribe` 的 handler 呼叫（共用哪幾層見 §3.4.1、決定 6） |
| (B) `/keys/changes` 補 `left`（決定 5） | `src/api/client/keys/get_key_changes.rs` 呼叫同一份補窗 |
| (C) 備份 14 列 | `bridge.rs`、`0x17-keys.md` |
| client 同步 | 合併後開 client repo 的 issue（跟 #65 對應） |
