# 每房連續序號 `seq` 與跨房間「全域最近 N 則」

> **這份文件回答：client 的聊天模型要 server 配合的兩件事，server 端怎麼做、為什麼這樣做。**
> 需求出處：issue #20（wbf-matrix-client `docs/design/chat-model.md` §4.3、§7）。
> 狀態：📄 提案，2026-09-05，等維護者同意後開實作分支。
> 相關：[wbf-wire-format.md](wbf-wire-format.md)（pack 與 kind 分配）、[roadmap.md](roadmap.md)。

## 0. 一句話

1. **`seq`**：每個 room 一個計數器，新事件 1、2、3…，聯邦補回的舊歷史 0、−1、−2…；**寫進存起來的 PDU JSON 的
   `unsigned["org.wbftw.wbfuwunel.seq"]`**，所以每一條把事件交給 client 的路徑自動都帶，不用逐條路徑補。
2. **全域最近 N 則**：pack `Event/Recent`（kind `0x14`、subtype `0x01`）。server 對使用者加入的每個 room 開一條倒序串流，
   用**既有的全域 count** 做 k 路合併，套 `/messages` 同一套可見性過濾。**不加索引、不加遷移**，游標就是全域 count。

## 1. `seq`：每房連續序號

### 1.1 為什麼寫進存起來的 JSON，而不是讀的時候補

issue 列了七條要帶 `seq` 的路徑（sync、messages、context、event、relations、timestamp_to_event、search）。它們最後全都讀
`pduid_pdu` 這一張表；`age` 現在是讀的時候在 `timeline/pdus.rs` 的 `each_pdu` 與 `room/event.rs` 兩處補的。如果照 `age` 的做法，
`seq` 要另開一張 `pduid → seq` 表，而且 `each_pdu` 是同步函數、要改成 async 才能點讀。

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

- 同一 room 內唯一且單調：新 append 一定大於之前所有正號。
- 正號發出去**永不重排**；backfill 只碰負號。
- state 事件也算：所有進 timeline 的 PDU 都編號（與 PduId 的 count 語意一致，server 端最簡單）。
  「只算訊息」若之後要，client 那邊不受影響，server 多分一層即可，現在不做。
- 型別 i64，寫進 JSON 是整數。

### 1.3 現有 room 的一次性回填

startup migration `backfill_room_seq`（`src/service/migrations/`，照既有 `rebuild_roomid_tscount_pducount` 的形狀，`global` 表的
marker 保證只跑一次）。對每個 room 走 `timeline.pdus(None, room, None)` 正序：Normal 的按順序從 1 編、Backfilled 的從 0 往下編，
改寫每一筆 `pduid_pdu` 的 `unsigned`，最後寫 `roomid_seqbounds`。

這是一次全表重寫。自架規模可接受；migration 跑在開始服務之前，不需要鎖。跑過的 room client 直接有號；沒跑到的（不會有，
但防禦性地）client 看不到 key 就退化，不會壞。

### 1.4 redact 會剝掉 `unsigned`

ruma 的 `redact_in_place` 只留規格允許的 key，`unsigned` 不在裡面，所以 redact 後 `seq` 會消失。`redact.rs` 在 redact 之後本來就
會塞 `unsigned.redacted_because`；**同一處**先把 `seq` 讀出來，redact 完放回去。issue 要求「被 redact 的事件保留原號」就是這條。
測試釘住：redact 前後 `seq` 相同。

### 1.5 聯邦送出要剝掉

`core/matrix/pdu/format.rs` 的 `into_outgoing_federation` 現在只從 `unsigned` 移掉 `transaction_id`；加一行移掉 `org.wbftw.wbfuwunel.seq`。
它是本站的編號，對別站沒意義；`allow_federation=false` 時本來也送不出去，但寫入點只有一個，不要靠設定值當防線。

### 1.6 不動的

- `eventid_outlierpdu`（outlier 不在 timeline，沒號）。
- `roomid_tscount_pducount`、PduId、PduCount：全域 count 照舊，`seq` 是另一個數，不取代 token。
- `/messages` 的 `from`/`to` 仍是 token；「跳到第 N 則」client 用 `seq` 找對應事件是 client 端的事（§3 列為候選）。

## 2. `Event/Recent`：跨房間全域最近 N 則

### 2.1 wire

kind `0x14 Event`（wire-format §3.3 已分配給 send／messages／context 這個領域），subtype **`0x01 Recent`**。無序類（請求號對回應）。

請求 meta（JSON，明文，server 要讀）：

```json
{ "limit": 10000, "before": "<游標，可省略>" }
```

回應 `Ack`（`IS_RESPONSE`，`id`、`seq` 抄請求）：

- meta：`{ "count": <本次筆數>, "next": "<游標>" | null }`。`next` 為 `null` 表示再往前沒有了。
- data：**JSON 陣列**，每個元素是完整的事件（`Pdu` 格式，含 `room_id`，`unsigned` 含 `seq`）。

data 由 server 填是既有先例（`Download/Read` 的回應 data 是讀出的 bytes）。不另外包 `{room_id, seq, event}` —— 事件本身
已經帶這兩個欄位，包一層是重複。

游標 = **全域 `PduCount` 的字串形式**（跟 `/messages` 的 token 同一個東西），client 不解讀、原樣帶回。

### 2.2 演算法：k 路合併，不加索引

`pduid_pdu` 的 key 是 `(shortroomid, count)`，沒有全域順序的索引；但 **count 是全域發的**（`globals.next_count`），跨 room 可以直接比大小。

```
rooms = state_cache.rooms_joined(user)
每個 room 開 timeline.pdus_rev(Some(user), room, before)   ← 倒序、從游標往舊
BinaryHeap 以 count 為鍵，每次彈最大的、再從那條串流補一個
每彈一個：ignored_filter → visibility_filter（api/client/message.rs 既有的兩個，pub(crate)）
停：滿 limit，或 data 再放一個就超過 wbf_data_max_bytes，或全部串流耗盡
next = 最後一個放進去的 count；耗盡則 null
```

- 為什麼不加 `count → pduid` 全域索引：那要一張新表、一次遷移、還要每個 append 多一筆寫；而且使用者只在少數 room 裡時，倒著掃全域
  索引大多在跳別人的房間。k 路合併只讀該使用者的 room，每個 room 一條既有串流，堆的大小 = room 數（幾十到幾百）。
- Backfilled 的事件 count 為負，會排在所有 Normal 之後 —— 它們是「本站知道這個 room 之前」的歷史，排在最舊那邊是對的。
- 可見性照 `/messages`：`history_visibility`、ignore、離開後看不到之後的，都在那兩個 filter 裡；`rooms_joined` 只給加入中的 room
  （issue 的「加入的所有 room」）。E2EE 密文原樣回。
- 上限：新 config `wbf_recent_max_limit`（預設 10000），超過 clamp 不報錯；另有 data 的 byte 上限兜底（10000 則 E2EE 事件可能超過
  16 MiB），所以一次可能回不滿 10000，client 用 `next` 接著翻 —— 這跟「往更舊翻」是同一個動作。

### 2.3 HTTP

`POST /_wbf/v1/pack` 同一個 pack 就能走，不另開 GET。issue 說 GET「也可以」，pack 才是必要的；少一條路徑少一份漂移。

### 2.4 規格向量

`wbf-vectors.json` 加一組 `Event/Recent` 請求與回應的外框（header、meta），data 用兩個最小事件。client repo 複製一份寫測試。

## 3. 不在這次裡（候選，另開）

- 「跳到第 N 則」的 server 端（`seq → event`）：需要 `(room, seq) → pduid` 反查表。client 現在可以先用 `/messages` 二分逼近；
  要做時是一張小表＋在 §1.2 的同一交易多寫一筆。
- 只算訊息的 `seq`。
- 自毀訊息、頻道功能（issue 明列不在內）。

## 4. 驗收

- 單元：計數器同一交易；redact 前後 `seq` 不變；`into_outgoing_federation` 剝掉 `seq`；k 路合併順序（三個 room 交錯的 count）
  與 byte 上限截斷後 `next` 正確。
- e2e（真伺服器）：兩個 room 各送幾則 → `/sync`、`/messages`、`/context`、`/event`、`/relations`、search 都帶同一個 `seq`；redact 後
  `seq` 不變；`Event/Recent` 用 WS 拿 limit=5、再用 `next` 翻到耗盡，順序等於送出順序的倒序；被 ignore 的使用者的訊息不出現；
  舊庫啟動後 migration 給每個既有事件編號、bounds 正確。
- 合併後：CHANGELOG 一列、roadmap §2 加一項、wire-format §3.3 的 Event 那列標上 `0x01 Recent`。
