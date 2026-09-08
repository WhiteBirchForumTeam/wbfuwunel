# WS 訂閱與推送：連線訂閱自己的帳號，server 把新事件推過來

**狀態**：📄 草案（2026-09-08），等維護者同意。這是維護者 2026-09-08 說的「工作 2」：WS 訂閱自己帳號的 event，在的任何房間的新事件自然推過來。
[streaming-messages.md](streaming-messages.md)（工作 3）坐在這份之上；工作 1（一般訊息走 WS）已經是 `Event/Send`（PR #24）。

**這份改的是什麼**：到 PR #33 為止，WS 只有 client 問、server 答；[wbf-pack-pipeline.md](wbf-pack-pipeline.md) §1 留了發送 task 與有界佇列，就是給這裡用的：
推送是**第二個往 client 送東西的來源**，跟 handler 的回應排同一個佇列。

## 0. 一句話

一條已登入的 WS 送 `Event/Subscribe` 之後，server 把這個使用者**所有加入房間的新事件**逐則推過來（`Event/Push`），推的可以掉、掉了用 `Recent` 補，
**持久化的真相永遠在 DB、在 `Recent`**；推送只是「叫你不用輪詢」。

## 1. 為什麼是訂閱、不是升級就推

- client 會開多條連線（pipeline §0-1）：一條走事件、一條走媒體。媒體那條不該收到事件推送，不然 Batch 串流跟大檔上傳擠同一條佇列，正好是 §0-1 要避免的。
  **誰要收，誰說**：`Subscribe` 是連線層的動作，不是帳號層的設定。
- 訂閱的單位是**帳號**（維護者定），不是房間：加入的每個房間都收，之後加入的新房間自動收，離開的自動停。server 不維護「這條連線訂了哪些房」，只維護「這個使用者有哪些連線在收」。

## 2. pack

kind `0x14 Event`（§3.3 的 Event 章），三個新 subtype：

| subtype | 誰發 | header | meta | data |
|---|---|---|---|---|
| `0x04 Subscribe` | client | `id` 由 client 選（之後每個 `Push` 抄它） | `{ "cg_seq"?: <g_seq> }` | 無。回 `Ack`，meta `{ "latest_g_seq": <g_seq> }` |
| `0x05 Unsubscribe` | client | 同上 | `{}` | 無。回 `Ack` |
| `0x06 Push` | **server → client** | `IS_RESPONSE=1`；`id` 抄 `Subscribe`；`seq` 從 0 起每推一次 +1 | `{ "bc": n, "fs", "ls", "gap": bool }` | `bc` 則事件，**u32 大端長度 ＋ 事件 JSON**，跟 `Batch` 同一個切法（room-seq-and-recent §2.1），新到舊 |

- **只走 WS**（准入表 `logged_in_websocket_only`）；HTTP 回 `Unsupported`。
- **一條連線一個訂閱**：再送 `Subscribe` 就換掉舊的（新 `id`、`seq` 歸零）；`Logout`／關線自動退訂。
- **`Subscribe` 帶 `cg_seq` 的意思**：「我快取到這裡了，比它新的請一起給」——server 回 Ack 之後**先推一輪 `cg_seq` 之後的事件**（走 `Recent` 同一個 `collect_window`，
  用 `Push` 包，最多 `wbf_recent_max_limit` 則），再開始推新的。這樣 client 連上後不用先 `Recent` 再 `Subscribe` 還要擔心中間那條縫。
  不帶 `cg_seq` = 只要新的。
- **`gap: true`**：這條連線在上一個 `Push` 之後**有事件沒推到**（§4）。client 看到就用 `Recent(cg_seq)` 補一窗；補完之後的推送接得上。
- `Push` 是**事件驅動類**（wire-format §4）：不 Ack、不重送、client 不守順序。`fs`／`ls` 是這一包的最新／最舊 g_seq，只給 client 推水位用。

## 3. server 端：一張表、一個接點

```
append_pdu 提交之後
   └─▶ push::notify(room_id, pdu)
          ├─ 這個房間現在的成員（state_cache）
          ├─ 每個成員：registry[user] 有連線在收嗎？沒有就跳過（絕大多數使用者）
          ├─ 有：ignored_filter（發送者被這個人 ignore 就不推）
          └─ 對每條在收的連線 try_send(Outgoing::Pack(Push))；佇列滿 → 記 gap，不等
```

- **registry**：`Services.push`（新 service，仿 `typing`）持 `Mutex<HashMap<OwnedUserId, Vec<Subscriber>>>`，`Subscriber { device, queue: mpsc::Sender<Outgoing>, id, seq: AtomicU32, gap: AtomicBool }`。
  `Subscribe` 註冊、`Unsubscribe`／連線結束移除（RAII，跟 `ConnectionSlot` 同一個形狀：`Subscription` drop 就移除，不靠 loop 記得）。
- **接點只有一個**：`append_pdu` 提交後。backfill 進來的舊事件不推（它們不是「新的」，`Recent` 拿得到）。redaction 是一則新事件，照推；client 自己套用。
- **事件 JSON 一則只序列化一次**，每個接收者共用同一份 `Bytes`（`Push` 的 data 段直接是它）。
- **可見性**：新事件對「現在是成員的人」永遠可見（`history_visibility` 管的是加入前的歷史），所以只做 ignore 過濾，不跑 `visibility_filter`。
  離開房間的人在 leave 事件之後就不在成員清單，自然停。
- **自己的事件也推**（含發送它的那條連線）：多裝置同步靠這個，發送者的其他裝置要收到；發送那條連線同時有 `Ack`（`event_id`）和 `Push`，client 用 `event_id` 去重。
  不做「排除發送連線」的特例：多一條規則，省一個幾百 byte 的包。

## 4. 掉包、背壓、上限

- **推送絕不阻塞 append**：`try_send`，佇列滿就丟並把 `gap` 記起來，下一次推得進去的 `Push` 帶 `gap: true`。append 是所有訊息的路徑，不能被一條讀得慢的連線拖住。
- **掉了的不重送**：持久化的事件 `Recent` 拿得到；推送的責任是「盡快」不是「一定」。這跟 pipeline §1 的背壓（handler 等佇列）**故意不同**：handler 的回應是 client 問的，等得起；推送是 server 塞的，塞不進就算。
- **每連線的成本**：訂閱本身零記憶體（registry 一筆）；佇列是 pipeline 的那個。**每則事件的成本**：成員數 × 一次 HashMap 查詢；有訂閱的成員才序列化與入隊。
- 上限：`wbf_push_max_events_per_pack`（一個 `Push` 最多幾則，預設 10，收 `cg_seq` 那輪用）；其餘沿用 pack 的 byte 上限。

## 5. 跟現有東西的關係

| 東西 | 關係 |
|---|---|
| `/sync` | 不動。沒連 WS 的 client 照舊 long-poll。推送不推進任何 sync token。 |
| `Event/Recent` | 真相與補洞。`Subscribe` 帶 `cg_seq` 那一輪內部就是 `collect_window`。 |
| typing、receipts、presence | **這版不推**（§6）。 |
| E2EE | 事件本來就是密文，推的是 `pduid_pdu` 裡的 JSON，server 不多讀任何東西。 |
| 聯邦 | 不相干：聯邦進來的事件走同一個 `append_pdu`，一樣推。 |
| [streaming-messages.md](streaming-messages.md) | 草稿走同一張 registry、同一條佇列，`push::relay(room, sender, pack)`。 |

## 6. 不做的

- 不推 typing／receipt／presence／to-device／帳號資料：先把事件這條走通；那些是 Matrix `/sync` 的其他區塊，之後各自一個 subtype。
- 不做 per-room 訂閱、不做 filter：帳號層訂閱是維護者定的；要少收是 client 的事。
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
| `Services.push`：registry、`subscribe`／`Subscription`（RAII）、`notify`、`relay` | `src/service/push_ws/mod.rs`（新；名字避開既有的 `service/push`＝Matrix push rules） |
| 接點 | `src/service/rooms/timeline/append.rs`，提交後 |
| `Subscribe`／`Unsubscribe` handler、`Push` 編碼（共用 `recent.rs` 的長度前綴） | `src/api/client/wbf/subscribe.rs`（新）、`mod.rs` 准入表三列 |
| 推送 task 怎麼餵佇列 | 不需要新 task：`notify` 直接對 registry 裡的 `mpsc::Sender` `try_send`，發送 task 已經在 |
| kind 表 | `wbf-wire-format.md` §3.2 三列 |
| 向量 | `subscribe`、`push_one`、`push_gap` |

## 9. 我不滿意或想再談的

- **每則事件掃一次成員**：大房間（幾千人）每則事件幾千次 HashMap 查詢。可以反過來維護 `room → 訂閱者`，但要跟 join／leave 同步，多一份會漂的表。先用簡單的，量了再說。
- **`gap` 只有一個 bit**：client 知道「有洞」但不知道洞多大，只能 `Recent` 一窗；洞比一窗大就要再補。可接受，跟 `Recent` 的翻窗是同一個動作。
- **`Subscribe` 帶 `cg_seq` 那輪與新推送之間的縫**：先讀 `latest` 再收窗、窗收完才開始推，中間到的事件在 registry 註冊之前 → 會漏。修法：**先註冊再收窗**，推送與窗可能重複同一則，client 用 `event_id` 去重。文件裡定這個順序。
