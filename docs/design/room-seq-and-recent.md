# 每房連續序號 `r_seq` 與跨房間「全域最近 N 則」

> **這份文件回答：client 的聊天模型要 server 配合的兩件事，server 端怎麼做、為什麼這樣做。**
> 需求出處：issue #20（wbf-matrix-client `docs/design/chat-model.md` §4.3、§7）。
> 狀態：🔧 維護者 2026-09-05 同意（PR #21），實作分支 `event/room-seq-recent`。
> 相關：[wbf-wire-format.md](wbf-wire-format.md)（pack 與 kind 分配）、[roadmap.md](roadmap.md)。

## 0. 一句話

1. **`r_seq`**（room sequence）：每個 room 一個計數器，新事件 1、2、3…，聯邦補回的舊歷史 0、−1、−2…。
   **`g_seq`**（global sequence）：本站全域的事件序號，就是事件的 PduCount（`/messages` token 裡那個數），跨 room 可比大小，
   client 拿它當水位線「我讀到哪」。兩個都**寫進存起來的 PDU JSON 的 `unsigned`**（`org.wbftw.wbfuwunel.r_seq`、`…g_seq`），
   所以每一條把事件交給 client 的路徑自動都帶，不用逐條路徑補。命名維護者 2026-09-05 定：兩個短名對照，而且跟 pack 標頭的 `seq`（請求號）分得開。
2. **全域更新**：pack `Event/Recent`（kind `0x14`、subtype `0x01`），client 帶自己快取的水位 `cg_seq`，server 回在它之後的事件（最多 `limit` 則）。
   server 對使用者加入的每個 room 開一條倒序串流，用 `g_seq` 做 k 路合併，套 `/messages` 同一套可見性過濾。**不加索引、不加遷移**。

## 1. `r_seq`：每房連續序號

### 1.1 為什麼寫進存起來的 JSON，而不是讀的時候補

issue 列了七條要帶 `r_seq` 的路徑（sync、messages、context、event、relations、timestamp_to_event、search）。它們最後全都讀
`pduid_pdu` 這一張表；`age` 現在是讀的時候在 `timeline/pdus.rs` 的 `each_pdu` 與 `room/event.rs` 兩處補的。如果照 `age` 的做法，
`r_seq` 要另開一張 `pduid → seq` 表，而且 `each_pdu` 是同步函數、要改成 async 才能點讀。

寫進存起來的 JSON 之後：**一個寫入點、零個讀取點**。任何現在或未來讀 `pduid_pdu` 的路徑都拿到它，client 不會看到
「同一個事件有時有、有時沒有」。代價是兩處要留意（§1.4、§1.5），都是「有人重寫這份 JSON」的地方。

`unsigned` 是 server 加的、不進事件雜湊，所以放這裡不動事件本體，其他 client 看到不認得的 key 照規格忽略。

### 1.2 計數器

新表 `roomid_seqbounds`：`room_id → (i64 last_forward, i64 last_backfilled)`。

| 路徑 | 拿號 | 寫入 |
|---|---|---|
| `append_pdu`（本地送出、聯邦即時收到） | `last_forward + 1`（第一個是 1） | 與 `pduid_pdu` **同一筆交易** |
| `backfill_pdu`（聯邦補回比現有最早還早的） | `last_backfilled − 1`，第一個是 **0** | 同上 |

兩條路徑在拿號那一刻都已經持有 `timeline.mutex_insert` 這把每房一鎖（`append.rs:179`、`backfill.rs:391`），所以同一 room 內
拿號不會撞；表的寫入跟 PDU 本身同一交易，不會出現「號發了 PDU 沒存」或反過來。

- **`r_seq` 是本站的到達順序，不是 `origin_server_ts` 順序。** 聯邦即時送來的事件（`PUT /send`），不管它的時間戳多舊 ——
  對方斷線一陣子、重連後一次送來的也一樣 —— 都走 `append_pdu`，拿當下的正號接在尾巴；收到事件時缺的 `prev_events` 抓回來後
  進 timeline 也走同一條、拿當下的正號。Matrix 的 timeline 本來就是到達順序，token、PduCount 都不會為時間戳重排，`r_seq` 跟它們一致。
  負號**只**給 `/backfill`：本站對這個 room 已知的最早事件之前還有歷史、而本站從來沒有（典型是透過聯邦加入一個聊了很久的 room，
  使用者往上滑到頭）。這條路徑在 `allow_federation=false` 時不會發生，但程式碼在，就要有定義；維護者 2026-09-05 確認這個模型。
- 同一 room 內唯一且單調：新 append 一定大於之前所有正號。
- 正號發出去**永不重排**；backfill 只碰負號。
- state 事件也算：所有進 timeline 的 PDU 都編號（與 PduId 的 count 語意一致，server 端最簡單）。
  「只算訊息」若之後要，client 那邊不受影響，server 多分一層即可，現在不做。
- 型別 i64，寫進 JSON 是整數。

### 1.3 現有 room 的一次性回填

startup migration `backfill_room_seq`（`src/service/migrations/backfill_room_seq.rs`，照既有 `rebuild_roomid_tscount_pducount` 的形狀，
`global` 表的 marker 保證只跑一次；新庫在 `fresh()` 直接寫 marker）。先只讀 `pduid_pdu` 的 **key**（不讀 body，全部 id 放得進記憶體），
按 shortroomid 分組、組內依 PduCount 排序：Normal 的從小到大從 1 編；Backfilled 的**從最接近 0 的 count 往下**編 0、−1…
（backfill 每次拿的 count 更負、事件更舊，這樣才跟它自己發號的順序一致），逐筆點讀、改 `unsigned`、寫回，最後寫那個 room 的 `roomid_seqbounds`。

這是一次全表重寫。自架規模可接受；migration 跑在開始服務之前，不需要鎖。跑過的 room client 直接有號；沒跑到的（不會有，
但防禦性地）client 看不到 key 就退化，不會壞。

### 1.4 redact 會剝掉 `unsigned`

ruma 的 `redact_in_place` 只留規格允許的 key，`unsigned` 不在裡面，所以 redact 後 `r_seq` 會消失。`redact.rs` 在 redact 之後本來就
會塞 `unsigned.redacted_because`；**同一處**先把 `r_seq` 讀出來，redact 完放回去。issue 要求「被 redact 的事件保留原號」就是這條。
測試釘住：redact 前後 `r_seq` 相同。

### 1.5 聯邦送出要剝掉

`core/matrix/pdu/format.rs` 的 `into_outgoing_federation` 現在只從 `unsigned` 移掉 `transaction_id`；加一行移掉 `org.wbftw.wbfuwunel.r_seq`。
它是本站的編號，對別站沒意義；`allow_federation=false` 時本來也送不出去，但寫入點只有一個，不要靠設定值當防線。

### 1.6 不動的

- `eventid_outlierpdu`（outlier 不在 timeline，沒號）。
- `roomid_tscount_pducount`、PduId、PduCount：全域 count 照舊，`r_seq` 是另一個數，不取代 token。
- `/messages` 的 `from`/`to` 仍是 token；「跳到第 N 則」client 用 `r_seq` 找對應事件是 client 端的事（§3 列為候選）。

## 2. `Event/Recent`：跨房間的全域更新（在 `cg_seq` 之後的事件）

### 2.1 wire

kind `0x14 Event`（wire-format §3.3 已分配給 send／messages／context 這個領域），subtype **`0x01 Recent`**。無序類（請求號對回應）。

請求 meta（JSON，明文，server 要讀）；三個欄位都可省略：

```json
{ "cg_seq": <g_seq>, "before": <g_seq>, "limit": 10000 }
```

- `cg_seq`（cached g_seq）：client 裝置上存的最新 `g_seq`。server 從最新往舊拿，**碰到它就停**；省略或 0 = 沒有快取，直接拿最新的 `limit` 則。
- `limit`：這頁最多幾則，預設與上限都是 `wbf_recent_max_limit`（10000）。
- `before`：只在補洞時用（見下），只要比它舊的。

server 從最新往舊走，收滿 `limit` 就停。差 4000 則就回 4000 則；差 85000 則就回**最新的** 10000 則，中間那段是洞。
維護者 2026-09-05 定：always 拿一萬本身有問題，client 要帶自己的水位、只取差異。

回應 `Ack`（`IS_RESPONSE`，`id`、`seq` 抄請求）：

- meta：`{ "returned": n, "latest_g_seq": <g_seq>, "complete": bool, "next": <g_seq> | null }`
  - `latest_g_seq`：server 此刻最新的全域序號，client 存下來當下次的 `cg_seq`（在合併之前讀，所以合併期間新進的事件一定比它新，不會漏）。
  - `complete`：`cg_seq` 到 `latest_g_seq` 之間是否全部給了。`false` 表示有洞，`next` 是洞的上緣：client 帶同一個 `cg_seq` 加 `before = next`
    再問，直到 `complete`。
- data：**JSON 陣列**，每個元素是完整的事件（`Pdu` 格式，含 `room_id`；`unsigned` 帶 `r_seq`、`g_seq`）。順序新到舊。

data 由 server 填是既有先例（`Download/Read` 的回應 data 是讀出的 bytes）。不另外包 `{room_id, g_seq, event}` —— 事件本身
已經帶這些欄位，包一層是重複。所有位置都是整數，跟 `unsigned` 裡的一致；client 不解讀、原樣帶回。

### 2.2 演算法：k 路合併，不加索引

`pduid_pdu` 的 key 是 `(shortroomid, count)`，沒有全域順序的索引；但 **count 是全域發的**（`globals.next_count`），跨 room 可以直接比大小。

```
rooms = state_cache.rooms_joined(user)
每個 room 開 timeline.pdus_rev(Some(user), room, before)   ← 倒序、從 before 往舊（沒給就從最新）
BinaryHeap 以 count 為鍵，每次彈最大的、再從那條串流補一個
每彈一個：ignored_filter → visibility_filter（api/client/message.rs 既有的兩個，pub(crate)）
停：滿 limit，或 data 再放一個就超過 wbf_data_max_bytes（→ complete=false，next=最後一則的 g_seq）；
    或堆頂的 g_seq 已經 ≤ cg_seq，或全部串流耗盡（→ complete=true，next=null）
```

- 為什麼不加 `count → pduid` 全域索引：那要一張新表、一次遷移、還要每個 append 多一筆寫；而且使用者只在少數 room 裡時，倒著掃全域
  索引大多在跳別人的房間。k 路合併只讀該使用者的 room，每個 room 一條既有串流，堆的大小 = room 數（幾十到幾百）。
- Backfilled 的事件 count 為負，會排在所有 Normal 之後 —— 它們是「本站知道這個 room 之前」的歷史，排在最舊那邊是對的。
- 可見性照 `/messages`：`history_visibility`、ignore、離開後看不到之後的，都在那兩個 filter 裡；`rooms_joined` 只給加入中的 room
  （issue 的「加入的所有 room」）。E2EE 密文原樣回。
- 上限：新 config `wbf_recent_max_limit`（預設 10000），超過 clamp 不報錯；另有 data 的 byte 上限兜底（10000 則 E2EE 事件可能超過
  16 MiB），所以一次可能回不滿 10000，client 用 `next` 接著翻 —— 這跟「往更舊翻」是同一個動作。
  被 byte 上限擋下的那一則不算進這頁，游標停在前一則，它成為下一頁的第一則。唯一的例外：**一則事件自己就大於 `wbf_data_max_bytes`**，
  那它永遠送不出去，游標跨過它（否則 client 會卡在同一頁），server 用 `debug_warn` 記下 event_id。被 ignore／不可見而跳過的事件也推進游標。
- 程式碼：`src/api/client/wbf/recent.rs`（`handle_event_recent`）；`mod.rs` 的 `event::RECENT` 派發；`Hello` 的 `features` 多了 `recent`、`r_seq`。

### 2.3 HTTP

`POST /_wbf/v1/pack` 同一個 pack 就能走，不另開 GET。issue 說 GET「也可以」，pack 才是必要的；少一條路徑少一份漂移。

### 2.4 規格向量

`wbf-vectors.json` 加一組 `Event/Recent` 請求與回應的外框（header、meta），data 用兩個最小事件。client repo 複製一份寫測試。

## 3. 不在這次裡（候選，另開）

- 「跳到第 N 則」的 server 端（`seq → event`）：需要 `(room, seq) → pduid` 反查表。client 現在可以先用 `/messages` 二分逼近；
  要做時是一張小表＋在 §1.2 的同一交易多寫一筆。
- 只算訊息的 `r_seq`。
- 自毀訊息、頻道功能（issue 明列不在內）。

## 4. 驗收

- 單元：計數器同一交易；redact 前後 `r_seq` 不變；`into_outgoing_federation` 剝掉 `r_seq`；k 路合併順序（三個 room 交錯的 count）
  與 byte 上限截斷後 `next` 正確。
- e2e（真伺服器）：兩個 room 各送幾則 → `/sync`、`/messages`、`/context`、`/event`、`/relations`、search 都帶同一個 `r_seq`；redact 後
  `r_seq` 不變；`Event/Recent` 用 WS 拿 limit=5、再用 `next` 翻到耗盡，順序等於送出順序的倒序；被 ignore 的使用者的訊息不出現；
  舊庫啟動後 migration 給每個既有事件編號、bounds 正確。
- 合併後：CHANGELOG 一列、roadmap §2.4 標 ✅（wire-format §3.2、§3.3 已在實作分支同步）。

## 5. 實作落點（給下一個讀的人）

| 什麼 | 哪裡 |
|---|---|
| `R_SEQ_KEY`、`G_SEQ_KEY`、`SeqBounds`（16 byte 編碼）、`Positions`、`get_json_positions`／`set_json_positions`／`remove_json_positions` ＋單元測試 | `src/core/matrix/pdu/seq.rs` |
| 計數表 `roomid_seqbounds` 的讀寫 | `src/service/rooms/timeline/seq.rs`、`src/database/maps.rs` |
| 兩個寫入點 | `timeline/append.rs`（`take_forward`，同交易 `put_seq_bounds`）、`timeline/backfill.rs`（`take_backfilled`） |
| redact 放回 | `timeline/redact.rs` |
| 聯邦送出剝掉 | `src/core/matrix/pdu/format.rs`（`into_outgoing_federation`）＋ `pdu/tests.rs` |
| 一次性回填 | `src/service/migrations/backfill_room_seq.rs`，marker `backfill_room_seq` |
| `Event/Recent` | `src/api/client/wbf/recent.rs`；config `wbf_recent_max_limit` |
| 規格向量 | `docs/design/wbf-vectors.json`：`recent`、`recent_next_page`、`ack_recent` |
