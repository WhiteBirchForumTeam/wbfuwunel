# Draft Message（草稿訊息）設計草案，第四版

> **狀態：📄 草案，等維護者同意。** 第四版依維護者 2026-09-08 的設計重寫：**草稿一開始就是一則真的、持久化的佔位訊息**，它的 `g_seq` 就是 `draft_id`；
> 之後的內容變化（`Keypoint`／`Delta`／`Append`）只廣播、不進庫；中途進來的人用 `Demand` 向作者要全文；`Abandon` 就是 redact 佔位訊息；
> 定案是一則正常訊息取代它。走 [wbf-event-push.md](wbf-event-push.md) 的 channel（工作 2），這份是工作 3。
> 第三版（暫時 id、server 零狀態、沒有錨）作廢：它的問題是中途進來的人對不回草稿，而且 `draft_id` 的作用域要靠 `(sender, device)` 拼。第四版用一則真事件當錨，全部解掉。

## 1. 這是什麼、不是什麼

「草稿訊息」是一則**還在長**的訊息：LLM 逐字吐、長訊息分段打。在線的人看著它變，講完了它變成一則正常訊息。

它跟 Matrix 的 `m.replace`（edit）不同：edit 每改一次一個永久事件；草稿的變化一個都不進歷史。歷史最後只有兩筆：被 redact 的佔位、正式訊息。
它跟分塊上傳只共用外框：草稿沒有「塊」，長文字靠 `Keypoint` ＋ `Append` 拼（§5）。

## 2. 三個規則

1. **錨是真事件**：`Draft` 讓 server 寫一則明文的 service 事件（§3.1），它有 `event_id`、有 `g_seq`；**`draft_id = g_seq`**。後加入者從 `Recent`／`Push` 看得到錨，知道「這裡有一則草稿」。
2. **變化是暫態**：`Keypoint`／`Delta`／`Append` 只經 channel 廣播給訂閱者，server 不存、不重送；掉了用 `Demand` 要一次 `Keypoint`。
3. **收尾是正常訊息**：定案 = 一則正常的 `Event/Send` 帶 `draft_id`，server 照常 append、**並 redact 佔位訊息**；撤回 = `Abandon` = redact 佔位訊息。草稿到此結束。

**server 對草稿仍是零記憶體狀態**：真相只有那則佔位事件（DB）；誰能發什麼，每次從它讀。

## 3. pack

kind `0x02 Stream`。**所有 subtype 的 meta 都是明文的 `room_id` 字串本身**（UTF-8，不是 JSON；維護者 2026-09-08：`g_seq` 已經有 header `id` 那格，meta 只剩房間）；
data 是密文（E2EE 房）或明文，server 不讀。server 轉發時 **pack 原樣送**，不加 `sender`／`device`：接收者用 `g_seq` 找到佔位事件，作者就在它的 `sender` 裡；哪台裝置在寫，接收者不需要知道。

header：`id`（8 byte）填 `g_seq`（`Draft` 填 0），它就是「這個 pack 屬於哪則草稿」。
**`seq`（4 byte）= 作者對這則草稿遞增的片計數**（`Keypoint`／`Delta`／`Append`；同一個 `g_seq` 下永遠遞增，維護者 2026-09-08）。
順序本身由 TCP 保，`seq` 不是拿來排序的，是拿來**發現洞**的：片會掉的地方不是 TCP，是 server 對某一個接收者的發送佇列滿了 `try_send` 丟掉（作者與其他人都不知道），
沒有計數的話接收者少一段 `Append`、或把 `Delta` 套在錯的基底上，都是默默壞掉。接收者看到跳號就 `Demand`。server 不看 `seq`。
`Draft`／`Abandon`／`Demand` 的 `seq` 照無序類規則當請求號，Ack 抄回。

| subtype | 誰發 | 進庫？ | data |
|---|---|---|---|
| `0x01 Draft` | 作者 | **是**：佔位事件 | 無。回 `Ack` `{ "event_id", "g_seq" }` |
| `0x02 Abandon` | 作者 | **是**：redact 佔位事件 | 無。回 `Ack` `{ "redaction_event_id" }`；訂閱者從 `Push` 收到 redaction |
| `0x03 Keypoint` | 作者 | 否 | **完整字段**：接收者把這則草稿的 buffer 整個清空換成它。明文上限 **8 KiB**（client 約定），server 對 data 的 hard limit **10 KiB**（密文，含加密外框） |
| `0x04 Delta` | 作者 | 否 | 相對於接收者目前狀態的差異（格式是 client 約定，§5） |
| `0x05 Append` | 作者 | 否 | 直接接在末尾的文字 |
| `0x10 Demand` | **任何成員** | 否 | 無。跟其他片一樣**廣播給房間 channel 的每個訂閱者**（維護者：不擋、不特別路由，更乾淨）；正在寫這則草稿的那台裝置回一次 `Keypoint`（太長就再跟 `Append`），其他人收到 `Demand` 就忽略 |

- **沒有 `Chunk`**（維護者 2026-09-08：多餘）。長於 8 KiB 的草稿 = 一個 `Keypoint`（前 8 KiB）＋若干個 `Append`；接收者的 buffer 先被削成 8 KiB、再長回來。
  這跟塊等效，少一個型別、少一套塊索引與 join 規則。
- **只走 WS**；HTTP 一律 `Unsupported`。
- **廣播對象**：房間 channel 裡的所有訂閱者，**不含發送這片的那條連線**（作者的其他裝置照收——它們是 Demand 的收件人「作者帳號組」，本來就要跟得上）。
- **`Keypoint`／`Delta`／`Append` 不回 Ack**：盡快語意。`Draft`／`Abandon` 回 Ack 是因為它們寫了庫。`Demand` 不回 Ack：要的東西是之後的 `Keypoint`，等不到是 client 的 timeout。
- **接收端可以把草稿的目前內容寫進自己的資料庫**（維護者）：UI 收到 Stream pack 就更新佔位訊息的顯示，要落地也無妨——它是揮發的，下次再收到就再更新，server 是權威。server 這邊仍然不存。

### 3.1 佔位事件長什麼樣

明文、自訂 type、內容幾乎是空的：

```json
{ "type": "org.wbftw.wbfuwunel.draft", "content": { "msgtype": "org.wbftw.wbfuwunel.draft", "body": "(draft)" }, "sender": "@a:…", … }
```

- **不用 `m.room.message`**：E2EE 房間裡一則明文 `m.room.message` 會讓相容 client 顯示「未加密」警告；自訂 type 它們直接不顯示。
- 沒有機密：裡面只有「這裡有一則草稿」。`body` 給不認識的工具看。
- 走既有的 `send_message_event`（同一個 txn 冪等、同一個 append），所以有 `r_seq`／`g_seq`、會 `Push`、`Recent` 拿得到。
- **一則草稿一則佔位事件**，作者同時開幾則草稿就有幾個錨，server 不限。

### 3.2 定案

`Event/Send` 的 meta 多帶 `"draft_id": <g_seq>`。server：

1. 從 `(room_id, g_seq)` 讀佔位事件；`sender` 必須是這條連線的使用者、type 必須是 `org.wbftw.wbfuwunel.draft`、沒被 redact → 否則 `Conflict`。
2. 照常 append 正式訊息，`unsigned["org.wbftw.wbfuwunel.draft_id"] = g_seq`（跟 `r_seq`／`g_seq` 同一個位置，不進雜湊、不進聯邦）。
3. redact 佔位事件（作者自己的事件，權限一定夠）。

接收者從 `Push` 收到正式訊息，看 `unsigned` 的 `draft_id` 把草稿的顯示換掉；redaction 也會 `Push` 來，佔位從歷史消失。
沒帶 `draft_id` 的 `Event/Send` 完全不受影響。

## 4. 誰能發什麼：每次從錨讀

```
Stream pack 進來（已登入、准入表過、meta 是合法的 room_id、header id = g_seq）
   ├─ 從 pduid_pdu 讀 (room_id 的短號, g_seq)                    ← 一次點讀；沒有 → Error(NotFound)
   ├─ 它是 org.wbftw.wbfuwunel.draft、沒被 redact                            ← 否則 Error(Conflict "not an open draft")
   ├─ Keypoint／Delta／Append／Abandon：sender == 這條連線的 user         ← 否則 Error(Forbidden "not the author")
   ├─ Demand：這條連線的 user 是 room_id 的成員                      ← 否則 Error(Forbidden)
   ├─ 限速（§7）、大小（§7）
   └─ 廣播：channels::relay(room_id, 除發送連線外, 原 pack)——Demand 也一樣，沒有特別路由；正在寫這則草稿的那台裝置回 Keypoint，其他人忽略
```

沒有 `g_seq → 事件` 的全站索引，所以 **`room_id` 是必填**：事件的 key 是 `(房間, g_seq)`，帶了 `room_id` 就是一次點讀。這是每片一次 DB 讀，
跟限速同量級（30 片／秒），不做快取——快取就是 server 的草稿狀態，第三版拿掉的東西不放回來。

## 5. Keypoint／Delta／Append：server 不懂內容

三種都是密文（E2EE 房），server 只轉發、只看 header 與明文 meta。語意是 client 之間的約定：

- **`Keypoint`**：完整字段，明文 ≤ 8 KiB。接收者把這則草稿的 buffer **整個清空**換成它。草稿比 8 KiB 長時，作者送 `Keypoint`（前 8 KiB）再 `Append` 其餘：
  例如 12 KiB 的草稿，有人 `Demand`，所有人的 buffer 先被削成 8 KiB、接著收到 4 KiB 的 `Append` 回到 12 KiB。這就是「塊」，不需要另一個型別。
- **`Delta`**：相對於接收者目前狀態的差異；接收者只在「有狀態」時套（收過 `Keypoint`，或從空字串開始且沒漏過片），不然等下一個 `Keypoint`（或自己 `Demand`）。
- **`Append`**：接在末尾。最常見的 LLM 情境，一片就是幾個 token。
- **片計數 `seq`**：作者對每則草稿遞增；接收者看到跳號就把這則草稿標成「等全文」並 `Demand`，收到 `Keypoint` 才繼續套。server 不看它。作者換連線續發：計數接著送，或先送一個 `Keypoint` 對齊。
- **第一片**可以是 `Keypoint`、也可以是從空字串起的 `Delta`／`Append`——都允許，client 決定。
- **delta 的內部格式**（密文裡）定在 client 的約定，server spec 只定「有這三種」與它們的語意。

### 5.1 中途進來的人（維護者的例子）

房間 A、B、C、D。A 送 `Draft` → 佔位事件 `g_seq = 123`，`Push` 給 B、C（D 不在線）。A 不斷 `Append`／`Delta`，B、C 跟著改。
D 上線、`Subscribe`、從 `Recent` 或 `Push` 看到 123 是一則草稿、接著收到 `Delta` 但沒有狀態 → D 送 `Demand(id = 123)`。
server 讀 123 → 作者是 A → 只送給 A 訂閱中的連線。A 廣播 `Keypoint(id = 123)`（超過 8 KiB 就再接幾個 `Append`）。B、C 也收到，等於重新對齊一次。
之後的 `Delta`／`Append` D 就跟得上。

## 6. 跟 channel 共用什麼

| 共用 | 在哪 |
|---|---|
| 訂閱 registry、`try_send`、掉了就掉、不含發送連線的廣播 | `Services.channels`（[wbf-event-push.md](wbf-event-push.md) §3）：`relay(room, except_connection, pack)`，六個 subtype 都只用這一個 |
| 發送佇列與發送 task | pipeline §1 |
| 佔位事件的寫入、redact、`Push` | 既有的 `send_message_event`／`redact` 路徑，什麼都不加 |

## 7. 上限（config）

| 名字 | 預設 | 管什麼 |
|---|---|---|
| `wbf_draft_pieces_per_second`／`wbf_draft_pieces_burst` | 30／60 | 每 (user, device) 的 `Keypoint`／`Delta`／`Append` 頻率（`IpTokenBuckets` 同款，key 換成 device） |
| `wbf_draft_max_piece_bytes` | 10240 | 一片的 data 上限（密文；維護者定：明文 8 KiB 是 client 約定，server 寬限到 10 KiB）；三種片同一個上限 |
| `wbf_draft_demands_per_second` | 1（突發 3） | 每 (user, device) 的 `Demand` 頻率：它會讓作者重送全文，要比片慢得多 |

| `wbf_draft_max_room_members` | 10 | **房間加入人數超過這個就不准 `Draft`**（`Conflict "room too large for drafts"`）。維護者：大房間的扇出是 server 不夠有錢的問題不是設計問題，這是 fallback；草稿是給 bot 用的，人類用不上 |

沒有「每房幾則草稿」的上限：錨是普通事件，受既有的送訊息限速。每則草稿三筆持久事件（佔位、正式、redaction）：給 bot 用、已經比 edit 划算很多，不是擔憂。

## 8. 驗收（e2e11，接在 channel 的情境後面）

- alice、bob 訂閱；alice `Draft` → Ack 有 `event_id`、`g_seq`；bob 收到 `Push`，事件 type 是 `org.wbftw.wbfuwunel.draft`；alice 自己也 `Push` 到。
- alice `Append`、`Delta`、`Keypoint`（`seq` 全填 0）→ bob 依序收到三個，pack 原樣（meta 是 room id、data 一個 byte 不差）；alice 發送那條連線**沒有**收到自己的；alice 的第二條訂閱連線收到。
- `Keypoint` data 10241 bytes → `TooLarge`；10240 → 過。meta 不是合法 room id → `Conflict`。
- carol 沒訂閱 → 什麼都收不到；carol 訂閱後送 `Demand(id)` → alice 與 bob 都收到（廣播）、carol 自己那條沒收到；alice 回 `Keypoint` → carol、bob 都收到。
- 片計數：alice 送 `seq` 5、6、8 → bob 在 8 看到跳號（e2e 只驗它原樣到達；跳號→Demand 是 client 的事）。
- 房間 11 人（`wbf_draft_max_room_members` 預設 10）→ `Draft` 回 `Conflict`；設成 0 → 不限。
- bob（非作者）送 `Append(id)` → `Forbidden`；不存在的 `id` → `NotFound`；HTTP → `Unsupported`；連送 100 片 → 一部分 `RateLimited`。
- 定案：alice `Event/Send` 帶 `draft_id` → bob 收到兩個 `Push`：正式訊息（`unsigned.draft_id` = id）與佔位的 redaction；之後對 id 送 `Append` → `Conflict`。
- `Abandon` → Ack、bob 收到 redaction 的 `Push`；`Recent` 裡佔位事件已 redact。
- bob 停止讀 socket、alice 送 50 片 → alice 一片都沒被擋、bob 恢復後收到的是後面的片。
- 關機中連線收到 Close 1001。

## 9. 維護者 2026-09-08 定的（原開放問題）

1. **定案用 redact**：正式版直接再發一則新訊息，佔位直接剥除；不用 `m.replace`。
2. **佔位不帶 device**：`Demand` 是去跟發文者的**帳號**要，他的每台裝置都收到，正在寫草稿的那台才回。
3. **佔位永不過期**：沒有 TTL，永遠可用。
4. **meta 不用 JSON**：直接放 `room_id`，`g_seq` 在 header `id`。
5. **接收端可以落地**草稿內容（§3），server 是權威。

自我審查後維護者再定的（同日）：

6. **`seq` 收回來當片計數**（§3）：片會在 server 的發送佇列被丟，沒計數接收者不知道有洞。
7. **`Demand` 不特別路由**，跟其他片一樣廣播全房；寫草稿的那台回，其他人忽略。
8. **`wbf_draft_max_room_members`**（預設 10）：大房間不准開草稿。
9. **`Demand` 風暴**（很多人同時進來、作者一直重傳）：只在超過 8 KiB 的草稿才痛，client 端加重傳間隔門檻（或禁用重傳）就能解；情境少，先不考慮。
10. **定案的順序**：先 append 正式訊息、再 redact 佔位。redact 那步失敗（極少）會留下「正式在、佔位仍開著」，那個佔位仍是 open draft，client 之後 `Abandon` 它就好。
11. **registry 的鎖**是實作細節（`RwLock`，`publish` 拿讀鎖），不進協議。

剩下的小事：`g_seq` 的型別——`PduCount` 是有號的（backfill 的事件為負），但佔位事件是本站新寫的、永遠正，塔進 u64 的 header `id` 沒問題；server 對讀不到的回 `NotFound`。

## 10. 這份文件的查證範圍

- `src/api/client/wbf/{mod,ws,send}.rs`（PR #33）：發送佇列、准入表、`Event/Send` 的 meta 解析與 `send_message_event`。
- `src/database/maps.rs`、`src/service/rooms/timeline/`：`pduid_pdu` 的 key 是 `(shortroomid, count)`，`count` 就是 `g_seq`，所以 `(room_id, g_seq)` 是點讀；沒有全站 `g_seq` 索引（room-seq-and-recent §2.2）。
- `src/api/client/redact.rs`：redact 自己的事件的既有路徑。
- 沒實測：每片一次點讀在 30 片／秒下的成本（預期可忽略，RocksDB 點讀微秒級）；大房間廣播的成本（跟 `Push` 同一題）。
