# to-device 走通道：`0x16 Device` 的訂閱、推送與銷毀

> **狀態**：✅ 已實作（提案 2026-09-11 PR #41 核可，實作 PR #43 2026-09-12 合併；討論過程在本 repo 的 issue #39 與 PR #41）。
> 起點是 client 端（`amaid/wbf-matrix-client`）的需求，原文在 client repo 的
> `docs/design/to-device-push-proposal.md`；它列的四個待決事項維護者 2026-09-10／2026-09-11 都拍板了，
> 連同討論中改掉的形狀（銷毀的閉環、`to`、`ot`／`nt`、上限）一起寫在下面各節。
> 🔁 **合併前後改掉的一條**：§4 的裝置綁定從「拒絕後來的」翻成「**後來的接手**」（維護者 2026-09-12）——
> 理由與被接手那條收到的 `Superseded`(1505) 都寫在 §4。§10 的三件事實作時查過，結論記在那節。
>
> 相關：[wbf-wire-format.md](wbf-wire-format.md)（§3.3 的 `0x16` 這一格、§3.4 錯誤詞表）、
> [wbf-event-push.md](wbf-event-push.md)（訂閱與推送，PR #36 已實作）、
> [room-seq-and-recent.md](room-seq-and-recent.md)（`Recent` 的拉窗）。

## 0. 一句話

to-device（Megolm 金鑰、裝置驗證、SSSS secret）走 `0x16 Device`，形狀跟 `0x14 Event` **同構** ——
推送是主路、`Fetch` 是斷線後補洞 —— 差別只有一個：**它的東西是一次性的，client 收好之後要叫 server 銷毀**。

## 1. 為什麼不併進 `Event/Recent`

維護者問過「device key 能不能混進 event 一起發」。答案是不併，理由只有一條，但那條夠硬：

> **一個游標同時代表兩件事，而其中一件是破壞性的。**

```
拉一窗 → 房間事件寫進 client 的快取 ✅ → 金鑰匯進 crypto store ❌（磁碟滿／store 壞）
                                        ↑ 游標已經前進 = 已經表示「可以刪了」
```

房間事件的水位前進只表示「我快取好了」，錯了**還能重讀**（事件永久保存）；
to-device 的水位前進表示「可以刪了」，錯了**就沒了**。要讓同一個游標兼任這兩種語意，
client 得讓兩個資料庫原子性地一起 commit —— 兩個 db、兩套失敗模式，做不到。

⭐ 第二個獨立的理由：**to-device 不只有房間金鑰**。`m.key.verification.*`（SAS）是**互動**的：
如果它只在「拉房間歷史」時順便帶回來，使用者按下「驗證這台裝置」之後，對面要等到有人去拉歷史才收得到。
📎 所以 to-device 需要自己的推送時機，不能寄生在房間事件的節奏上。

## 2. 游標：同一個號碼空間，兩個不同的水位

`add_to_device_event`（`src/service/users/device.rs`）用的是 `globals.next_count()` ——
**跟 PDU 的 count 同一支**，所以 to-device 的 count 與 `g_seq` 可以直接比大小。但它們是兩個水位，名字要分開：

| | client 存的水位 | 語意 | 誰推進 |
|---|---|---|---|
| 房間事件 | `cg_seq` | 我快取到哪 | client 自己，可重讀 |
| **to-device** | **`cd_seq`** | 我 **durable 收下並處理完**到哪 | client 自己；**銷毀是另一個獨立的動作**（§5） |

⚠️ **`g_seq` 寫得進 PDU 的 `unsigned`，to-device 的 count 沒有地方放** —— 它不是 PDU，
存起來的就是 `{ type, sender, content }`。所以每一則的 count 必須由 pack 的 meta 帶（§3 的 `counts`）。

## 3. `0x16 Device` 的七個 subtype

編號刻意跟 `0x14 Event` 對齊（同號同位置，好對照）：

⚠️ **「`id` 由 client 選」講的是選那七個 byte 的值，第一個 byte 是型別 `0x01`**（client 的會話號，[wbf-wire-format.md](wbf-wire-format.md) §2.2，PR #47）：這個 kind 的每個 subtype 都吃 `0x01`，裸值（型別 `0x00`）在 handler 之前就被回 `InvalidRequest`。

| subtype | 方向 | meta | data | 順序類別 |
|---|---|---|---|---|
| `0x01 Fetch` | client → server | `{ "limit": 1000?, "cd_seq": <count>? }`；`id` 由 client 選 | 無 | 無序 |
| `0x02 Batch` | **server → client** | `{ "tc", "bc", "ot", "nt", "counts": [...], "r" }`；`id` 抄 `Fetch`，`seq` 從 0 嚴格 +1 | `bc` 則事件，u32 大端長度 ＋ JSON | 有序 |
| `0x03 ItemsDestroy` | client → server | `{ "tc": <筆數> }`；`id` 由 client 選 | **`tc` × 8 byte**，每個是一個 u64 大端的 count（§5.1） | 無序 |
| `0x04 Subscribe` | client → server | `{ "device_id": "…", "cd_seq": <count>? }`；`id` 由 client 選 | 無 | 無序 |
| `0x05 Unsubscribe` | client → server | `{}` | 無 | 無序 |
| `0x06 Push` | **server → client** | `{ "bc", "ot", "nt", "counts": [...], "gap": bool }`；`id` 抄 `Subscribe`，`seq` 每推一次 +1 | 同 `Batch` 的切法 | 事件驅動 |
| `0x07 ItemsDestroyed` | **server → client** | `{ "tc", "bc" }`；`id` 抄 `ItemsDestroy` | **`bc` × 8 byte**，銷毀掉的 count（§5.2） | 無序（一個命令一則） |

### 3.1 跟 `Event` 那一套刻意不同的三處

**① 順序是舊 → 新，不是新 → 舊。** `Event/Batch`／`Push` 是新到舊（讀歷史從最新看起）；這裡相反：

- **銷毀是逐則推進的**：處理一則、記一則，舊→新才走得順。
- **`m.key.verification.*` 有序**：一個 SAS flow 的步驟顛倒過來就跑不動。

**② `fs`／`ls` 換成 `ot`／`nt`**（維護者 2026-09-10 定的縮寫）。`Event` 那邊 `fs`／`ls` 的定義是
「這批**最新**／**最舊**的 `g_seq`」——**語意的，不是位置的**。順序一翻，同一組名字會指到相反的東西。
🚫 **不要把 `ot`／`nt` 跟 `fs`／`ls` 混用，兩邊順序相反**：`ot` = 這批最舊的 count，`nt` = 這批最新的。

**③ 多一個 `counts` 陣列。** `Event` 的每則事件自己的 `unsigned` 裡有 `g_seq`；to-device 沒有那個位置，
所以 meta 帶 `counts: [c1, c2, …]`（`bc` 個，跟 data 的事件一一對應）。
⚠️ 沒有它就只能整批銷毀：批次中間匯入失敗時，要嘛整批重來、要嘛冒險銷毀還沒處理好的。

### 3.1.1 📎 考慮過但不做的：`Fetch` 的區間上界（`to`）

client 原提案有一個 `to`（只要比它舊的）。維護者 2026-09-11 問「`to` 跟 `Recent` 的 `before` 是不是同一個東西」——
**形狀像，角色不同**，因為兩邊方向相反：

| | 底（只要比它新的） | 翻頁把手 |
|---|---|---|
| `Event/Recent`（新→舊） | `cg_seq` | **`before`**（下一窗帶上一窗的 `ls`） |
| `Device/Fetch`（舊→新） | **`cd_seq`** —— **同時**是翻頁把手（下一窗帶上一窗的 `nt`） | 同左 |

所以 `to` 對到的是 **`cg_seq` 的位置**（區間的另一端），不是 `before`；`before` 的角色這邊由 `cd_seq` 兼任。
剩下的問題是「別給我比 X 新的」誰要用：唯一想得到的情境是「訂閱後一邊補洞一邊收推送，想把補的範圍卡在訂閱當下」，
而那個情境**不需要它** —— 重複拿到同一則無害（匯進 crypto store 冪等、銷毀命令也冪等），`Event` 那邊同樣的縫是靠 client 自己去重，
也沒有為它加參數。

⭐ **定案：不做**（維護者 2026-09-11：「`to` 看起來很多餘」）。加一個線上參數很便宜，**拿掉一個已經在線上的參數很貴**；
之後真有呼叫點再加，那時它的語意也會被那個呼叫點定得更準。

### 3.2 跟 `Event` 一樣的部分（不重複定義）

- **只走 WS**（准入表），HTTP 回 `Unsupported`。
- **一個 pack 同時受兩個上限切**：則數與 `wbf_data_max_bytes`，共用 `core::wbf::events::list_pack_ranges`
  （[wbf-wire-format.md](wbf-wire-format.md) §2.1 那條教訓：一個規則兩份實作一定會漂）。
- **`gap`**、**`seq` 只是推送序號**、**水位只認 `nt`／`ot` 不認 `seq`**：`wbf-event-push.md` §2／§4 原封適用。

## 4. 訂閱：一個裝置同時只有一條連線在收，**後來的接手**

⚠️ 協議層的訂閱者仍然是**連線**（`connection_id`），`wbf-event-push.md` §1 那條**不動**。
這裡多的是**登記時的鍵**：`Device/Subscribe` 的 meta 帶 `device_id`，server 把那條連線綁到該裝置。

理由是**銷毀是破壞性的**：同一裝置兩條連線都在收，A 收到 count=500 匯入成功後叫 server 銷毀，
B 那邊還在處理就沒得救了。所以同時只有一條 —— 但「哪一條」的答案是**最後訂的那條**。

- server 有一張 **`device_id → connection_id`** 的綁定（實作上是串流註冊表裡這個裝置的 topic，
  🚫 **不另外開一張表**：兩張表遲早對不上，而對不上的那天沒有人會收到通知）。
- 🔁 **後來的搶先來的**（維護者 2026-09-12 定）。原本寫的是相反的規則（第二條回 `Conflict`，先來的不動），
  **推翻掉**，理由是**卡死的代價不對稱**：
  | | 搶佔（現在） | 拒絕（原本） |
  |---|---|---|
  | 先來的還活著、正在匯入 | 它被打斷，但**收得到通知**，而且那些項目沒被銷毀、還在佇列裡，重訂就再推一次 | 沒事 |
  | 先來的其實已經死了（半開連線） | 新連線立刻接上 | **到 idle timeout（300 秒）之前，這個裝置根本訂不進來** |
  | 先來的卡死不動 | 新連線立刻接上 | ⛔ **這個 device id 永久廢掉**，金鑰再也送不到 |
  ⭐ 換句話說：搶佔最壞是**重推一次已經還在佇列裡的東西**，拒絕最壞是**這個裝置再也收不到金鑰**。
  手機換網路就會踩到中間那列，這不是罕見狀況。
- 🚨 **被搶走的那條一定要被告知** —— 靜默失效是這裡最糟的結果：它會一直以為自己還在收金鑰。
  它收到一個 `Control/Error`：
  ```
  code = Superseded  code_id = 1505   IS_LAST
  id   = 它自己當初 Subscribe 用的 id
  seq  = 接在它那段會話後面
  ```
  ⭐ 用**它自己的 id**，所以 client 不必為這件事準備第二套解析：對回自己的訂閱就知道死的是哪一個
  （wire-format §3.4）。⚠️ **連線本身不關**：它的房間訂閱照常，只有 to-device 這一路被接手。
- ⚠️ **這條規則屬於註冊表，不屬於 handler**：建構這個串流時就宣告「一個 topic 一條連線」，
  進 topic 的**同一把寫鎖內**執行。🚫 不要在 handler 裡「先查有沒有人佔著、再登記」——
  那是兩次取鎖，兩條同時來的連線會雙雙看到空的、雙雙進去，綁定等於沒有（PR #42 的 join hook 同型錯誤）。
- **`Unsubscribe` 解除綁定**（維護者 2026-09-11）：訂閱與綁定是同一件事的兩面 —— 不解除的話，
  那條連線退訂了卻還佔著裝置，其他連線得靠搶佔才進得來，而且沒有任何東西會來收拾它。
- 🚨 **在同一條連線上換身分（`Session/Login` 換成另一個 user／device）也要解除** —— 而且是**每一條串流都退**，
  不是只退房間（審查者 cirno／rumia／salvia，PR #43）。漏掉的話：舊裝置的 to-device 項目會被推進**現在屬於
  另一個人的**佇列（別人的 Megolm 金鑰送進錯的 session），而且新 session 不是那個 topic 的持有者、銷毀會被拒 ——
  那個裝置從此卡死。⚠️ 呼叫的是 `Streams::remove_connection`（唯一的全清入口），🚫 **不是**
  `Event/Unsubscribe` 用的 `unsubscribe_all_rooms` —— 後者的語意是「不聽房間了」，把 to-device 一起清掉是另一回事。
- 連線結束時也解除（RAII，跟 `ConnectionGuard` 同形），下一條連得上。
  ⭐ 兩條路都要有：`Unsubscribe` 是說出口的退出，斷線是沒說出口的退出，🚫 不能只接一種。
  📎 搶佔存在不代表這兩條可以省：它們是**正常**的歸還路徑，搶佔是**故障**的那條。
- ⚠️ **`device_id` 必須跟 session 的對得上，對不上回 `Error(Forbidden)`（1302）**（維護者 2026-09-10）。
  連線是 `Session/Login` 換來的，**session 裡那個才是身分的唯一來源**；client 帶進來的只是**明示意圖**。
  🚫 不驗的話，裝置 A 可以訂閱裝置 B 的 to-device 再叫 server 銷毀 —— 同一個帳號底下的橫向破壞。
- 📎 **為什麼還要 client 報**（session 已經知道）：它保護 client 不被自己的 bug 害死 ——
  client 若拿著錯的 `device_id`，沒有這道比對時 server 會照 session 正確地同步，而 client 把金鑰匯進
  **另一個 crypto store**，然後那些項目已經被銷毀，救不回來。一次登記多一個字串，換 fail closed。

## 5. 銷毀：`ItemsDestroy` → `Ack` → `ItemsDestroyed` 的閉環

維護者 2026-09-11 定的形狀。**client 收到推送不回任何東西**；等它 durable 存好，才發銷毀命令。

```
client 訂閱 ──▶ server 推送（Push）
client 存好（不回 ACK）
client ──▶ Device/ItemsDestroy { tc }，data = tc × 8 byte 的 count
server ──▶ Control/Ack          ← 只表示「命令收到」，不表示刪了
server 執行刪除
server ──▶ Device/ItemsDestroyed { tc, bc }，data = bc × 8 byte 的 count（真的沒了的那些）
client：在清單裡的 → 本地是唯一真相；不在清單裡的 → 遠端仍有，下次再清
```

- ⭐ **`Ack` 只表示「收到命令」**，不表示刪除完成 —— 這是維護者 2026-09-11 的更正，本來的 `Ack{until}`
  同時兼任兩件事，而其中一件會失敗。
- ⭐ **只有一種結果封包**（`ItemsDestroyed`）。沒刪掉的**由 client 相減得出**：它手上有送出去的清單，
  `tc` 又抄回了命令的總數。🚫 不另外發一個「沒刪掉」的封包 —— 同一件事兩個來源會漂。
- ⚠️ **一把都沒刪掉，也要回一則 `bc = 0` 的 `ItemsDestroyed`。** 否則「一把都沒刪」跟「server 沒回應」
  在 client 眼裡長得一樣，而那兩件事的處置不同（前者 do nothing，後者是「卡在可能要重試」）。
- ⭐ **「已經不在了」算銷毀成功**（維護者 2026-09-11）：server 去刪、發現確實不在，就列進 `ItemsDestroyed`。
  client 要的是「遠端沒有這把了」這個**狀態**，不是「這次是我刪掉的」這個**事件**。所以命令是**冪等**的，重送安全。
- **client 不必無窮等**：整條路是事件驅動的，收到 `ItemsDestroyed` 就更新本地。真正的卡住只有一種
  ——**連 `Ack` 都沒收到**，那就是「可能要重試」，而重試安全（上一條）。

### 5.1 命令的 data：固定 8 byte，**沒有分隔符號**

每個 count 是 **u64 大端 8 byte**，一個接一個，`tc` 在 meta。

- 🚫 **不要分隔符號。** `0xFF` 這種分隔會出現在資料裡（count = 255 的最低位就是 `0xFF`），
  解析端會把一個數字切成兩半。固定寬度本來就不需要分隔：筆數就是 `data.len() / 8`。
- ⭐ 於是得到一個免費的 fail-closed 檢查：**`tc * 8 != data.len()` → `Error(InvalidRequest)`，一個都不刪。**
  兩邊對不上代表有一端的編碼壞了，那種時候不准動手。
- 大小：**典型是一個包一個呼叫** —— 100 × 8 = **800 byte**（§5.4 的節奏）。
  上限是 `tc` 受 `wbf_data_max_bytes` 自然限制（16 MiB ÷ 8 ≈ 2M 筆）；client 若選擇積整窗再送，也不過 1000 × 8 = 8 KB。

### 5.2 結果的 data：同一個格式

`ItemsDestroyed` 的 data 是 `bc` × 8 byte 的 count（同樣沒有分隔符號），meta 的 `tc` 抄命令的總數、
`bc` 是這則清單的筆數。client 用 `tc` 對回自己送出去的那張清單，相減得出還在遠端的。

### 5.3 `Error` 與「沒刪掉」是兩件事

| | 意思 | client 怎麼辦 |
|---|---|---|
| **`Error`** | **命令沒被受理**：不是綁定的那條連線（`Forbidden`）、`tc` 與 data 對不上（`InvalidRequest`）、server 自己爆了（`Internal`） | 照 [wbf-wire-format.md](wbf-wire-format.md) §3.4 那張表 |
| **`ItemsDestroyed` 沒列到的 count** | **命令受理了，但這幾把沒刪掉** | do nothing，遠端仍有；下次開機再清 |

⭐ 所以**不需要 NACK**：NACK 想講的「還在、再試一次」就是「不在 `ItemsDestroyed` 清單裡」，
而且它比 NACK 多告訴你**是哪幾把**。

### 5.4 節奏：一個包處理完就銷毀那一包

⭐ **銷毀跟著 pack 走，不是跟著窗走**（維護者 2026-09-11）：client 每收到一個 `Batch`／`Push`，
解包、逐則處理完，就對**那一包的 counts** 發一個 `ItemsDestroy`。整條路都是事件驅動的，
🚫 不必等一窗（10 包）收齊。

- 所以**典型的命令是 100 筆、800 byte**（§5.1），不是一窗 1000 筆。
- ⭐ 這樣「已經處理好、但遠端還留著」的那段窗口最短 —— 一包而不是一窗。
- 📎 client 要積起來一起送也可以（協議不限制，上限是 §7 的一窗），只是沒有理由這麼做。

## 6. 保留期：無窮 TTL（維護者 2026-09-11 定）

**沒被銷毀的 to-device 永遠留著**，`ItemsDestroy` 是唯一的刪除入口（底下就是既有的
`remove_to_device_events`）。⭐ 理由：靜默丟掉金鑰的代價（那些訊息**永遠解不開**）遠大於佔磁碟。

⚠️ 代價要寫清楚：**被遺棄的裝置那條佇列會永遠長大**（手機掉了、重灌又沒登出）。配套兩條，
實作那支要一起做，否則「無窮」會是**不可觀測的**無窮：

1. **刪裝置就清佇列** —— 實作時確認 `remove_device` 那條路現在有沒有做；沒有就補。
2. **admin 看得到佇列大小** —— 一個指令印出「誰的哪台裝置堆了多少則、最舊的是什麼時候」。

## 7. 上限與設定

⭐ **`limit` 不是隨便挑的數字，是算出來的**（維護者 2026-09-11 指正）。關鍵是：
**佔發送佇列的是「包數」，不是則數** —— `wbf_ws_send_queue_len`（32）數的是 pack，
所以一窗切出來的包數必須放得進去，server 送完一窗才不會卡在佇列上等 client 讀。

```
Recent：  一包  10 則 × 32 包 = 一窗  320 則     32 包 = 佇列剛好滿
Device：  一包 100 則 × 10 包 = 一窗 1000 則     10 包 < 32，佇列還留得下別的回應
```

⚠️ 兩邊的形狀是**相反**的：`Recent` 是**小包多包**（一包 10 則），Device 的補窗是**大包少包**（一包 100 則）。
to-device 一則約 1 KB 又不需要逐則渲染，包大一點反而省來回；房間事件要一進來就上畫面，所以切細。
📎 但**推送那條路兩邊一樣**（一包最多 10 則，共用 `wbf_push_max_events_per_pack`）：推送要的是快，不是吞吐。

| 旋鈕 | 預設 | 說明 |
|---|---|---|
| `wbf_device_fetch_default_limit` | **1000** | **一次 `Fetch` 回幾則**（client 沒帶 `limit` 時）。積得比這多就多叫幾次，帶上一窗的 `nt` 當 `cd_seq` |
| `wbf_device_fetch_max_limit` | **1000** | client 帶的 `limit` 夾到這裡。🔲 維護者 2026-09-11 只定了「一次一千則」這個數，所以兩者同值；要讓 client 能要更多再分開 |
| `wbf_device_default_batch` | **100** | **一包幾則**（`Fetch` 的回應 `Batch`）。⚠️ **client 不能指定** —— `Fetch` 沒有 `batch` 參數（`Recent` 有），一包裝多少是 server 的事 |
| `wbf_push_max_events_per_pack` | 10（**與 `Event` 共用**） | **server 主動推的一包最多幾則**（維護者 2026-09-11）。推送要的是快，一包小一點先出去；補窗要的是吞吐，才用上面那個 100 |
| ~~包數~~ | *（算出來的，10）* | `ceil(limit ÷ 一包則數)`。🚫 **不給旋鈕**：三個數字只有兩個自由度，三個都能調就會有「互相矛盾時誰贏」的問題。✅ 啟動時斷言它 ≤ `wbf_ws_send_queue_len`（`config/check.rs` 的 `check_wbf_device_window`），不成立就是設定錯，fail closed |

⚠️ **那個斷言擋的是什麼，講精確一點**（PR #43 審查時補上的，審查者 rumia／salvia 抓到它只寫在文件上、沒落地）：
`Fetch` 的 `Batch` 是**等得到佇列**才送的（handler `await`，不是 `try_send`），所以**不會掉包** ——
放不下時發生的是**那個 handler 卡在半窗**等 client 讀，同時佔著它的名額。那不是資料損失，但是個沒人看得見的
停頓，而且它的成因純粹是設定值互相矛盾 —— 這種東西應該在啟動時就講出來。

📎 **`limit: 0` 就是「不要」**：回一則空的 `Batch`（`r=0`），跟 `Event/Recent` 一致。
🚫 不夾成 1 —— 那會回一則沒人要的，還順手把 client 那個「欄位沒初始化」的 bug 藏起來。

⭐ **兩條路兩個數，是刻意的**：`Fetch`→`Batch` 是補洞（**大包少包**，100 × 10）、`Push` 是即時（**一包最多 10**，
而且實務上就一則）。to-device 一則約 1 KB，所以兩邊都遠在 `wbf_data_max_bytes` 之內。

`wbf_data_max_bytes` 對所有 pack 一樣適用，**兩個上限哪個先滿就切在哪**。

### 7.1 現有的旋鈕實際上管什麼（`Recent` 那組**不動**）

⭐ **定案（維護者 2026-09-11）：`Recent` 的變數名字與預設值都不改。** 下面這張表留著，是因為
`0x16` 的數字要跟它對齊（一包幾則、一窗幾則、誰夾誰），而**照名字猜會猜錯** —— 這一輪就猜錯過一次。

**先擺事實**（值與行為都從程式碼讀出來，不是照名字猜）：

| 現在的名字 | 預設 | **實際上管什麼**（程式碼） | 誰在用 |
|---|---|---|---|
| `wbf_recent_default_limit` | **320** | **一窗幾則**：`Recent` 的 meta 沒帶 `limit` 時用它 | `RecentRequest::parse` |
| `wbf_recent_max_limit` | **500** | **一窗的上限**：client 帶的 `limit` 夾到這裡 | 同上；`Subscribe{cg_seq}` 的補窗直接用這個值 |
| `wbf_recent_default_batch` | **10** | **一包幾則**：meta 沒帶 `batch` 時用它 | `RecentRequest::parse` |
| `wbf_recent_max_batch` | **100** | **一包則數的上限**：client 帶的 `batch` 夾到這裡（`.max(1)`，0 會變 1） | 同上 |
| `wbf_push_max_events_per_pack` | **10** | **一個 `Event/Push` 幾則**：live 推送本來就一則；它實際上界的是 `Subscribe{cg_seq}` 補窗那一輪 | `push_window` |
| `wbf_ws_send_queue_len` | **32** | **一條連線的出站佇列裝幾個 pack**（數的是 pack，不是則） | `serve` |
| `wbf_data_max_bytes` | **16 MiB + 4096** | 一個 pack 的 data 上限；所有切包都同時受它 | `list_pack_ranges` |

📎 **包數從來不是旋鈕**：它是 `ceil(limit ÷ 每包則數)` 算出來的（預設 320 ÷ 10 = 32 = 佇列剛好滿）。
維護者 2026-09-11 問過要不要加一個「一次最多幾包」的上限（例如 50）——**不加**，理由兩條：
(1) 一窗則數、一包則數、包數三個數字只有**兩個自由度**，三個都能調就要回答「衝突時誰讓步」；
(2) 不需要擋 —— client 送 `limit=500, batch=1`（500 包）時，佇列滿了 handler 就卡在 `Reply::send` 等它讀，
那正是 [wbf-pack-pipeline.md](wbf-pack-pipeline.md) §1 設計好的背壓。

**討論過但不改的**（維護者 2026-09-11 定）：

| 想過要改的 | 為什麼有人會想改 | 為什麼不改 |
|---|---|---|
| `wbf_push_max_events_per_pack` → `wbf_event_push_…` | 讀起來像管「所有推送」 | `0x16` 的推送**就是共用它**，所以這個名字現在是準的 —— 它管的正是「server 主動推的一包幾則」，不分 kind |
| `wbf_recent_default_batch` ／ `_max_batch` → `…events_per_pack` | `Batch` 在這個協議裡是一個 pack 的名字，`batch = 10` 會被讀成「10 個包」 | 改名要動 config、`Hello` 的回應欄位（線上可見）、文件與 e2e，換來的只是可讀性 —— **維護者定：先不動**。`0x16` 這邊改用 `wbf_device_default_batch`，**跟現有的家族對齊**，一致比各自漂亮更值錢 |

⚠️ 所以 `batch` 這個字在這份協議裡固定是「**一包幾則**」，不是包數。🚫 之後新增旋鈕沿用這個用法，
不要在同一個 repo 裡出現第二種讀法。

📎 **一則有多大**（照 [wbf-pack-pipeline.md](wbf-pack-pipeline.md) §1 的規矩，把記憶體算出來）：
to-device 的一則**不是訊息事件**，是一小段 olm 密文 ——
`{ algorithm, sender_key, ciphertext: { <收件端 key>: { type, body } } }`，
裡面最大的東西是 Megolm 的 session key（ratchet 四段 32 byte ＋ 簽章公鑰，raw 約 240 byte），
olm 封裝再 base64 之後**一則大約 1 KB**；SAS 驗證與 `m.secret.send` 更小（幾百 byte）。

| | 大小（估算） |
|---|---|
| 一則 | ~1 KB |
| 一包（100 則） | ~100 KB |
| 一窗（1000 則） | ~1 MB |

⚠️ **這是估算，不是量測** —— 實作那支要量一次真實數據再回來改這裡。
📎 一包 ~100 KB **遠低於 `wbf_data_max_bytes`（16 MiB）**，所以實際切包的是 100 這個則數；
byte 上限仍然要接（規則只有一份，[wbf-wire-format.md](wbf-wire-format.md) §2.1 那條教訓），只是幾乎不會觸發。
理論上界仍是 `wbf_ws_send_queue_len` × `wbf_data_max_bytes`，跟其他 kind 同一條，不是這裡新增的風險。
🔲 這幾個數字**先這樣定**（維護者 2026-09-11），量過再調。

## 8. client 端會怎麼用（給讀 server 的人理解脈絡）

```
daemon 啟動、Login → Subscribe{device_id, cd_seq: 上次存的}   ← 先登記，再補洞
                  → Device/Fetch(cd_seq)                    ← 離線期間漏的，一窗 = 10 個 Batch
每收到一個 Batch／Push（100 則）
                  → 逐則匯進 crypto store（OlmMachine::receive_sync_changes）
                  → ItemsDestroy{ 這一包裡成功的那些 count }   ← 一包一個呼叫，不等整窗
                  → 收到 ItemsDestroyed：在清單裡的，本地是唯一真相
一窗收完（r = 0）還有更舊的 → 帶上一窗的 nt 當 cd_seq 再叫一次
之後靠 Push；收到 gap 就再 Fetch 一次
```

📎 **server 對 to-device 的內容本來就是瞎的**（`add_to_device_event` 只存 `type`／`sender`／`content`），
這個提案不改變那件事。
📎 client 完成之後照自己的 config 把金鑰備份回 server，走的是 Matrix 既有的 key backup（`0x17 Keys` 那一格），
**不在本提案範圍**。

## 9. 跟 client 原提案不同的地方（合併後要在 client repo 開同步 issue）

| client 原提案 | 這裡定的 | 為什麼 |
|---|---|---|
| `0x03 Ack { until }`，前綴刪除 | **`0x03 ItemsDestroy { tc } + data`**，明確列出每一則的 count | 維護者 2026-09-11：刪除是命令不是回執；前綴會把「我處理到哪」與「可以刪哪些」綁成一件事 |
| `Ack` 之後就算刪了 | **`Ack` 只表示收到命令**，結果另外由 `ItemsDestroyed` 帶回 | 同上；命令要有回應，回應要講結果 |
| `oldest` / `newest` | **`ot` / `nt`** | 維護者 2026-09-10；短名跟線上其他欄位（`bc`、`tc`、`fs`、`ls`、`r`）一致，而且仍然跟 `fs`／`ls` 長得不一樣 |
| 保留期待定 | **無窮 TTL** ＋ 刪裝置清佇列 ＋ admin 可觀測 | §6 |
| `Fetch { to }` | **拿掉** | §3.1.1：它對到的是 `cg_seq` 的位置不是 `before`，而唯一想得到的情境不需要它（維護者 2026-09-11） |

## 10. 實作那支要確認或量的

設計上沒有未決項了（2026-09-11）。動手時這三件要有答案，不要當成已經成立 —— **PR #43 查過了，結論如下**：

1. ✅ **`remove_device` 本來就清 to-device 佇列**（前綴刪除）—— 無窮 TTL 唯一的自動回收路徑存在，不用補。
2. ✅ **佇列大小看得見了**：`!admin user to-device-queue <user>`。⚠️ 本來要印「最舊那筆的時間」，
   查了 `next_count` 才發現**它是純遞增計數器、不帶時鐘** —— 那行會直接在 admin 輸出上說謊，
   所以只印 count 並註明「低的代表較早，這裡不說是什麼時候」。
3. 🔲 **一則到底多大：還是估算，沒量。** `limit`／`batch` 照提案的 1000／100 先定。
   量了之後回來改這裡的數字 —— ⭐ 但 §7 的**關係**（一窗的包數要放得進發送佇列）現在由
   `config/check.rs` 的 `check_wbf_device_window` 在啟動時擋著，所以調錯不會變成執行期的怪現象。
