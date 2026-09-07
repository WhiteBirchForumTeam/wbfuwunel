# wbf pack 處理管線：連線就是佇列、handler 的契約、以及 HTTP 一章章搬過來的模子

**狀態**：📄 提案（2026-09-07）。維護者定的決定列在 §0；其餘是照那些決定推出來的做法，等同意再開分支。

[wbf-wire-format.md](wbf-wire-format.md) 講的是**封包的版面與協議**（header、kind、順序類別、兩種送法）；
[chunked-upload.md](chunked-upload.md)、[room-seq-and-recent.md](room-seq-and-recent.md)、wire-format §6.3 講的是**各個 kind 的語意**。
兩者之間少一層：一個 pack 從連線進來到回應出去，經過哪些關卡、handler 拿到什麼、能送出什麼、連線本身有什麼規則。
現在那層散在 `ws.rs` 與 `mod.rs` 的程式碼裡，沒有文件。維護者 2026-09-07 的話：「發明了 protocol 講清楚了封包規格，講清楚架構協議，但沒講設計」。
這份補的就是那層，而且它是**之後每個 HTTP 端點搬到 WS 時要照的模子**（§7）——上傳、登入、`Recent` 只是先走過這條線的三個例子。

同一支分支的第二部分（§8）是 [review-followups-2026-09-06.md](review-followups-2026-09-06.md) 剩下的三條上傳生命週期問題（§2.2、§2.5、§2.8）。
它們碰的檔案跟管線不重疊，放同支是維護者的決定，分開 commit 審。

## 0. 維護者 2026-09-07 定的

| # | 決定 |
|---|---|
| 1 | client 會開**多條** WS，一條專走事件、一條專走媒體，避免大檔分塊上傳把事件排在後面。分工是 **client 的設計，server 不知道、不驗、不管**：每條連線一模一樣。 |
| 2 | **每個 (user, device) 最多 4 條 WS**。超過**踢新的那條**，舊的不動。匿名連線不算，登入（`Login`／`Refresh` 拿到身份那刻）才算。HTTP `/_wbf/v1/pack` 不算。 |
| 3 | 一條連線就是一個 queue：收到的 pack **依序**處理；Ack 是**真的落地**（DB 交易執行完）才回。 |
| 4 | `Event/Recent` **不能一包回一萬條**：first byte 會很久，client 會卡在 DB 同步。WS 上要拆成**很多小 pack** 串流回去，新的 pack 格式 `Event/Batch`（§6）。 |
| 5 | `Recent` 走 HTTP 回 `Error(Unsupported)`。 |
| 6 | `Batch` 的 meta 是 `{ tc, bc, fs, ls, r }`；**`r = 0` 就是結束**，不用 `IS_LAST`。`tc` 要準（先數再送，§6.3），效能實測再說：常態同步 `tc` 很低，只有久未同步才付那筆延遲。 |
| 7 | client 在同步中**不能中止**串流，也收不到新訊息（那條連線被佇列佔著）；要就再開一條。這是 client 的事，server 不做取消指令。 |
| 8 | 這條 checklist 的範圍是**整條 pack 處理流程**，不只上傳下載：登入登出、以及未來每個 HTTP→WS 的新功能都走它。 |

## 1. 模型：一條連線就是一個佇列

```
client ──── WebSocket ────▶ 接收 loop ──▶ 關卡（§3）──▶ 派發 ──▶ handler（§4）
                                                                    │ 0..n 個回應 pack
client ◀─── WebSocket ◀──── 發送 task ◀──── 有界 mpsc ◀─────────────┘
```

- **接收 loop** 一條連線一個（現在的 `serve`）：讀一個 message、走完關卡、交給 handler、**等 handler 結束**才讀下一個。
  依序處理不是效能取捨，是語意：有序類（`Chunk`）的 `OutOfOrder` 判定、`Login` 換身份後「下一個 pack 就是新身份」，都靠它。
- **發送 task** 一條連線一個（**新**，wire-format §5 早寫了、`ws.rs` 一直沒做，因為到 §6 之前沒有 handler 需要送第二個 pack）：
  從有界 `mpsc` 讀 pack、寫進 WebSocket sink。誰要往 client 送東西都經這裡：handler 的回應、關卡的 `Error`、`Close` frame、
  以後 server 主動推的東西。**只有一個寫 sink 的地方**，順序就不會亂（A4：一個路徑起點）。
- **背壓**：`mpsc` 有界（`wbf_ws_send_queue_len`，預設 32 個 pack）。client 收得慢 → 佇列滿 → handler 的 `send().await` 停住 →
  接收 loop 不讀下一個 → TCP 視窗關上。全鏈路靜止，記憶體有上界：一條連線最多佇列長度 × 一個 pack 上限。
- **一條連線不保存任何業務狀態**（wire-format §6.1 已定）：上傳進度在 DB、session 在 `Session` 一個 struct。連線死了什麼都不會丟。

client 那邊「開幾條、哪條走什麼、pending → sending → sent」是 client 的設計，這份不寫。

## 2. 連線的規則

### 2.1 每個 (user, device) 最多 4 條（`wbf_ws_max_connections_per_device`，預設 4，0 = 不限）

計數的 key 是 `Session` 裡的 `(user_id, device_id)`。規則照 §0-2：

| 時刻 | 做什麼 |
|---|---|
| 帶 Bearer 升級 | `authenticate` 過了之後、升級之前，向 `connections` 要一個名額。沒有 → **不升級**，HTTP 回 `429 Too Many Requests`，body 是 `Error(TooManyConnections)`。 |
| 匿名升級 | 不算名額（沒有身份可算）。它最多活 `wbf_ws_unauthenticated_timeout` 秒，不會被拿來囤。 |
| `Login`／`Refresh` 拿到新 `Session` | **先要新名額，再放舊的**。要不到 → 回 `Error(TooManyConnections)`，然後 Close 1008；連線上原本的 session（若有）也一起結束。「踢新的」在這裡的意思是：這次登入沒成，這條線走人；別條線不受影響。 |
| `Refresh` 同一個 (user, device) | 名額不動（是同一個）。 |
| `Logout` | 名額隨連線結束一起放。 |
| 連線結束（任何原因） | 名額放掉。 |

**名額是 RAII**：`connections.take_slot(user, device) -> Option<ConnectionSlot>`，`ConnectionSlot` drop 時減計數。`serve` 把它放在自己的
區域變數裡，loop 怎麼 break 都會放，不靠某條路徑記得減（A5：斷線的路徑太多，靠記憶會漏，而漏的方向是名額永遠回不來 = 那台裝置再也連不上）。
`Login` 換身份時 `Option<ConnectionSlot>` 用 `replace`，舊的在新的拿到之後才 drop。

計數表放 `service/connections.rs`，那裡已經是連線的登記處（`JoinSet` 在那）。表是 `Mutex<HashMap<(OwnedUserId, OwnedDeviceId), u32>>`；
`take_slot` 在鎖下比較與加一，超過就不加，回 `None`。**沒有第二份計數**：連線數的真相只有這張表。

**為什麼不是 per user**：維護者定 per device。同一個人手機加桌機各兩條就是 4，若算 user 第三台裝置會被擠掉；算 device 則上限幾乎不會撞到，
它的意義是防**一個 client 失控**（重連風暴、忘了關），不是配額。

### 2.2 一條連線的一生

```
升級 ──▶ [匿名：只答 Hello/Ping/Login/Refresh；30 秒內要登入] ──Login──▶ [已登入] ──▶ 結束
  │                                                                     │
  └─ 帶 Bearer ───────────────────────────────────────────────────────────┘

結束的原因：client 關、idle 300 秒、匿名逾時（1008）、session 重驗失敗（1008）、Logout（1000）、
            名額不足（1008）、server 關機（1001）、發送佇列的對端死了。
```

沿用 wire-format §6.3.3 的狀態機，只多「名額不足」一個出口。

## 3. 一個 pack 的路：關卡順序

從 WebSocket 讀到一個 message 到 handler 拿到 `PackView`，**固定這個順序**，每個關卡只回答一個問題：

| # | 關卡 | 不過怎麼辦 | 為什麼在這個位置 |
|---|---|---|---|
| 1 | frame 大小（WebSocket 層，`max_message_size`） | WebSocket 層拒收 | 還沒進記憶體就擋，最便宜 |
| 2 | 是 binary 嗎 | Text → `Error(Corrupt)`；Ping/Pong 由 WS 層答；Close → 結束 | 不是 pack 的東西不往下走 |
| 3 | **session 還有效嗎**（`revalidate`：token 仍在、同一人、未到期、未鎖定） | `Error(Unauthorized)` ＋ Close 1008 | 在 decode **之前**：死掉的 session 連送壞封包續命都不行 |
| 4 | decode（版本、旗標、長度、兩個 CRC） | `Error(<PackError 對應碼>)`，`id`/`seq` 只在 DataCrc 錯時從 header 抄 | 到這裡才碰 bytes 內容 |
| 5 | meta／data 不超過設定上限 | `Error(TooLarge)` | decode 之後才知道長度是真的 |
| 6 | **這個 kind 在這個狀態、這個傳輸上准不准** | 匿名連線送非白名單 → `Error(Unauthorized)`；HTTP 送只准 WS 的 kind → `Error(Unsupported)`；沒人認得的 kind → `Error(UnknownKind)` | 一張表回答，不散在 handler 裡 |
| 7 | 派發到 handler（§4） | handler 的 `Reject` → `Error(code)` 同 `id`/`seq` | |
| 8 | 回應進發送佇列；套用 handler 回的 `SessionChange` | `Close` → 送 Close 1000 結束 | 回應**先於** Close 入隊，client 一定先收到 Ack 再收到關線 |

第 6 關現在是兩層 `match` 交錯著做（`handle_pack` 先擋 Session-over-HTTP 與匿名，`handle_authenticated_pack` 再擋 Stream-over-HTTP）。
改成**一張准入表**：

```rust
/// 一個 kind 在哪些狀態、哪些傳輸上可以進 handler。缺項 = 不准（fail closed）。
struct Admission { anonymous_ok: bool, http_ok: bool }
fn admission(kind: Kind, subtype: u8) -> Option<Admission>
```

`Control/Hello`、`Ping`：匿名可、HTTP 可。`Session/*`：匿名可、**HTTP 不可**（現在回 `Conflict`，改 `Unsupported`，§9）。
`Upload/*`、`Download/*`、`Event/Send`：要登入、HTTP 可。`Event/Recent`：要登入、**HTTP 不可**（§0-5）。`Stream/*`：要登入、HTTP 不可。
其餘 `None` → `UnknownKind`。新 kind 搬進來時第一件事就是在這張表加一列（§7）。

HTTP 傳輸走同一條路，只是第 1～3 關換成 HTTP 的（body 大小、Bearer 驗一次），第 8 關的「發送佇列」是一個只裝得下**一個** pack 的 body：
handler 送第二個 pack 就是 bug，由 `Reply` 型別在 HTTP 模式下拒絕（§4.2）。

## 4. handler 的契約

### 4.1 簽名

```rust
async fn handle_xxx(
    services: &Services,
    ctx: &PackContext<'_>,        // 誰、從哪來、走哪個傳輸
    view: &PackView<'_>,          // 已 decode、已過大小關的 pack；meta/data 切片指回原緩衝
    reply: &mut Reply,            // 往這條連線送 pack 的唯一出口
) -> Result<SessionChange, Reject>

struct PackContext<'a> { session: Option<&'a Session>, client: IpAddr, transport: Transport }
```

- **`ctx.session` 是 `Option`**，只有准入表寫 `anonymous_ok` 的 handler 會拿到 `None`；其他 handler 在派發時已保證 `Some`，用一個小函式
  `ctx.user()?` 取，`None` 就 `Reject(Unauthorized)`——不 `unwrap`（A5）。
- **`reply` 是唯一出口**：handler 不拿 sink、不拿 `mpsc::Sender`、不知道自己在 WS 還是 HTTP。它只會 `reply.send(pack).await`。
- **回 `SessionChange`**：`Keep`（絕大多數）、`Replace(Session)`（`Login`／`Refresh`）、`Close`（`Logout`）。這是 handler 唯一能對連線做的事。
- **錯就 `Reject`**：一個 code ＋ 一句話，管線包成 `Error` pack、抄請求的 `id`/`seq`。handler 自己不造 `Error` pack。

### 4.2 `Reply`

```rust
struct Reply { sink: ReplySink, sent: u32 }
enum ReplySink { WebSocket(mpsc::Sender<Bytes>), Http(Option<Vec<u8>>) }

impl Reply {
    /// WS：入發送佇列，佇列滿就等（背壓）。對端死了回 Err(ConnectionGone)，handler 該停。
    /// HTTP：第一個 pack 收下當 body；第二個回 Err(HttpSinglePack)——那是 handler 的 bug，
    /// 管線把它記成 error log 並回 Error(Internal)。
    async fn send(&mut self, pack: Vec<u8>) -> Result<(), ReplyError>
}
```

三種回應形狀都走它：

| 形狀 | 例 | 怎麼用 `reply` |
|---|---|---|
| 一問一答 | `Create`、`Seal`、`Info`、`Login`、`Send` | `send(ack)` 一次 |
| 有序輸入、一答 | `Chunk` | 一樣 `send(ack)` 一次；順序檢查在 handler 之前（上傳服務用 DB 的 `next_seq`） |
| 一問、**有序輸出串流** | `Recent` → `Batch`（§6） | `send` n 次，同 `id`、`seq` 遞增；最後一個帶結束標記（`Batch` 是 `r = 0`） |

### 4.3 五條規則

1. **落地才 Ack**（§0-3）。`Chunk` 的 Ack 在 txn 執行後；`Send` 在事件 append 後；`Login` 在 token 寫入後。handler 裡先 `send` 再寫 DB 是 bug。
2. **handler 不持連線狀態**。要記的東西進 DB 或進 service 層跨連線共用的記憶體（上傳的 `hot_upload`）；「這條連線上一個 pack 是什麼」不存在。
3. **handler 不知道傳輸**。只有准入表（第 6 關）與 `Reply` 知道。一個 handler 若非得問 `transport`，先問是不是該改准入表。
4. **meta 只在需要時 parse，data 不複製**（wire-format §5 的禁令）。`Batch` 的 data 用長度前綴而不是 JSON 陣列，就是為了 client 端這條。
5. **一條連線一次一個 handler**。不 spawn、不並行。慢的 handler（Seal、`Recent` 一萬條）就是會佔住那條連線；那正是 client 分連線的理由（§0-1）。
   將來若要讓 `Ping` 插隊，是改這條規則，不是某個 handler 偷 spawn。

## 5. 發送 task

```rust
// serve() 裡
let (tx, rx) = mpsc::channel::<Outgoing>(config.wbf_ws_send_queue_len);
enum Outgoing { Pack(Bytes), Close(CloseFrame) }
// 一個 task：loop { rx.recv() → sink.send(...) }；收到 Close 送完就結束；sink 錯了就結束並讓 rx 的 sender 端看到 closed。
```

- 接收 loop 持有 `tx`，`Reply::WebSocket` 是它的 clone。接收 loop 結束時 drop `tx`，發送 task 把佇列**送完**再退出：`Logout` 的 Ack、
  `Error(Unauthorized)`、最後的 Close 都不會被截掉。這也是為什麼 Close 走佇列而不是直接寫 sink。
- 關機（`until_shutdown`）：接收 loop 入隊 `Close(1001)` 然後 break；`Services::stop` 的 `close_and_join` 等的是**兩個** task
  （`JoinSet` 裡 `serve` 用 `tokio::join!` 等發送 task）。15 秒的 `JOIN_TIMEOUT` 不變。
- 現在 `ws.rs` 裡每一處 `sink.send(...)` 都改成入隊。改完 `serve` 裡不該再看得到 `sink`。

## 6. `Event/Recent` 串流與 `Event/Batch`

### 6.1 格式

**`0x14 Event / 0x03 Batch`**，只有 server → client，是 `Recent` 的回應（`Recent` 不再用 `Ack` 回）。

| 欄位 | 內容 |
|---|---|
| header | `IS_RESPONSE = 1`；`id` 抄請求；`seq` 從 0 起嚴格 +1；**沒有 `IS_LAST`**（§0-6） |
| meta | `{ "tc": 10000, "bc": 10, "fs": 1243, "ls": 1234, "r": 9990 }` |
| data | `bc` 則事件，每則 **u32 大端長度 ＋ 事件 JSON bytes**，新到舊排（`fs ≥ ls`） |

| meta 欄 | 意思 |
|---|---|
| `tc` | total count：這一輪（這個請求）**總共會送**幾條。每個 Batch 都一樣 |
| `bc` | batch count：這個 Batch 幾條 |
| `fs` | first g_seq：這批第一條（最新那條）的 `g_seq` |
| `ls` | last g_seq：這批最後一條（最舊那條）的 `g_seq` |
| `r` | remain：**這批之後**還剩幾條。`r = 0` → 這輪結束，client 不會再收到這個 `id` 的 Batch |

不變量：每個 Batch `tc = 已送 + bc + r`；最後一個 Batch `r = 0`；`tc = 0` 時送一個 `bc = 0, r = 0` 的 Batch（`fs`、`ls` 為 0）。
事件的 JSON 跟現在一樣（含 `room_id`，`unsigned` 帶 `org.wbftw.wbfuwunel.r_seq`／`g_seq`），見 room-seq-and-recent.md §2。

**為什麼長度前綴不用 JSON 陣列**：client 切事件只看四個 byte，不掃逗號、不先 parse 整段；收一個 Batch 寫一次 DB，同步一條條前進。
**為什麼不設 `WANT_ACK`**：Batch 是 server 產生的有序串流，順序由 WebSocket 保證，背壓由 §1 的佇列與 TCP 保證。client 不回 Ack，server 不等。

### 6.2 `Recent` 請求

meta 多一個 `batch`：每個 Batch 幾條，預設 `wbf_recent_default_batch`（10）、上限 `wbf_recent_max_batch`（100），超過夾。
`limit`（總上限，預設與上限 `wbf_recent_max_limit` = 10000）、`cg_seq`、`before` 語意不變。一個 Batch 的 data 另外受 `wbf_data_max_bytes` 限：
放不下第 n 條就提早結束這個 Batch（`bc < batch`），那條進下一個 Batch；**單一事件比 pack 還大**照現在的做法跳過並 `debug_warn!`，
而且**數的那趟也跳過它**，不然 `tc` 對不上。

### 6.3 兩趟：先數再送（維護者定 `tc` 要準）

`g_seq` 是全站計數器，跨這個人沒加入的房間，`latest − cg_seq` 是上界不是條數；準確的 `tc` 只能把跨房合併＋可見性過濾走一遍。

```
latest = 現在的全站計數（合併前讀，兩趟共用；之後 append 的事件不在這輪，下輪 cg_seq 會涵蓋）
第一趟：同一個堆合併、同一個可見性過濾、同一個 limit 與 cg_seq 停止條件、同一個「太寬就跳過」規則 —— 只數，不序列化   → tc
第二趟：再跑一次，每 batch 條 send 一個 Batch，r = tc − 已送
```

兩趟的**停止條件與過濾規則是同一段程式**（一個 `recent_events(...) -> impl Stream<Item = Pdu>`，兩趟各消費一次），不然 `tc` 遲早跟第二趟對不上。
第二趟數到的若比 `tc` 少（中間被 redact 成不可見之類），最後一個 Batch 的 `r` 仍要是 0：`r = max(tc − 已送, 0)`，寧可 `tc` 高估、串流一定收得到尾。
多出來的（第二趟比 `tc` 多）不送，`limit` 已經夾住。

**效能**：維護者定「先實測再說」。常態同步 `tc` 個位數到幾十，第一趟可忽略；久未同步一萬條，第一趟讀的 key 第二趟大多命中 RocksDB 快取。
驗收（§10）要量 first byte 與整輪時間，數字寫回這一節。

### 6.4 client 的水位（寫在這裡是因為 server 的欄位語意決定了它）

- 串流走完（`r = 0`）而且沒撞到 `limit`（`tc < limit`）：把 `cg_seq` 存成**第一個 Batch 的 `fs`**——這輪最新的一條；比它新的下輪會來。
- 撞到 `limit`（`tc == limit`）：**`cg_seq` 不動**，拿最後一個 Batch 的 `ls` 當 `before` 再問一輪；一路問到某輪 `tc < limit` 才推水位。
  `tc` 剛好等於 `limit` 又剛好沒有更舊的，多問一輪會拿到 `tc = 0`，一個來回的代價，不另加欄位。
- 串流中途斷線或收到 `Error`：已收到的 Batch 有效；用最後一個 Batch 的 `ls` 當 `before` 續問，不必重來。**不要**在中途推水位：
  回應是新到舊，第一批之後還有比舊 `cg_seq` 新的事件沒到。

### 6.5 HTTP

`Recent` 走 `POST /_wbf/v1/pack` → `Error(Unsupported)`，訊息「Event/Recent streams; use the WebSocket channel」。
e2e 腳本要看 `Recent` 的結果就開 WS 收 Batch（e2e9 現在用 HTTP 打 `Recent` 的地方要改）。

## 7. HTTP → WS 的模子：搬一個端點的步驟

wire-format §3.3 已經把 kind 按 Matrix 章節占好號。搬一個端點 = 填一次這張清單，**順序不變**：

| 步 | 做什麼 | 落點 |
|---|---|---|
| 1 | 決定 subtype 號、回應形狀（一問一答／有序輸入／串流輸出）、HTTP 准不准、匿名准不准 | wire-format §3.2 加一列；本文 §3 的准入表加一列 |
| 2 | meta = 那個端點 ruma 的 request 型別 serde 成 JSON；回應 = ruma 的 response 型別；有 bytes 本體的（媒體）放 data | 不另造欄位。`Login` 就是這樣做的：`IncomingRequest::try_from_http_request` 重用 ruma 的解析 |
| 3 | 把業務邏輯抽到 `service/`，HTTP route 與 pack handler **都呼叫它** | `users::login::issue_session` 是範本：HTTP `/login` 與 `Session/Login` 共用一份 |
| 4 | handler 照 §4.1 簽名寫；`Reject` 對應 Matrix 錯碼（`M_LIMIT_EXCEEDED` → `RateLimited` 之類，`Reject::from(Error)` 那張表補齊） | `src/api/client/wbf/<kind>.rs`，一個 kind 一個檔 |
| 5 | 向量：請求與回應各至少一個進 `wbf-vectors.json` | `core/wbf/vectors.rs` |
| 6 | e2e：HTTP 與 WS 各打一次同一個操作，結果一致；HTTP 不准的驗 `Unsupported` | `tests/e2e/` |
| 7 | 限速、鎖定、暫停：HTTP 那邊有的關卡 WS **一條都不少**，寫在 service 層（`check_login_rate` 的位置）而不是 route 層，兩個入口才會一起有 | |

**不搬的**：`/sync`（long-poll 的語意在 WS 上是 server 推，另一個提案）、媒體 `GET` 下載（已有 `Download`）。

## 8. 第二部分：上傳生命週期（review-followups §2.2、§2.5、§2.8）

同支、分開 commit。做法照 review-followups 那三節，這裡只記順序與跟管線的關係：

| 順序 | 條 | 一句話 | 跟管線的關係 |
|---|---|---|---|
| 1 | §2.5 | `upload_status` 拿 `upload_locks`；sweeper 鎖下重讀 `last_chunk_at_secs` | 無；純 service 層 |
| 2 | §2.2 | Seal 永遠串流 `put_multi`，塊大小用 provider 的 `multipart_part_size`（本地夾 8 MiB） | Seal 是慢 handler 的代表，§4.3-5 說的「佔住連線」就是它；驗收要在 WS 上量 |
| 3 | §2.8 | sweeper 多掃「宣告在、進度不在、超過 TTL」的舊上傳 | 無。**維護者先答**：有沒有跑過 #16～#18 之間的版本？沒有就只是防禦 |

## 9. 版本與相容

`protocol` 號不動（舊 client 不送 `batch`，`Batch` 是它沒收過的 subtype；而且現在沒有舊 client 在跑）。**但下面三條是行為改變，CHANGELOG 要點名**：

| 改變 | 影響 |
|---|---|
| `Recent` 在 WS 上的回應由一個 `Ack` 變成 `Batch` 串流 | wbf client 要改收法。Matrix 端點不受影響 |
| `Recent` 走 HTTP 由「回一頁」變 `Error(Unsupported)` | 只影響測試腳本 |
| `Session/*` 走 HTTP 的錯碼由 `Conflict` 改 `Unsupported` | 錯碼改變；`Unsupported` 是新碼，語意「這個 kind 不走這個傳輸」 |

新錯碼兩個：`Unsupported`、`TooManyConnections`（wire-format §3.2 Error 那列補）。
新 config 四個：`wbf_ws_max_connections_per_device`（4）、`wbf_ws_send_queue_len`（32）、`wbf_recent_default_batch`（10）、`wbf_recent_max_batch`（100）。

## 10. 驗收（e2e7 加情境 5、e2e9 改）

- **名額**：同一 device 開 4 條 WS 都活；第 5 條升級 429 ＋ `Error(TooManyConnections)`；關一條再開就成；另一個 device 不受影響；
  匿名開 5 條都活，第 5 條 `Login` 回 `TooManyConnections` 並被 Close 1008，其他四條的 session 正常。
- **名額回收**：4 條全關再開 4 條成功（RAII 有放）；一條在上傳中被 server 關機關掉，重啟後名額是 0（表在記憶體，重啟即清）。
- **串流**：灌 100 條 → `Recent(batch=10)` 收到 10 個 Batch，`tc = 100`、`r` 遞減到 0、`fs`/`ls` 單調遞減、每則事件長度前綴對得上；
  `batch=1000` 被夾成 100；`limit=30` → `tc = 30`；沒新事件 → 一個 `bc = 0, r = 0`。
- **first byte**：灌 10000 條，量「送出 `Recent` 到收到第一個 Batch」與「到 `r = 0`」的時間，記進 §6.3。這條是量測不是門檻，數字先看再定門檻。
- **背壓**：client 送 `Recent` 後停止讀 socket 5 秒，server 的 working set 不隨時間增長（有界佇列擋住了），恢復讀之後串流補完。
- **順序**：同一連線送 `Recent` 緊接 `Ping`，Pong 在最後一個 Batch **之後**（一次一個 handler）。
- **HTTP**：`Recent`、`Login` 打 `/pack` 都回 `Unsupported`。
- **關機**：一條連線在收 Batch 中途 `!admin server shutdown`，client 收到的最後一個 frame 是 Close 1001，程序乾淨退出。
- 第二部分照 review-followups §2.2／2.5／2.8 各自的驗收。

## 11. 落點

| 東西 | 檔 |
|---|---|
| 名額表、`ConnectionSlot`、`take_slot` | `src/service/connections.rs` |
| 發送 task、`Outgoing`、接收 loop 改入隊、名額拿放 | `src/api/client/wbf/ws.rs` |
| `PackContext`、`Reply`、`ReplySink`、准入表 `admission`、`Unsupported`／`TooManyConnections` | `src/api/client/wbf/mod.rs` |
| `Batch` 編碼、兩趟 `recent_events`、`batch` 欄 | `src/api/client/wbf/recent.rs`（`recent_events` 若超過一屏就抽到 `service/rooms/timeline/recent.rs`） |
| kind 表、Error 列、§5 改成「已實作」、§6.1 補名額 | `docs/design/wbf-wire-format.md` |
| `Batch` 事件格式、水位規則 | `docs/design/room-seq-and-recent.md` §2 |
| 向量 `recent_batch`、`batch_first`、`batch_last`、`error_unsupported`、`error_too_many_connections` | `src/core/wbf/vectors.rs`、`docs/design/wbf-vectors.json` |
| 四個 config | `src/core/config/mod.rs`、`tuwunel-example.toml` |

## 12. 我不滿意或想再談的

- **`tc` 高估時的尾巴**：§6.3 的 `r = max(tc − 已送, 0)` 保證串流收得到尾，但 client 會看到進度條在 `r > 0` 時突然結束（最後一個 Batch `bc` 小、`r` 直接 0）。
  發生條件是兩趟之間有事件變不可見，很少；接受。
- **一條連線一次一個 handler** 讓 `Recent` 一萬條期間那條連線連 `Ping` 都不回。維護者定這是 client 用多連線解的事。若之後 idle timeout 因此誤殺
  （Batch 一直在送，接收方向卻 300 秒沒東西），要把 idle 的定義改成「兩個方向都沒動」——現在先不改，等實測。
- **名額表只在記憶體**：多節點部署各自算。這個 fork 單機，先不管。
- **`Reply` 在 HTTP 上只裝一個 pack** 是用型別擋 handler 的錯，不是功能。將來若真要 HTTP 回多個 pack（body 串接），改 `ReplySink::Http` 一處就好。
