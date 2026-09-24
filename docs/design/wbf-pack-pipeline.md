# wbf pack 處理管線：連線就是佇列、handler 的契約、以及 HTTP 一章章搬過來的模子

**狀態**：§1–§6 ✅ 已實作（PR #33，2026-09-08 合併；之後發送佇列的 bytes 預算 PR #50、窗的 bytes 上限 PR #53）；§8 第二部分 ✅ PR #48（原本停在「🔲 未開」，2026-09-14 補標）。維護者定的決定列在 §0；其餘是照那些決定推出來的做法。
實作跟提案不同的地方標 📎，理由就地寫。client 端要跟的東西開在 wbf-matrix-client #15。

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
| 6 | `Batch` 的 meta 是 `{ tc, bc, fs, ls, r }`；**`r = 0` 就是這一窗結束**，不用 `IS_LAST`、不另做結束用的 pack（先想過一個「蓋子」subtype，後定省掉：`tc` 一開始就給了，尾巴不需要第二個信號）。📎 PR #53 加了 `more`：它不是第二個「這窗完了」，是「**這窗後面還有沒有**」—— 它原本靠 `tc < limit` 推得出來，窗有了 bytes 上限之後推不出來了（§6.3）。 |
| 7 | client 在同步中**不能中止**串流，也收不到新訊息（那條連線被佇列佔著）；要就再開一條。這是 client 的事，server 不做取消指令。 |
| 8 | 這條 checklist 的範圍是**整條 pack 處理流程**，不只上傳下載：登入登出、以及未來每個 HTTP→WS 的新功能都走它。 |
| 9 | **同步是 client 拉的視窗**（維護者 2026-09-07 晚）：一次 `Recent` 只拿 `limit` 條（例 320 = 32 個 pack × 10 條），server 只數這一窗、`tc` 是這一窗的條數；client 收完一窗再帶 `before` 叫下一窗，一萬條由 client 自己累計。server **不記串流狀態**、不另起 `Continue` subtype：`before` 游標本來就是 continue。節流完全在 client。 |
| 10 | client 約定：送出 `Recent` 後若一段時間沒收到回應就自己斷線重連（第一窗可以等長一點，例 60 秒，之後例 10 秒）。**server 不做事**，但這給 first byte 立了門檻（§6.3）。 |
| 11 | `wbf_recent_max_limit` 從 10000 **壓到 500**：一窗要多大是 client 決定的，不夠就帶 `before` 再要一段，本質是「拿固定區間的一段」；一萬太龐大。 |
| 12 | **HTTP `/_wbf/v1/pack` 之後是 debug／fallback 用，WS 才是主力。** 准入表裡「HTTP 不可」的 kind 只會變多。 |
| 13 | 這套 server／client binary **從未上線**，只在本機 debug 過：沒有 #16～#18 之間的庫存在，review-followups §2.8 是純防禦，排最後。 |

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
- **背壓**：佇列有**兩個**界（`PackQueue`，PR #50）：包數 `wbf_ws_send_queue_len`（預設 32）與 bytes `wbf_ws_send_queue_bytes`（預設 16 MiB），誰先用完誰擋。client 收得慢 → 佇列滿 → handler 的 `send().await` 停住 →
  接收 loop 不讀下一個 → TCP 視窗關上。全鏈路靜止，記憶體有上界：**一條連線最多 `wbf_ws_send_queue_bytes`**，加上正在寫出與正在收的那兩個 pack（§5）。
  📎 這裡原本寫「佇列長度 × 一個 pack 上限」—— 那是數包數時代的界，32 × 16 MiB ≈ 512 MiB。
- **一條連線不保存任何業務狀態**（wire-format §6.1 已定）：上傳進度在 DB、session 在 `Session` 一個 struct。連線死了什麼都不會丟。

client 那邊「開幾條、哪條走什麼、pending → sending → sent」是 client 的設計，這份不寫。

## 2. 連線的規則

### 2.1 每個 (user, device) 最多 8 條（`wbf_ws_max_connections_per_device`，預設 8，0 = 不限）

📌 **預設從 4 改成 8**（維護者 2026-09-24）。理由沒變（§2.1 末段：防一個 client 失控，不是配額），只是 4 對「手機＋桌機＋平板，每台開幾條分工」來說太緊。

計數的 key 是 `Session` 裡的 `(user_id, device_id)`。規則照 §0-2：

| 時刻 | 做什麼 |
|---|---|
| 帶 Bearer 升級 | `authenticate` 過了之後、升級之前，向 `connections` 要一個名額。沒有 → **不升級**，HTTP 回 `429 Too Many Requests`，body 是 `Error(TooManyConnections)`。 |
| 匿名升級 | **不算這個名額**（沒有身份可算）。它最多活 `wbf_ws_unauthenticated_timeout` 秒。⚠️ 「不會被拿來囤」這句話在 §2.2 之前是**靠時限撐著的** —— 30 秒內開幾條沒有上限。擋它的是 §2.2 的位址名額，不是這一道。 |
| `Login`／`Refresh` 拿到新 `Session` | **先要新名額，再放舊的**。要不到 → 回 `Error(TooManyConnections)`，然後 Close 1008；連線上原本的 session（若有）也一起結束。「踢新的」在這裡的意思是：這次登入沒成，這條線走人；別條線不受影響。📎 **名額要在 users service 寫任何 token 之前拿**：Matrix 語意下同一 device 再登入會換掉它的 access token，若先發 token 再拒，被拒的 Login 已經把該裝置其他連線的 token 廢了。所以 `issue_session`／`refresh_session` 多一個 `admit: &mut dyn FnMut(&UserId, &DeviceId) -> Result` 閘門，在裝置定下來、token 還沒寫時問一次；HTTP 路由傳 `admit_any`。 |
| `Refresh` 同一個 (user, device) | 名額不動（是同一個）。 |
| `Logout` | 名額隨連線結束一起放。 |
| 連線結束（任何原因） | 名額放掉。 |

**名額是 RAII**：`connections.take_slot(user, device) -> Option<ConnectionSlot>`，`ConnectionSlot` drop 時減計數。`serve` 把它放在自己的
區域變數裡，loop 怎麼 break 都會放，不靠某條路徑記得減（A5：斷線的路徑太多，靠記憶會漏，而漏的方向是名額永遠回不來 = 那台裝置再也連不上）。
`Login` 換身份時 `Option<ConnectionSlot>` 用 `replace`，舊的在新的拿到之後才 drop。

計數表放 `service/connections.rs`，那裡已經是連線的登記處（`JoinSet` 在那）。表是 `Mutex<HashMap<(OwnedUserId, OwnedDeviceId), u32>>`；
`take_slot(user, device, max)` 在鎖下比較與加一，超過就不加，回 `None`；`max = 0` 表示不限、不佔格。**沒有第二份計數**：連線數的真相只有這張表。
📎 名額掛在 `Session` 上（`Session.slot`），不是 `serve` 的區域變數：session 被換掉時舊名額跟著舊 session 一起放，同裝置再登入時新 session 用 `inherit_slot` 接手舊的。

**為什麼不是 per user**：維護者定 per device。同一個人手機加桌機各兩條就是 4，若算 user 第三台裝置會被擠掉；算 device 則上限幾乎不會撞到，
它的意義是防**一個 client 失控**（重連風暴、忘了關），不是配額。

### 2.2 每個來源位址最多 40 條（`wbf_ws_max_connections_per_address`，預設 40，0 = 不限）（✅ 維護者 2026-09-24 同意）

§2.1 那道閘門按 `(user, device)` 算，所以它**只擋得到已經有身份的連線**。⭐ **匿名升級完全不佔它的名額** —— 一個沒有 token 的人要開幾條就開幾條，只受 `wbf_ws_unauthenticated_timeout`（30 秒）限制。
30 秒對一台機器來說很長：只要持續重連，未登入的連線數就沒有上限，而每一條都要佔一個 socket、一個 task、一份讀寫緩衝。**這一節就是補那個洞**，而且它是唯一能補的那道 —— 匿名連線沒有身份，但它一定有一個來源位址。

| 時刻 | 做什麼 |
|---|---|
| 任何 WS 升級（**匿名或帶 Bearer 都算**） | 在**認證之前**向 `connections` 要一個位址名額。沒有 → **不升級**，HTTP 回 `429`，body 是 `Error(TooManyConnectionsFromAddress)`。 |
| 連線結束（任何原因） | 名額放掉。 |
| HTTP pack（`POST /_wbf/v1/pack`） | 🚫 不算。它不是長連線，一問一答就結束（跟 §2.1 同一個理由）。 |

⭐ **兩道閘門的順序是「先位址、後 token」**：位址名額只看一張記憶體表，token 要讀資料庫。**擋得掉的連線不該先讓它花一次 DB 讀** —— 這跟 `ws_route` 開頭那句「正在關機的 server 不欠任何人一次資料庫讀」是同一個理由。
⭐ **兩道都要過**：位址名額過了仍可能被 §2.1 擋下，兩者各擋各的，不互相取代。

**實作跟 §2.1 同一個形狀**（A4：同一個問題只留一份機制）：`service/connections.rs` 多一張 `Mutex<HashMap<IpAddr, u32>>` 與一個 RAII 的 `AddressSlot`，drop 時減計數。
名額在 `ws_route` 裡拿，**放進 `on_upgrade` 的 closure**，所以連線怎麼結束都會還；升級沒成（token 壞、§2.1 擋下、關機中）時它在 `ws_route` 結束就 drop，不會漏。

**位址從哪來**：`ws_route` 已經收 `ClientIp(client)`，那是**傳輸層照 `ip_source` 解析好的 client IP**（`router/client_ip.rs`）。⭐ 這一節不自己解析任何 header —— 解析點只有一個，在那裡（A4）。

🚨 **部署上最容易踩的一個坑，要寫進設定說明**：如果 server 前面有反向代理，而 `ip_source` **沒有**設成讀轉發 header，那麼**每一條連線看起來都來自代理那一個位址** —— 40 就不是「每個使用者 40」，而是**整台 server 只能有 40 條**。
所以這個功能跟 `ip_source` 是綁在一起的：有代理就一定要設。**啟動時會檢查並留一行 warning**（`warn_wbf_address_limit_without_ip_source`）—— 🚫 不擋啟動，直面 client 的部署是合法的（P 條）。

#### 維護者 2026-09-24 定的四條

1. **錯誤碼新開 `TooManyConnectionsFromAddress`（1403）**，不沿用 1402。詞表的規矩是「一個 code 對一種處置」（wire-format §3.4），而 1402 的處置是**「關掉一條你自己的舊連線」** —— 那對位址名額是**錯的建議**：撞到上限的人可能一條都不是他開的（同一個 NAT、同一間辦公室、同一台代理），他關不掉別人的。訊息因此**刻意不說「關掉一條你的」**，只說等或換網路。
2. **IPv6 按 `/64` 聚合**（IPv4 仍按確切位址）。理由：一般家用 IPv6 會拿到一整個 `/64`（甚至 `/56`），**換一個位址是零成本的** —— 按確切位址算等於在 IPv6 下沒有上限。實作是 `to_address_group`（`service/connections.rs`）。
3. **沒有豁免**：loopback 與信任網段照樣計入。多一條豁免就多一條要驗的路徑。
4. **沒設 `ip_source` 時啟動留 warning**，不擋啟動。

📎 **撞到上限的處置**（維護者同日明訂）：**拒新的，舊的照常跑**。升級根本不發生 —— 429 帶一個 `Error` pack，沒有 WebSocket 被建立，所以也沒有「關掉哪一條」的問題。跟 §2.1 「踢的永遠是新的那條」同一個方向。


#### 驗收

- e2e：同一個位址開到第 41 條被 `429` ＋ `Error(TooManyConnectionsFromAddress)` 拒；關掉一條之後可以再開一條（名額真的有還）。
- **匿名也要算**：不帶 Bearer 開到上限一樣被拒 —— 這是整個功能的理由，不驗等於沒做。
- **兩道閘門各擋各的**：位址沒滿但裝置滿了 → `TooManyConnections`（1402）；位址滿了 → 1403。
- HTTP pack 不受影響：位址名額滿的時候，同一個位址打 `POST /_wbf/v1/pack` 照常。

### 2.3 一條連線的一生

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
| 6.5 | **`id` 的型別是這個 `(kind, subtype)` 要的嗎**（[wbf-wire-format.md](wbf-wire-format.md) §2.2，PR #47） | 型別不符、表沒定義的 byte、以及「沒有會話卻帶值」→ `Error(InvalidRequest)` | 跟第 6 關同一張表的同一列（多一欄 `id_type`）：⭐ 兩張以同一個 key 索引的表遲早漂 |
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
  📎 實作：一問一答的 handler（`Upload/*`、`Download/*`、`Event/Send`）保留原本 `-> Result<Vec<u8>, Reject>` 的形狀，由派發者 `handle_one_reply` 統一送進 `reply`；
  只有會送多個 pack 的（`Recent`）與會改 session 的（`Session/*`）拿 `reply`／`ctx`。八個 handler 為了一致改簽名是儀式，出口仍然只有一個。
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
// serve() 裡（實作在 src/service/streams/mod.rs 的 PackQueue）
let (queue, rx) = PackQueue::new(config.wbf_ws_send_queue_len, config.wbf_ws_send_queue_bytes);
enum Outgoing { Pack(Vec<u8>), Close { code, reason } }
struct Queued { outgoing: Outgoing, _room: Option<OwnedSemaphorePermit> }  // 額度跟著 pack 排隊
// 一個 task：loop { rx.recv() → sink.send(item.outgoing) → drop(item) 還額度 }；收到 Close 送完就結束；sink 錯了就結束。
```

⭐ **額度在送出任務寫完、drop 掉這個 item 之後才還**，所以這個數字是「這條連線現在握著多少」，不是「它可以塞進去多少」。
三個 fail-closed 的決定：close frame 成本 0（滿了也要掛得了電話）；比整個預算還大的 pack **拒絕而不是等**（`QueueError::TooLargeForBudget`，設定上由 `check_wbf_send_queue_bytes` 讓它不可能發生）；push 放不下就丟並記 `gap`。

- 接收 loop 持有 `tx`，`Reply::WebSocket` 是它的 clone。接收 loop 結束時 drop `tx`，發送 task 把佇列**送完**再退出：`Logout` 的 Ack、
  `Error(Unauthorized)`、最後的 Close 都不會被截掉。這也是為什麼 Close 走佇列而不是直接寫 sink。
- 關機（`until_shutdown`）：接收 loop 入隊 `Close(1001)` 然後 break；`Services::stop` 的 `close_and_join` 等的是**兩個** task
  （`JoinSet` 裡 `serve` 用 `tokio::join!` 等發送 task）。15 秒的 `JOIN_TIMEOUT` 不變。
  📎 實作：`serve` 結束時 drop 掉所有 sender，再等發送 task 最多 `DRAIN_TIMEOUT`（5 秒）把佇列寫完；對端不讀就 abort 它、socket 隨之關掉。
  發送 task 只持有 socket 的 sink，不借 `Services`，所以它比 `serve` 晚一點結束也不會懸空。
  📎 Close frame 入隊也有同一個上限（`enqueue_close`，PR #33 review，rumia）：佇列滿且對端不讀時，接收 loop 不會為了送 Close 永遠等，5 秒後直接結束、socket drop。
  最壞的記憶體界：一條連線 **`wbf_ws_send_queue_bytes`（預設 16 MiB）**，加上正在寫的那個與正在收的那個。
  ⚠️ **原本這個界是「幾個 pack」而不是「幾 bytes」**（`wbf_ws_send_queue_len × wbf_data_max_bytes` ＝ 32 × 16 MiB ＝ **512 MiB**，而且這行字就這樣寫著、沒有人把它乘出來）——
  一個 client 只要要求 32 個大塊再慢慢讀，就能讓一條連線扣住半 GB。現在包數與 bytes 兩個界誰先用完誰擋；包數留著是因為有些東西本來就是按包數算的（`Device/Fetch` 的一窗）。
  📎 同一次把 `wbf_data_max_bytes` 從 16 MiB 降到 **2 MiB**：那樣一個滿包（meta 64 KiB ＋ data 2 MiB ＋ 32 byte 外框 ＝ 2,166,816 bytes）在 16 MiB 的預算裡放得下 **7** 個，pipeline 仍然成立。⚠️ 反過來，**data 上限若留在 16 MiB，一個滿包（16,846,880 bytes）根本放不進 16 MiB 的預算** —— 那個組合啟動檢查會直接拒絕，而不是「只放得下兩個」（維護者 2026-09-13）。
- 現在 `ws.rs` 裡每一處 `sink.send(...)` 都改成入隊。改完 `serve` 裡不該再看得到 `sink`。

## 6. `Event/Recent` 串流與 `Event/Batch`

### 6.1 一次 `Recent` 是一窗

client 送 `Recent { rooms?, limit, cg_seq, before?, batch? }`：

| 欄 | 意思 | 預設／上限 |
|---|---|---|
| `rooms` | 只讀這幾個房間（PR #51）。⭐ **一個房 ＋ `before` 就是那個房的歷史** | 沒帶 = 每個加入的房；`[]` = 空窗；**點名了不在的房 → 整個請求 `Forbidden`**；同一個房點兩次是一個房 |
| `limit` | **這一窗**最多幾條 | 沒帶 `wbf_recent_default_limit`（320）；上限 `wbf_recent_max_limit`（**預設改 500**，原 10000）。⚠️ **窗還有一個 bytes 的上限 `wbf_window_max_bytes`（8 MiB），先於它生效**（PR #53，§6.3） |
| `cg_seq` | client 已有的最新 `g_seq`，這窗不會回到它或比它舊 | 沒帶或 0 = 沒有快取 |
| `before` | 只要比這個舊的（上一窗最後一條的 `ls`） | 沒帶 = 從最新開始 |
| `batch` | 每個 Batch 幾條 | 預設 `wbf_recent_default_batch`（10）；上限 `wbf_recent_max_batch`（100），超過夾 |

server 在 `(cg_seq, before)` 之間從最新往舊，**只收這一窗**：收到 `wbf_window_max_bytes` 或 `limit` 條，**哪個先到停在哪**（bytes 先問，§6.3），然後每 `batch` 條送一個 `Batch`。
📎 點名一個房間是**最便宜**的情況：`collect_window` 對每個房開一條倒序串流、用 heap 合併，所以一個房是 k 路合併退化成單路掃描（跟 `/messages` 同一個迭代器）。
⚠️ **一窗結束講的是「本站這份副本沒有更舊的了」，不是「這個房間沒有更舊的了」** —— `Recent` 不會像 `/messages` 那樣去聯邦 backfill（[room-seq-and-recent.md](room-seq-and-recent.md) §2.1）。
下一窗由 client 帶 `before = 這窗最後一個 Batch 的 ls` 再叫一次 `Recent`；server 不記任何跨請求的狀態，`id` 由 client 決定要不要沿用。
兩窗之間那條連線是空的，`Ping` 或別的請求可以插進去 —— 這是拉式視窗換來的，也是 §4.3-5「一次一個 handler」不會餓死別人的原因。

### 6.2 `Event/Batch`

**`0x14 Event / 0x03 Batch`**，只有 server → client，是 `Recent` 的回應（`Recent` 不再用 `Ack` 回）。

| 欄位 | 內容 |
|---|---|
| header | `IS_RESPONSE = 1`；`id` 抄請求；`seq` 從 0 起嚴格 +1；沒有 `IS_LAST` |
| meta | `{ "tc": 320, "bc": 10, "fs": 20000, "ls": 19991, "r": 310, "more": true }` |
| data | `bc` 則事件，每則 **u32 大端長度 ＋ 事件 JSON bytes**，新到舊排（`fs ≥ ls`） |

| meta 欄 | 意思 |
|---|---|
| `tc` | total count：**這一窗**總共會送幾條（≤ `limit`）。同一窗每個 Batch 都一樣 |
| `bc` | batch count：這個 Batch 幾條 |
| `fs` | first g_seq：這批第一條（最新那條）的 `g_seq` |
| `ls` | last g_seq：這批最後一條（最舊那條）的 `g_seq`；最後一個 Batch 的 `ls` 就是下一窗的 `before` |
| `r` | remain：這批之後這一窗還剩幾條。**`r = 0` 就是這一窗結束** |
| `more` | **這一窗是被上限截斷的**（收滿 `limit` 條，或收滿 `wbf_window_max_bytes`），所以更舊的可能還有。`false` = 這窗是因為**沒有事件了**才停的（到了 `cg_seq` 或本站副本的最舊）。同一窗每個 Batch 都一樣。⚠️ client **沒看到這個欄位要當 `true`**（多問一次是一個來回，少問一次是漏事件）。📎 **`limit: 0` 是 `false`**：什麼都沒要，就沒有被截斷（PR #53 審查，rumia；原本回 `true`，會把 client 叫回來再要一次「什麼都不要」） |

不變量：每個 Batch `tc = 已送 + bc + r`；最後一個 `r = 0`；**空窗**（`tc = 0`）送一個 `bc = 0, r = 0, fs = ls = 0` 的 Batch。
事件 JSON 跟現在一樣（含 `room_id`，`unsigned` 帶 `org.wbftw.wbfuwunel.r_seq`／`g_seq`），見 room-seq-and-recent.md §2。
一個 Batch 的 data 另受 `wbf_data_max_bytes` 限：放不下第 n 條就提早結束這個 Batch（`bc < batch`），那條進下一個；**單一事件比 pack 還大**照現在跳過並 `debug_warn!`，
數的那趟也跳過它，`tc` 才對得上。

**為什麼長度前綴不用 JSON 陣列**：client 切事件只看四個 byte，不掃逗號、不先 parse 整段；收一個 Batch 寫一次 DB。
**為什麼不設 `WANT_ACK`**：Batch 是 server 產生的有序串流，順序由 WebSocket 保證，背壓由 §1 的佇列與 TCP 保證。
**為什麼沒有結束用的 pack**：`tc` 在第一個 Batch 就告訴 client 這窗有幾條，`r = 0` 是尾；再加一個型別是第二個講同一件事的信號。

### 6.3 先收齊一窗，再切 Batch

`g_seq` 是全站計數器，跨這個人沒加入的房間，`before − cg_seq` 是上界不是條數，準確的 `tc` 得把跨房合併＋可見性過濾走一遍。
📎 提案寫「兩趟：先數再送」；實作改成**一趟收進記憶體再切**：兩趟要保證「數的規則與送的規則是同一段程式」，多一個會漂移的接縫。收齊之後 `tc` 就是 `window.len()`，第二趟那些「數到的比 `tc` 少怎麼辦」的規則全部消失。

⚠️ **收進記憶體的那一窗有多大，原本只數則數**：一窗最多 500 則（§0-11），當時的理由是「幾百則事件在 RAM 裡是幾百 KB 到幾 MB，可忽略」。
那句話只對**一般大小**的事件成立 —— 每則只要求放得進一個 pack（`wbf_data_max_bytes`，2 MiB），所以理論上一窗是 500 × 2 MiB ≈ **1 GiB**；
而且原本的 `build_batches` 先把**所有** pack 造好再送，等於整窗在記憶體裡**兩份**。跟 PR #50 修掉的發送佇列是同一種病：界是用「幾個」寫的，沒有人把它乘出來。
✅ **PR #53（維護者 2026-09-14 指定「加上 filesize 限制，優先於 limit」）**：

- 一窗另有 **`wbf_window_max_bytes`（預設 8 MiB）**，數的是 data 裡的 bytes（每則 JSON ＋ 4 byte 長度前綴）。**先問 bytes、再問則數**；兩個上限共用一條規則 `core::wbf::events::WindowBudget`，`Event/Recent`、`Device/Fetch`、兩種 `Subscribe` 的補窗都用它。
- **窗的第一則一定收**，不管多大：拒收第一則的窗會對之後每一次請求回同一個空窗，client 永遠翻不過去。啟動檢查要求 `wbf_window_max_bytes ≥ wbf_data_max_bytes`（`check_wbf_window_max_bytes`），所以第一則也在界內，「一窗最多這麼多 bytes」就是真的。
- **一旦拒收，之後全拒**（`WindowBudget` 自己記著）：被拒的那則是下一窗的第一則，讓後面一則比較小的插隊，這窗就會有一個游標已經走過去的洞。
- 被 bytes 截斷的窗 `tc < limit` —— 那原本是「沒有更舊的了」的長相，所以 Batch 多了 **`more`**（§6.2、§6.4）。
- pack 改成**送一個造一個**：記憶體是「一窗 ＋ 一個 pack」，不再是兩份。
- 預設 8 MiB 的算法：一般事件幾 KB，500 則 × 4 KiB ≈ 2 MiB，**正常的窗碰不到它**（`the_default_window_budget_holds_a_default_window_of_ordinary_events` 釘著）；它只擋病態的窗。一窗 8 MiB 在發送佇列（16 MiB）裡放得下，送完一窗不必等 client 讀。

```
collect_window：堆合併 → ignored／visibility 過濾 → 一則自己就比 pack 大 → 跳過（debug_warn），不算進 tc
               → WindowBudget：先 bytes 後則數，放不下就停；停在 cg_seq 或沒有事件了 → more = false，停在上限 → more = true
build_batches：每 batch 則切一個 Batch；下一則放不進 wbf_data_max_bytes 也切；r = tc − 已送；空窗一個空 Batch；送一個造一個   ← 單元測試
```

**門檻**（§0-10）：client 等不到回應會斷線重連再問，server 又數一次，形成迴圈。所以「一窗 320 條的第一趟」必須遠低於 client 的等待時間；
驗收（§10）量一窗的 first byte，數字寫回這裡。這也是為什麼預設視窗是 320 而不是 10000：一萬條的一窗，first byte 就是數一萬條。

**實測（2026-09-08，e2e8 情境 4，Windows e2e build，同一台機器上的 PowerShell client；一個人 30 個房間各 10 則 ＋ state 事件）**：

| 窗 | first byte | `r = 0` | Batch 數 |
|---|---|---|---|
| 320／10，冷 | 25 ms | 691 ms | 32 |
| 320／10，暖 | 106 ms | 459 ms | 32 |
| 500／100，暖 | 112 ms | 457 ms | 5 |

first byte 是 30 房間的堆合併＋可見性過濾＋收齊 320 則的時間，離 client 的 10 秒預算兩個數量級；`r = 0` 的時間大半是 PowerShell 端逐包收與解析（32 包 vs 5 包差不多，
說明瓶頸不在 server 切片）。門檻定「冷 < 2 秒」，只擋回歸。

### 6.4 client 的水位（server 欄位語意決定的，寫在這裡）

- 一窗收完（`r = 0`）且 `more = false`：這窗已經回到 `cg_seq`，同步結束；把 `cg_seq` 存成**第一窗第一個 Batch 的 `fs`**（這輪最新的一條；比它新的下輪會來）。
- `more = true`（或沒有這個欄位）：可能還有更舊的，帶 `before = 最後的 ls` 再叫一窗；**水位不動**。剛好沒有更舊的會拿到一個空 Batch，一個來回的代價。
- 🚨 **不要再用 `tc < limit` 判斷同步結束**（PR #53 之前的規則）。窗被 bytes 截斷時 `tc < limit` 而且**還有更舊的**，照舊規則會把水位推過去、那段就再也補不回來。`more` 是唯一分得開這兩種情況的東西。
- 中途斷線或收到 `Error`：已收到的 Batch 有效；從最後一個 `ls` 續問。**不要**在中途推水位：回應是新到舊，第一批之後還有比舊 `cg_seq` 新的。
- 總數（一萬）是 client 自己累計的；server 每窗只知道自己的 `tc`。

### 6.5 HTTP

`Recent` 走 `POST /_wbf/v1/pack` → `Error(Unsupported)`，訊息「Event/Recent streams; use the WebSocket channel」。
e2e 腳本要看 `Recent` 的結果就開 WS 收 Batch（e2e9 現在用 HTTP 打 `Recent` 的地方要改）。

## 7. HTTP → WS 的模子：搬一個端點的步驟

> ⭐ **一般的 Matrix 端點不照這張表手搬了**：走橋（[wbf-api-bridge.md](wbf-api-bridge.md)），搬一個端點＝[bridge-specs/index.md](../bridge-specs/index.md) 加一列、`bridge.rs` 的表加一列、`KIND.md` 補範例、e2e 跟 HTTP 比一次；關卡（第 7 步）是 HTTP 那一道本身，不用另外寫。下面這張表留給**橋做不到的**：原生的 pack（串流、訂閱、推送、上傳）與改變連線身份的（`Login` 那一類）。
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
| 3 | §2.8 | sweeper 多掃「宣告在、進度不在、超過 TTL」的舊上傳 | 無。維護者已答（§0-13）：從未上線，這條是純防禦，做最小的那版、排最後 |

✅ **結果（PR #48，2026-09-13 合併）**：1、2 照做；📎 2 的實作跟這張表不同 —— 不是「本地夾 8 MiB」一條規則，而是 provider 回 `Option`（S3 給自己的 part 大小、至少 5 MiB；本地沒有意見、由上傳那側用 8 MiB），另加啟動檢查 `check_s3_part_size`。
🚫 3 **不做**：維護者 2026-09-13 確認這個 fork 從未上線，§0-13 那個「純防禦」要防的資料狀態不存在（review-followups §2.8）。

## 9. 版本與相容

`protocol` 號不動（舊 client 不送 `batch`，`Batch` 是它沒收過的 subtype；而且現在沒有舊 client 在跑）。**但下面三條是行為改變，CHANGELOG 要點名**：

| 改變 | 影響 |
|---|---|
| `Recent` 在 WS 上的回應由一個 `Ack` 變成 `Batch` 串流 | wbf client 要改收法。Matrix 端點不受影響 |
| `Recent` 走 HTTP 由「回一頁」變 `Error(Unsupported)` | 只影響測試腳本 |
| `Session/*` 走 HTTP 的錯碼由 `Conflict` 改 `Unsupported` | 錯碼改變；`Unsupported` 是新碼，語意「這個 kind 不走這個傳輸」 |

新錯碼兩個：`Unsupported`、`TooManyConnections`（wire-format §3.2 Error 那列補）。
新 config 五個：`wbf_ws_max_connections_per_device`（4）、`wbf_ws_send_queue_len`（32）、`wbf_recent_default_limit`（320）、`wbf_recent_default_batch`（10）、`wbf_recent_max_batch`（100）。
📎 之後加的：`wbf_ws_send_queue_bytes`（16 MiB，PR #50）；同一支把 `wbf_data_max_bytes` 從 16 MiB 降到 **2 MiB + 4096**、`media_chunk_size_max` 從 16 MiB 降到 **2 MiB**。
既有 config 改預設一個：`wbf_recent_max_limit` 10000 → **500**（§0-11）。
📎 PR #53：`wbf_window_max_bytes`（8 MiB，≥ `wbf_data_max_bytes`）；`Event/Batch` 與 `Device/Batch` 的 meta 多 `more`。⚠️ **這是 client 要跟的行為改變**：靠 `tc < limit` 判斷同步結束的 client，在窗被 bytes 截斷時會漏掉更舊的那段（§6.4）。

## 10. 驗收（e2e7 加情境 5、e2e9 改）

- **名額**：同一 device 開 4 條 WS 都活；第 5 條升級 429 ＋ `Error(TooManyConnections)`；關一條再開就成；另一個 device 不受影響；
  匿名開 5 條都活，第 5 條 `Login` 回 `TooManyConnections` 並被 Close 1008，其他四條的 session 正常。
- **名額回收**：4 條全關再開 4 條成功（RAII 有放）；一條在上傳中被 server 關機關掉，重啟後名額是 0（表在記憶體，重啟即清）。
- **視窗**：灌 1000 條 → `Recent(limit=320, batch=10)` 收到 32 個 Batch，`tc = 320`、`r` 遞減到 0、`fs`/`ls` 單調遞減、每則長度前綴對得上；
  帶 `before = 最後的 ls` 再叫兩窗各 320，第四窗 `tc = 40 < 320`；四窗事件不重複不漏、合起來剛好 1000；`batch=1000` 被夾成 100；`limit=10000` 被夾成 500；沒新事件 → 一個 `bc = 0, r = 0` 的 Batch。
- **first byte**（e2e8 情境 4）：一個人 30 個房間各 10 則，量一窗 320 從送出 `Recent` 到第一個 Batch、到 `r = 0` 的時間（冷／暖，另量 500/100），數字記進 §6.3。
  唯一的門檻是「冷的 first byte < 2 秒」，遠低於 client 的 10 秒重連預算（§0-10）。
- **背壓**：client 送 `Recent` 後停止讀 socket 5 秒，server 的 working set 不隨時間增長（有界佇列擋住了），恢復讀之後串流補完。
- **順序**：同一連線送 `Recent` 緊接 `Ping`，Pong 在這窗最後一個 Batch **之後**（一次一個 handler）；兩窗之間送 `Ping` 立刻有 Pong。
- **HTTP**：`Recent`、`Login` 打 `/pack` 都回 `Unsupported`。
- **關機**：一條連線在收 Batch 中途 `!admin server shutdown`，client 收到的最後一個 frame 是 Close 1001，程序乾淨退出。
- 第二部分照 review-followups §2.2／2.5／2.8 各自的驗收。

## 11. 落點

| 東西 | 檔 |
|---|---|
| 名額表、`ConnectionSlot`、`take_slot` | `src/service/connections.rs` |
| 發送 task、`Outgoing`、接收 loop 改入隊、名額拿放 | `src/api/client/wbf/ws.rs` |
| `PackContext`、`Reply`、`ReplySink`、准入表 `admission`、`Unsupported`／`TooManyConnections` | `src/api/client/wbf/mod.rs` |
| `Batch` 編碼（`build_batches`，純函數）、`collect_window`、`batch` 欄 | `src/api/client/wbf/recent.rs` |
| `admit` 閘門（`AdmitSession`、`admit_any`），`issue_session`／`refresh_session` 在寫 token 前問它 | `src/service/users/login.rs`；HTTP 呼叫點 `api/client/session/{mod,refresh}.rs` 傳 `admit_any` |
| kind 表、Error 列、§5 改成「已實作」、§6.1 補名額 | `docs/design/wbf-wire-format.md` |
| `Batch` 事件格式、水位規則 | `docs/design/room-seq-and-recent.md` §2 |
| 向量 `recent_batch`、`batch_first`、`batch_last`、`error_unsupported`、`error_too_many_connections` | `src/core/wbf/vectors.rs`、`docs/design/wbf-vectors.json` |
| 四個 config | `src/core/config/mod.rs`、`tuwunel-example.toml` |

## 12. 我不滿意或想再談的

- **`tc` 高估時的尾巴**：兩趟之間有事件變不可見時，最後一個 Batch `bc` 偏小、`r` 直接 0。一窗只有幾百條、兩趟間隔毫秒級，極少；接受。
- **一條連線一次一個 handler**：一窗期間那條連線不回 `Ping`；視窗 320 條是幾十毫秒的事，兩窗之間就空了，所以 idle timeout 不會被 Batch 串流誤殺。若之後有別的慢 handler
  （Seal 大檔）撞到 idle 的定義，再改成「兩個方向都沒動」。
- **名額表只在記憶體**：多節點部署各自算。這個 fork 單機，先不管。
- **`Reply` 在 HTTP 上只裝一個 pack** 是用型別擋 handler 的錯，不是功能。將來若真要 HTTP 回多個 pack（body 串接），改 `ReplySink::Http` 一處就好。
