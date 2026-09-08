# WS 訂閱與推送：連線訂閱自己的帳號，server 把新事件推過來

**狀態**：📄 草案（2026-09-08），等維護者同意。這是維護者 2026-09-08 說的「工作 2」：WS 訂閱自己帳號的 event，在的任何房間的新事件自然推過來。
[streaming-messages.md](streaming-messages.md)（工作 3）坐在這份之上；工作 1（一般訊息走 WS）已經是 `Event/Send`（PR #24）。

**這份改的是什麼**：到 PR #33 為止，WS 只有 client 問、server 答；[wbf-pack-pipeline.md](wbf-pack-pipeline.md) §1 留了發送 task 與有界佇列，就是給這裡用的：
推送是**第二個往 client 送東西的來源**，跟 handler 的回應排同一個佇列。

## 0. 一句話

一條已登入的 WS 送 `Event/Subscribe` 之後，server 把這個使用者**所有加入房間的新事件**逐則推過來（`Event/Push`），推的可以掉、掉了用 `Recent` 補，
**持久化的真相永遠在 DB、在 `Recent`**；推送只是「叫你不用輪詢」。

## 1. 模型：channel（維護者 2026-09-08 定）

維護者的話：群組不是房間，是 **channel**，像 Redis 的 channel：純記憶體、跟房間深度綁定、隨房間開關而生滅。

| 規則 | 意思 |
|---|---|
| **一房一個 channel** | channel 的名字就是 `room_id`；房間沒人訂閱時 channel 不存在（HashMap 沒那個 key），第一個訂閱者進來才建，最後一個退出就拆。 |
| **顯式訂閱才推** | 使用者上線、登入、自己送 `Subscribe`，這時才進 channel。升級、登入都不自動訂；媒體那條連線不訂就不收。 |
| **進得了嗎？** | 在那個房間裡（`state_cache.is_joined`）才進得了；不在 → 那個房跳過（帳號層訂閱）或 `Forbidden`（點名訂那一房）。 |
| **退房自動退訂** | leave／kick／ban 那一刻（`state_cache` 的寫入點）把這個 user 的所有連線從這個 channel 拿掉。 |
| **斷線自動退訂** | on disconnect：這條連線持有的所有 channel 全退（RAII，`Subscription` drop）。 |
| **訂閱者的識別碼是連線** | 維護者定：不是 `user_id`（多裝置只有一台收得到），也不是 `(user, device)`（一個裝置會開多條連線，哪條處理事件是 client 的事）。WebSocket 本身沒有連線識別碼，server 在升級時發一個 **`connection_id`**（process 內單調遞增的 u64，process 活著期間唯一），跟那條連線的 `Session` 一起活；訂閱者就是它。連線已登入，所以 `connection_id → (user, device)` 由 `Session` 給。 |
| **冪等** | 重複訂閱自動去重（同一條連線在同一個 channel 只有一筆）；退訂不存在的 = no-op。同裝置兩條連線都訂，兩條都收：server 不裁決，client 自己別這樣做。 |

為什麼這比「每則事件掃房間成員」好：事件到使用者在 DB 裡沒有直接關係，只有事件→房間（`pduid_pdu` 的 key）與房間↔使用者（`roomuserid_joined`／`userroomid_joined`）；
每則事件掃一次成員是房間人數的成本，而真正在收的人通常是少數。channel 把這件事反過來：每則事件只碰訂閱者，O(訂閱者)。
代價是 channel 要跟 join／leave 同步，而 join／leave 只有 `state_cache` 一個寫入點（`mark_as_joined` 那一組），hook 接在那裡就不會漂。

**訂閱的單位**：`Subscribe` 可以點名房間（`rooms: […]`），也可以不帶 = 帳號層：現在加入的每個房都進，**之後新加入的房也自動進**（join hook 把這個 user 帳號層訂閱中的連線加進新房的 channel）。
兩種都是同一張表，差別只在「新房要不要自動跟」。

## 2. pack

kind `0x14 Event`（§3.3 的 Event 章），三個新 subtype：

| subtype | 誰發 | header | meta | data |
|---|---|---|---|---|
| `0x04 Subscribe` | client | `id` 由 client 選（之後每個 `Push` 抄它） | `{ "rooms"?: ["!…"], "cg_seq"?: <g_seq> }`；沒帶 `rooms` = 帳號層（所有加入的房，含之後加入的） | 無。回 `Ack`，meta `{ "latest_g_seq": <g_seq>, "joined": n, "skipped": ["!…"] }`（`skipped` = 點名了但不是成員的房） |
| `0x05 Unsubscribe` | client | 同上 | `{ "rooms"?: ["!…"] }`；沒帶 = 全退 | 無。回 `Ack`；退不存在的是 no-op |
| `0x06 Push` | **server → client** | `IS_RESPONSE=1`；`id` 抄 `Subscribe`；`seq` 從 0 起每推一次 +1 | `{ "bc": n, "fs", "ls", "gap": bool }` | `bc` 則事件，**u32 大端長度 ＋ 事件 JSON**，跟 `Batch` 同一個切法（room-seq-and-recent §2.1），新到舊 |

- **只走 WS**（准入表 `logged_in_websocket_only`）；HTTP 回 `Unsupported`。
- **一條連線一個訂閱者身分**（一個 `id`、一條 `seq`），它可以在多個 channel 裡；再送 `Subscribe` 是加進更多 channel（去重），不是換掉。`Logout`／關線全退。
- **`Subscribe` 帶 `cg_seq` 的意思**：「我快取到這裡了，比它新的請一起給」——server 回 Ack 之後**先推一輪 `cg_seq` 之後的事件**（走 `Recent` 同一個 `collect_window`，
  用 `Push` 包，最多 `wbf_recent_max_limit` 則），再開始推新的。這樣 client 連上後不用先 `Recent` 再 `Subscribe` 還要擔心中間那條縫。
  不帶 `cg_seq` = 只要新的。
- **`gap: true`**：這條連線在上一個 `Push` 之後**有事件沒推到**（§4）。client 看到就用 `Recent(cg_seq)` 補一窗；補完之後的推送接得上。
- `Push` 是**事件驅動類**（wire-format §4）：不 Ack、不重送、client 不守順序。`fs`／`ls` 是這一包的最新／最舊 g_seq，只給 client 推水位用。

## 3. server 端：兩張表、兩個接點

```
registry（純記憶體，`Services.channels`）
  SubscriberId = connection_id (u64)                              ← 維護者定：基於連線，不是使用者、也不是裝置；server 升級時發，AtomicU64 遞增
  channels:     HashMap<room_id, HashSet<connection_id>>         ← 「channel」本體；空了就 remove
  subscribers:  HashMap<connection_id, Subscriber>                ← Subscriber { user, device, queue: mpsc::Sender<Outgoing>, id, seq, gap, account_wide: bool }
  by_user:      HashMap<user_id, HashSet<connection_id>>          ← 退房 hook 要用：這個人的所有訂閱中的連線

接點 1：append_pdu 提交之後
   └─▶ channels::publish(room_id, pdu)
          ├─ channels[room_id] 沒有 → 回（絕大多數房間：零成本）
          ├─ 事件 JSON 序列化一次
          └─ 對每個訂閱者：ignored_filter（發送者被這人 ignore 就跳）→ try_send(Push)；佇列滿 → 記 gap，不等

接點 2：state_cache 的 join／leave 寫入點（mark_as_joined 那一組）
   ├─ leave／kick／ban(user, room) → channels::evict(user, room)：by_user[user] 裡每個訂閱者從 channels[room] 拿掉
   └─ join(user, room)             → channels::follow(user, room)：by_user[user] 裡 account_wide 的訂閱者加進 channels[room]
```

- **`Subscribe`**：點名的房逐一 `is_joined`，是成員才進 channel，不是的列進 Ack 的 `skipped`；沒點名 = 帳號層：掃 `userroomid_joined` 的前綴 `(user, *)`（`Recent` 每次掃的同一張表，
  幾十到幾百個房，一次前綴讀），每個房把這個訂閱者放進 `channels[room]`，並標 `account_wide`——這個旗標只做一件事：之後這個帳號 join 新房時，join hook 把它加進新房的 channel。
  **順序：先登記 `subscribers`／`by_user`（含 `account_wide`），再掃表加 channel。** 反過來的話，掃到一半發生的 join 其 hook 找不到這個訂閱者就漏了；先登記則最多重複加一次，HashSet 的 no-op。
  同一訂閱者進同一 channel 兩次永遠是 no-op。
- **`Unsubscribe`**：從點名的 channel 拿掉，不在裡面就 no-op；沒點名 = 全退並拿掉 `account_wide`。
- **`connection_id`**：`ws_route` 升級時從一個 `AtomicU64` 拿，傳進 `serve`，放進 `PackContext`；`Hello` 的回應多報 `connection_id`（除錯用，client 不必用）。它只在 process 內有意義，不進 DB。
  `Login` 換帳號時連線號不變，但訂閱**全退**（channel 成員資格是舊帳號的），新帳號要收就再 `Subscribe`。
- **on disconnect**：`Subscription` 是 RAII（跟 `ConnectionSlot` 同形），drop 就把這個連線號從它在的每個 channel、`by_user`、`subscribers` 拿掉；不靠 loop 記得。
- **真相在哪**：誰能收的真相是 DB 的成員表；channel 是它的記憶體投影，靠接點 2 保持一致。漂移只可能來自漏接 hook 的新 join／leave 路徑，而那只有 `state_cache` 一組；重啟就清空，安全方向是「少推」，`Recent` 補得回來。
- **接點 1 只有 `append_pdu` 提交後**：backfill 進來的舊事件不推（不是「新的」，`Recent` 拿得到）；redaction 是一則新事件，照推。
- **可見性**：新事件對「現在是成員的人」永遠可見（`history_visibility` 管的是加入前的歷史），而能在 channel 裡的一定是成員，所以只做 ignore 過濾。
- **自己的事件也推**（含發送它的那條連線）：多裝置同步靠這個，發送者的其他裝置要收到；發送那條連線同時有 `Ack`（`event_id`）和 `Push`，client 用 `event_id` 去重。
  不做「排除發送連線」的特例：多一條規則，省一個幾百 byte 的包。

## 4. 掉包、背壓、上限

- **推送絕不阻塞 append**：`try_send`，佇列滿就丟並把 `gap` 記起來，下一次推得進去的 `Push` 帶 `gap: true`。append 是所有訊息的路徑，不能被一條讀得慢的連線拖住。
- **掉了的不重送**：持久化的事件 `Recent` 拿得到；推送的責任是「盡快」不是「一定」。這跟 pipeline §1 的背壓（handler 等佇列）**故意不同**：handler 的回應是 client 問的，等得起；推送是 server 塞的，塞不進就算。
- **每連線的成本**：訂閱者一筆 ＋ 它在的 channel 數個 HashSet 項；佇列是 pipeline 的那個。**每則事件的成本**：一次 `channels[room]` 查詢＋訂閱者數次 `try_send`；跟房間人數無關。
- 上限：`wbf_push_max_events_per_pack`（一個 `Push` 最多幾則，預設 10，收 `cg_seq` 那輪用）；其餘沿用 pack 的 byte 上限。

## 5. 跟現有東西的關係

| 東西 | 關係 |
|---|---|
| `/sync` | 不動。沒連 WS 的 client 照舊 long-poll。推送不推進任何 sync token。 |
| `Event/Recent` | 真相與補洞。`Subscribe` 帶 `cg_seq` 那一輪內部就是 `collect_window`。 |
| typing、receipts、presence | **這版不推**（§6）。 |
| E2EE | 事件本來就是密文，推的是 `pduid_pdu` 裡的 JSON，server 不多讀任何東西。 |
| 聯邦 | 不相干：聯邦進來的事件走同一個 `append_pdu`，一樣推。 |
| [streaming-messages.md](streaming-messages.md) | 草稿走同一個 channel、同一條佇列，`channels::relay(room, except_connection, pack)`。 |

## 6. 不做的

- 不推 typing／receipt／presence／to-device／帳號資料：先把事件這條走通；那些是 Matrix `/sync` 的其他區塊，之後各自一個 subtype。
- 不做事件型別的 filter：進了 channel 就是該房全部新事件；要少收是 client 的事。
- 不保證推送順序與不掉：見 §4。
- 不做 server 端的推送重送佇列：那是第二份真相。

## 7. 驗收（e2e11 新腳本）

- alice 兩條 WS，一條 `Subscribe`、一條不：bob 送訊息 → 只有訂閱那條收到 `Push`（一則、`fs=ls=` 該 g_seq）；alice 自己送 → 兩個包：`Ack` 帶 `event_id`，`Push` 同一則。
- `Subscribe` 帶 `cg_seq`：連上前 bob 送了 5 則 → Ack 後先來 5 則（一或多個 `Push`），再來新的。
- 訂閱後 alice 加入新房間 → 那房的新事件也推；離開 → 停。
- ignore bob → bob 的事件不推。
- 背壓：訂閱那條停止讀 socket、bob 連送 100 則 → server 不阻塞（bob 每則都 Ack 得到）、恢復讀之後第一個 `Push` 帶 `gap: true`、`Recent` 補齊 100 則。
- `Unsubscribe` 後不再推；`Logout` 後 registry 為空（重啟不需要，表在記憶體）。
- 關機：訂閱中的連線收到 Close 1001、程序乾淨退出（registry 的 sender 隨連線 drop）。

## 8. 落點

| 東西 | 檔 |
|---|---|
| `Services.channels`：三張表、`subscribe`／`unsubscribe`／`Subscription`（RAII）、`publish`、`relay`、`follow`／`evict` | `src/service/channels/mod.rs`（新；名字避開既有的 `service/push`＝Matrix push rules） |
| 接點 1 | `src/service/rooms/timeline/append.rs`，提交後 |
| 接點 2 | `src/service/rooms/state_cache/update.rs`，`mark_as_joined`／left 那一組 |
| `Subscribe`／`Unsubscribe` handler、`Push` 編碼（共用 `recent.rs` 的長度前綴） | `src/api/client/wbf/subscribe.rs`（新）、`mod.rs` 准入表三列 |
| 推送 task 怎麼餵佇列 | 不需要新 task：`publish` 直接對訂閱者的 `mpsc::Sender` `try_send`，發送 task 已經在 |
| kind 表 | `wbf-wire-format.md` §3.2 三列 |
| 向量 | `subscribe`、`push_one`、`push_gap` |

## 9. 我不滿意或想再談的

- **接點 2 的完整性**：退房的路徑不只一條（自己 leave、被 kick、被 ban、房間被刪、聯邦進來的 leave），但它們最後都經 `state_cache` 的同一組寫入。實作時要用 e2e 把每一條都走一遍，而不是相信這句話。
- **`gap` 只有一個 bit**：client 知道「有洞」但不知道洞多大，只能 `Recent` 一窗；洞比一窗大就要再補。可接受，跟 `Recent` 的翻窗是同一個動作。
- **`Subscribe` 帶 `cg_seq` 那輪與新推送之間的縫**：先讀 `latest` 再收窗、窗收完才開始推，中間到的事件在 registry 註冊之前 → 會漏。修法：**先註冊再收窗**，推送與窗可能重複同一則，client 用 `event_id` 去重。文件裡定這個順序。
