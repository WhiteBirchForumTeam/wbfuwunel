# Draft Message（草稿訊息）設計草案，第四版

> **狀態：📄 草案，等維護者同意。** 第四版依維護者 2026-09-08 的設計重寫：**草稿一開始就是一則真的、持久化的佔位訊息**，它的 `g_seq` 就是 `draft_id`；
> 之後的內容變化（`Keypoint`／`Delta`／`Append`）只廣播、不進庫；中途進來的人用 `Demand` 向作者要全文；`Abandon` 就是 redact 佔位訊息；
> 收尾是 client 的事：自己 `Abandon`、再自己送一則正常訊息，server 不做多餘的事。走 [wbf-event-push.md](wbf-event-push.md) 的 channel（工作 2），這份是工作 3。
> 第三版（暫時 id、server 零狀態、沒有錨）作廢：它的問題是中途進來的人對不回草稿，而且 `draft_id` 的作用域要靠 `(sender, device)` 拼。第四版用一則真事件當錨，全部解掉。

## 1. 這是什麼、不是什麼

「草稿訊息」是一則**還在長**的訊息：LLM 逐字吐、長訊息分段打。在線的人看著它變，講完了它變成一則正常訊息。

它跟 Matrix 的 `m.replace`（edit）不同：edit 每改一次一個永久事件；草稿的變化一個都不進歷史。歷史最後只有兩筆：被 redact 的佔位、正式訊息。
它跟分塊上傳只共用外框：草稿沒有「塊」，長文字靠 `Keypoint` ＋ `Append` 拼（§5）。

## 2. 三個規則

1. **錨是真事件**：`Draft` 讓 server 寫一則明文的 service 事件（§3.1），它有 `event_id`、有 `g_seq`；**`draft_id = g_seq`**。後加入者從 `Recent`／`Push` 看得到錨，知道「這裡有一則草稿」。
2. **變化是暫態**：`Keypoint`／`Delta`／`Append` 只經 channel 廣播給訂閱者，server 不存、不重送；掉了用 `Demand` 要一次 `Keypoint`。
3. **收尾是 client 的事**（維護者 2026-09-08）：真的要收掉草稿時，client **自己送 `Abandon`**（redact 佔位），然後**自己再送一則正常訊息**。
   server 對這兩個 pack 照常處理就好，不做任何多餘的事：`Event/Send` 完全不動，沒有 `draft_id` 欄、沒有 `unsigned` 裡的對應、沒有順手 redact。正式訊息要不要對回草稿，是 client 在密文內容裡自己約定。

**server 對草稿仍是零記憶體狀態**：真相只有那則佔位事件（DB）；誰能發什麼，每次從它讀。

## 3. pack

kind `0x02 Stream`。**所有 subtype 的 meta 都是明文的 `room_id` 字串本身**（UTF-8，不是 JSON；維護者 2026-09-08：`g_seq` 已經有 header `id` 那格，meta 只剩房間）；
data 是密文（E2EE 房）或明文，server 不讀。server 轉發時 **pack 原樣送**，不加 `sender`／`device`：接收者用 `g_seq` 找到佔位事件，作者就在它的 `sender` 裡；哪台裝置在寫，接收者不需要知道。

header：`id`（8 byte）填 `g_seq`（`Draft` 填 0），它就是「這個 pack 屬於哪則草稿」。
**`seq`（4 byte）= 作者對這則草稿遞增的片計數**（`Keypoint`／`Delta`／`Append`；同一個 `g_seq` 下永遠遞增，維護者 2026-09-08）。
順序本身由 TCP 保，`seq` 不是拿來排序的，是拿來**發現洞**的：片會掉的地方不是 TCP，是 server 對某一個接收者的發送佇列滿了 `try_send` 丟掉（作者與其他人都不知道），
沒有計數的話接收者少一段 `Append`、或把 `Delta` 套在錯的基底上，都是默默壞掉。接收者看到跳號就 `Demand`。server 不看 `seq` 的值。
`Draft`／`Abandon`／`Demand` 的 `seq` 照無序類規則當請求號，Ack 抄回。

### 3.0 片的 data 前 4 個 byte 是**它接在誰後面**（維護者 2026-09-12）

```
片的 data ＝ [prev: u32 大端] ‖ 密文
```

⭐ **`prev` 指向這片所依據的那一片的 `seq`** —— 「這個 `Append` 接在第 11 片之後」。有了它，接收者在**套用之前**就能判斷自己對不對得上，而不是靠「號碼應該要剛好大 1」這個算術推論：

```
Keypoint  seq=1  prev=0    "Hello"        ← 0 ＝ 沒有基底，它自己就是起點
Append    seq=2  prev=1    " world"
Append    seq=3  prev=2    "!"
```

- **`seq` 從 1 起算**，`0` 永遠保留給 `prev` 的「沒有基底」。🚫 兩者不能共用 `0` —— 那樣「接在第 0 片之後」跟「沒有基底」在線上長得一樣，正是 [wbf-wire-format.md](wbf-wire-format.md) §3.4 那條「佔位值不能跟真值撞」的同一個坑。
  ⚠️ 這跟 §4.1 講的「新會話 `seq` 歸零」不衝突：那條講的是 **server 自己的推送計數**（`Push`／`Batch`），這裡的片計數是**作者填的**。
- **`Keypoint` 的 `prev` 必須是 0**：它把 buffer 整個換掉，指向誰都沒有意義。
- 中途加入的人手上沒有狀態，所以收到 `Delta`／`Append` 時不論 `prev` 是什麼都對不上 → `Demand`。
  ⚠️ **但 `Keypoint` 是例外**：它帶的就是完整內容（`prev=0`），所以中途加入的人**直接拿它建立狀態就好，不要再 `Demand`** ——
  否則兩個新來的人會互相觸發對方的重傳（外部審查 2026-09-12）。
- 📎 **掉了最後一片就是掉了**（維護者 2026-09-12）：串流沒有「寫完了」這個可觀測事件 —— 掉最後一片跟作者停下來打字在線上一模一樣，連作者自己都不知道自己是不是寫完了。而真正的內容最後會走**正式訊息**那條路（持久事件、`g_seq` 水位、`Recent` 補洞），所以最壞只是預覽停在倒數第二片，直到那則訊息蓋過去。🚫 **不為此加任何 server 端的補洞機制**。
- server **不讀密文、也不追 `prev` 對不對**（它對草稿零狀態，無從比對）。
  📎 包括**不驗 `prev` 指到的那一片存不存在**（審查者 rumia）：`prev=9999` 這種丟下去照轉不誤。
  ⭐ 那不是破洞，是同一個設計的尾端：接收端本來就要把 `prev` 跟**自己手上的狀態**比，比不上就 `Demand`。它只做三件無狀態的檢查，好讓這個約定在線上真的成立而不只是紙上規則：
  | 檢查 | 不合 |
  |---|---|
  | 片的 `data` 至少 4 byte | `InvalidRequest` |
  | 片的 `seq ≥ 1` | `InvalidRequest` |
  | `Keypoint` 的 `prev` ＝ 0 | `InvalidRequest` |

| subtype | 誰發 | 進庫？ | data |
|---|---|---|---|
| `0x01 Draft` | 作者 | **是**：佔位事件 | 無。回 `Ack` `{ "event_id", "g_seq" }` |
| `0x02 Abandon` | 作者 | **是**：redact 佔位事件 | 無。回 `Ack` `{ "redaction_event_id" }`；訂閱者從 `Push` 收到 redaction |
| `0x03 Keypoint` | 作者 | 否 | `prev`(=0) ＋**完整字段**：接收者把這則草稿的 buffer 整個清空換成它。明文上限 **8 KiB**（client 約定），server 對 data 的 hard limit **10 KiB**（密文，含 4 byte 的 `prev` 與加密外框） |
| `0x04 Delta` | 作者 | 否 | `prev` ＋相對於 `prev` 那個狀態的差異（格式是 client 約定，§5） |
| `0x05 Append` | 作者 | 否 | `prev` ＋直接接在 `prev` 那個狀態末尾的文字 |
| `0x10 Demand` | **任何成員** | 否 | 無。跟其他片一樣**廣播給房間 channel 的每個訂閱者**（維護者：不擋、不特別路由，更乾淨）；正在寫這則草稿的那台裝置回一次 `Keypoint`（太長就再跟 `Append`），其他人收到 `Demand` 就忽略 |

- **沒有 `Chunk`**（維護者 2026-09-08：多餘）。長於 8 KiB 的草稿 = 一個 `Keypoint`（前 8 KiB）＋若干個 `Append`；接收者的 buffer 先被削成 8 KiB、再長回來。
  這跟塊等效，少一個型別、少一套塊索引與 join 規則。
- **只走 WS**；HTTP 一律 `Unsupported`。
- **廣播對象**：房間 channel 裡的**所有**訂閱者，**含發送這片的那條連線**（維護者 2026-09-08：不擋，全域廣播；作者自己也收到自己寫的，client 自己辨）。
- **接收端只認作者，而這由 server 保證**：`Keypoint`／`Delta`／`Append` 進來時 server 用佔位事件的 `sender` 驗作者，不是作者直接 `Forbidden`、不轉發（§4）；
  所以接收者拿到的片一定是作者的，不用再驗。`Demand` 誰都能發，但它不帶內容，偽造不了什麼。若之後要在 meta 放 server 確認過的 sender，meta 可以改 JSON（維護者允許）。
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
  📎 `Draft` 沒有 `txn_id` 欄位，server 用 `(連線號, 請求 seq)` 組一個。⚠️ 所以**同一條連線把同一個請求 `seq` 用第二次＝重放**：拿回的是**第一次那則錨**（可能已經被 `Abandon` 掉），不是一則新草稿 —— 這就是 Matrix 交易 id 的既有語意（審查者 rumia，PR #45）。無序類的 `seq` 本來就由發送端保證不重複（§4），照著做就不會遇到。
- **一則草稿一則佔位事件**，作者同時開幾則草稿就有幾個錨，server 不限。

### 3.2 收尾（client 的流程，server 沒有角色）

1. client 送 `Abandon`：server redact 佔位事件，訂閱者從 `Push` 收到 redaction，佔位從歷史消失。
2. client 送一則正常的 `Event/Send`（或任何送訊息的路徑）：就是一則正常訊息，照常 append、照常 `Push`。

兩步之間與之後 server 什麼都不記、不驗、不補。接收端看到 redaction 就把草稿顯示收掉，看到新訊息就顯示新訊息；要把兩者連起來（例如正式訊息的密文內容帶上草稿的 `g_seq`）是 client 約定。
順序由 client 定；先送正式訊息再 `Abandon` 也行。

## 4. 誰能發什麼：每次從錨讀

🚨 **順序本身是設計的一部分**（外部審查 2026-09-12，R7）：**成員資格在讀錨之前**。反過來的話，
`NotFound`／`Conflict`／`Forbidden` 三種回應的差別，就讓一個沒進房的人問得出「那個 `(房間, g_seq)`
存不存在、是不是一則還開著的草稿」—— 在一個他讀不到的房間裡。

```
Stream pack 進來（已登入、准入表過）
   ├─ meta 是合法的 room_id                                  ← 否則 Error(InvalidRequest)
   ├─ flags == 0                                            ← 否則 Error(InvalidRequest)（§3.0）
   ├─ 宣告無 data 的 subtype（Draft／Abandon／Demand）帶了 > 1 KiB  ← Error(InvalidRequest) 並關閉連線
   ├─ 片：大小 ≤ wbf_draft_max_piece_bytes、data ≥ 4 byte、seq ≥ 1、Keypoint 的 prev = 0
   ├─ 🚨 這條連線的 user 是 room_id 的成員                      ← 否則 Error(Forbidden)。在讀錨之前
   ├─ 從 pduid_pdu 讀 (room_id 的短號, g_seq)                  ← 一次點讀；沒有 → Error(NotFound)
   ├─ 它是 org.wbftw.wbfuwunel.draft、沒被 redact              ← 否則 Error(Conflict "not an open draft")
   ├─ Keypoint／Delta／Append／Abandon：sender == 這條連線的 user  ← 否則 Error(Forbidden "not the author")
   ├─ 片：這個帳號**現在**還能不能發言                            ← 見 §4.1；否則 Error(Forbidden)
   ├─ 限速（§7）
   └─ 廣播：Services.streams.relay_to(room_id, 沒有 ignore 作者的收件人, pack)
        · 全房含發送連線；Demand 也一樣廣播，不特別路由
        · flags 一律 0；Demand 的 data 一律剝掉
```

### 4.1 錨是「持續的許可」，所以每片都要重問

⚠️ 一則開著的草稿**不會過期**，所以「當初能開稿」不等於「現在還能發」。外部審查（2026-09-12，R2／R4）
指出兩條會被繞過的政策，兩條都補在片的路徑上：

| 政策 | 為什麼非問不可 |
|---|---|
| **帳號被停權**（`is_suspended`） | 一般發訊明確拒絕停權帳號的非 redaction 事件；不問的話，停權者用一則永遠存在的錨繼續對全房輸出 |
| **房間撤掉他的發言權**（power level） | 「禁言」若擋不住草稿，那個禁言就是假的。問的是房間本來就知道的問題：**這個 user 現在還能不能送錨那個 type 的事件** |

📎 **那個查詢實際量到的是什麼**（審查者 rumia）：錨的 type 是**保留型別**（見下），沒有人能從一般路徑送它，
所以 `user_can_send_message` 量的其實是這個 user 對房間 `events_default` 的權限（除非有人在 `events` 表裡
特別為這個 type 建了一條）—— 也就是「這個人現在在這個房間講不講得了話」。⭐ 那正是我們要問的問題，
但它跟字面上的「能不能送這個 type」不完全是同一件事，寫在這裡免得下一個讀的人以為 server 在授權一個已經無路可走的 type。

⭐ **`Abandon` 故意不走這個閘門**：撤回自己的東西是停權帳號**仍然有**的權利（既有的 redaction 政策
本來就這樣定），把它一起擋掉會讓被停權的人連收拾自己的草稿都做不到。

⭐ **`Demand` 也不走這個閘門，而且是刻意的**（審查者 rumia 問過）：停權與禁言禁的是**寫**，不是**讀**。
`Demand` 不搬任何內容進房間（它的 data 還會被剝掉，§4），它只是說「我跟不上了」—— 而一個看得到這個房間的人
本來就該能把畫面追上。⚠️ 它確實會讓作者重傳一次，所以擋住「一直問」的是**節流**（1/s、突發 3，§7），
不是發言權。

🚨 **錨只能由 `Stream/Draft` 寫**（R6）：`org.wbftw.wbfuwunel.draft` 是**保留的事件型別**，一般
`Event/Send` 與 HTTP `/send` 一律 `Forbidden`。否則人數上限只綁得住**自願**走 `Draft` 的 client ——
自己送一則同型別事件就能拿到一個可用的錨。📎 聯邦不是破口：外站寫的錨 sender 是外站的人，而片必須是錨的
作者發的。

### 4.2 收件人：跟一般訊息同一套

廣播前逐一問 `user_is_ignored(作者, 收件人)`（`Demand` 則問發問者）—— 房間推送本來就這樣過濾，
草稿不照做的話，「把某人 ignore 掉」會擋住他的訊息卻擋不住他的草稿（R5）。
⚠️ 這一步會 **await 資料庫**，所以送出時**在鎖內重驗**每個收件人還在不在這個房間：中間落地的
leave／kick 必須算數（`relay_to`）。

沒有 `g_seq → 事件` 的全站索引，所以 **`room_id` 是必填**：事件的 key 是 `(房間, g_seq)`，帶了 `room_id` 就是一次點讀。這是每片一次 DB 讀，
跟限速同量級（30 片／秒），不做快取——快取就是 server 的草稿狀態，第三版拿掉的東西不放回來。

## 5. Keypoint／Delta／Append：server 不懂內容

三種都是密文（E2EE 房），server 只轉發、只看 header 與明文 meta。語意是 client 之間的約定：

- **`Keypoint`**：完整字段，明文 ≤ 8 KiB。接收者把這則草稿的 buffer **整個清空**換成它。草稿比 8 KiB 長時，作者送 `Keypoint`（前 8 KiB）再 `Append` 其餘：
  例如 12 KiB 的草稿，有人 `Demand`，所有人的 buffer 先被削成 8 KiB、接著收到 4 KiB 的 `Append` 回到 12 KiB。這就是「塊」，不需要另一個型別。
- **`Delta`**：相對於接收者目前狀態的差異；接收者只在「有狀態」時套（收過 `Keypoint`，或從空字串開始且沒漏過片），不然等下一個 `Keypoint`（或自己 `Demand`）。
- **`Append`**：接在末尾。最常見的 LLM 情境，一片就是幾個 token。
- **片計數 `seq` 與 `prev`**（§3.0）：`seq` 是作者對每則草稿遞增的號（**從 1 起**），`prev` 是這片依據的那一片的號。接收者**套之前先比 `prev` 跟自己手上的狀態**；對不上就把這則草稿標成「等全文」並 `Demand`，收到 `Keypoint` 才繼續套。
  ⭐ 比「號碼應該大 1」可靠的地方在於：作者**換連線續發**時不必假裝連號 —— 它可以接著送（`prev` 指得到）或先送一個 `Keypoint`（`prev=0` 重新起頭），兩種接收端都讀得懂，而不是只能靠算術猜。
- **第一片**可以是 `Keypoint`、也可以是從空字串起的 `Delta`／`Append`——都允許，client 決定。
- **delta 的內部格式**（密文裡）定在 client 的約定，server spec 只定「有這三種」與它們的語意。

### 5.1 中途進來的人（維護者的例子）

房間 A、B、C、D。A 送 `Draft` → 佔位事件 `g_seq = 123`，`Push` 給 B、C（D 不在線）。A 不斷 `Append`／`Delta`，B、C 跟著改。
D 上線、`Subscribe`、從 `Recent` 或 `Push` 看到 123 是一則草稿、接著收到 `Delta` 但沒有狀態 → D 送 `Demand(id = 123)`。
server 讀 123 → 驗 D 是房間成員 → 跟其他片一樣**廣播給全房**（維護者 2026-09-08：不特別路由）；A 那台在寫草稿的裝置回一次
`Keypoint(id = 123)`（超過 8 KiB 就再接幾個 `Append`），其他人收到 `Demand` 就忽略。B、C 也收到那個 `Keypoint`，等於重新對齊一次。
之後的 `Delta`／`Append` D 就跟得上。

## 6. 跟 channel 共用什麼

| 共用 | 在哪 |
|---|---|
| 訂閱 registry、`try_send`、掉了就掉、全房廣播 | `Services.streams`（[wbf-event-push.md](wbf-event-push.md) §3）：`relay(room, None, pack)`，六個 subtype 都只用這一個 |
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

⚠️ **這一節是線上契約的一部分**：照它實作的 client 送出來的東西必須是 server 收的。所以每個例子裡的
`seq`／`prev`／錯誤碼都跟 §3.0／§4 同一套 —— 一份規格自己前後不一致，比少寫一段更糟（審查者 cirno）。

- alice、bob 訂閱；alice `Draft` → Ack 有 `event_id`、`g_seq`；bob 收到 `Push`，事件 type 是 `org.wbftw.wbfuwunel.draft`；alice 自己也 `Push` 到。
- alice `Append`（`seq=1`、`prev=0`，從空字串起）、`Delta`（`seq=2`、`prev=1`）、`Keypoint`（`seq=3`、`prev=0`）
  → bob 依序收到三個，meta 是 room id、data 一個 byte 不差（含開頭那 4 byte 的 `prev`）；alice 發送那條連線**也**收到自己的（全域廣播）。
- 鏈的三條檢查：`data` 不足 4 byte、片的 `seq=0`、`Keypoint` 的 `prev≠0` → 各回 `InvalidRequest`。
- `Keypoint` 的 data 10241 bytes → `TooLarge`；10240（含 4 byte `prev`）→ 過。meta 不是合法 room id → **`InvalidRequest`**。
  任何 `Stream` pack 帶了 flags → `InvalidRequest`。
- carol 沒訂閱 → 什麼都收不到；carol 訂閱後送 `Demand(id)` → alice、bob、carol 自己都收到（全域廣播）；alice 回 `Keypoint` → 三人都收到。
- 片計數：alice 送 `seq` 5、6、8（`prev` 各指 4、5、7）→ bob 在 8 看到跳號（e2e 只驗它原樣到達；跳號→Demand 是 client 的事）。
- 房間人數超過 `wbf_draft_max_room_members` → `Draft` 回 `Conflict`；設成 0 → 不限。
- bob（非作者）送 `Append(id)` → `Forbidden`；不存在的 `id` → `NotFound`；不是草稿的事件 → `Conflict`；HTTP → `Unsupported`；連送 100 片 → 一部分 `RateLimited`。
- **權限與可見性**（外部審查 2026-09-12 那七條，§4.1／§4.2）：
  非成員對「開著的草稿」與「不存在的 `g_seq`」拿到**一模一樣**的 `Forbidden`；
  一般 `Event/Send` 送錨的 type → 拒絕；
  ignore 作者的人收不到他的片，其他人照收；
  作者被降權之後、被停權之後的片都 `Forbidden`，但 `Abandon` 仍成功；
  `Demand` 帶 data → 轉發時被剝掉，> 1 KiB → `InvalidRequest` 並關線；
  重啟 server 之後同一個請求 `seq` 的 `Draft` 要寫出**新**錨，不是拿回上一輪那個。
- 收尾（client 流程）：alice `Abandon` → Ack、bob 收到 redaction 的 `Push`、`Recent` 裡佔位已 redact；之後對 id 送 `Append` → `Conflict`；alice 再送一則普通 `Event/Send` → 普通的 `Push`，`unsigned` 裡沒有任何草稿欄位。
- bob 停止讀 socket、alice 送 50 片 → alice 一片都沒被擋、bob 恢復後收到的是後面的片。
  📎 這條**沒有單獨的 e2e**：草稿走的是跟房間推送同一個 `try_send`／掉了就掉的路徑，情境 2 已經釘住它（維護者 2026-09-12 同意草稿允許掉包）。
- 關機中連線收到 Close 1001。

## 9. 維護者 2026-09-08 定的（原開放問題）

1. **定案用 redact，而且是 client 自己做**：client 送 `Abandon`，再自己發一則新訊息；server 對這兩種 pack 照常處理，不做多餘的事（維護者 2026-09-08）。不用 `m.replace`。
2. **佔位不帶 device**：`Demand` 是去跟發文者的**帳號**要，他的每台裝置都收到，正在寫草稿的那台才回。
3. **佔位永不過期**：沒有 TTL，永遠可用。
4. **meta 不用 JSON**：直接放 `room_id`，`g_seq` 在 header `id`。
5. **接收端可以落地**草稿內容（§3），server 是權威。

自我審查後維護者再定的（同日）：

6. **`seq` 收回來當片計數**（§3）：片會在 server 的發送佇列被丟，沒計數接收者不知道有洞。
7. **`Demand` 不特別路由**，跟其他片一樣廣播全房；寫草稿的那台回，其他人忽略。
8. **`wbf_draft_max_room_members`**（預設 10）：大房間不准開草稿。
9. **`Demand` 風暴**（很多人同時進來、作者一直重傳）：只在超過 8 KiB 的草稿才痛，client 端加重傳間隔門檻（或禁用重傳）就能解；情境少，先不考慮。
10. **定案的順序**已經不是 server 的問題（見 1）：client 自己送 `Abandon` 與新訊息，哪個先都行。
11. **registry 的鎖**是實作細節（`RwLock`，`publish` 拿讀鎖），不進協議。

剩下的小事：`g_seq` 的型別——`PduCount` 是有號的（backfill 的事件為負），但佔位事件是本站新寫的、永遠正，塔進 u64 的 header `id` 沒問題；server 對讀不到的回 `NotFound`。

## 10. 這份文件的查證範圍

- `src/api/client/wbf/{mod,ws,send}.rs`（PR #33）：發送佇列、准入表、`Event/Send` 的 meta 解析與 `send_message_event`。
- `src/database/maps.rs`、`src/service/rooms/timeline/`：`pduid_pdu` 的 key 是 `(shortroomid, count)`，`count` 就是 `g_seq`，所以 `(room_id, g_seq)` 是點讀；沒有全站 `g_seq` 索引（room-seq-and-recent §2.2）。
- `src/api/client/redact.rs`：redact 自己的事件的既有路徑。
- 沒實測：每片一次點讀在 30 片／秒下的成本（預期可忽略，RocksDB 點讀微秒級）；大房間廣播的成本（跟 `Push` 同一題）。
