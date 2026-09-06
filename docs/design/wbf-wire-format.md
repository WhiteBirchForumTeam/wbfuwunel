# wbf pack：通道與 HTTP 共用的二進位封包，以及收發的管線

> **狀態：草案，等維護者同意。** 定義 fork 自己的即時通道（WebSocket over TLS）上**每一個二進位訊框**，也是 HTTP 測試路徑的
> request body 與 response body —— **兩邊都是 pack，沒有 JSON 回應**。分塊上傳（[chunked-upload.md](chunked-upload.md)）與
> 流式訊息（[streaming-messages.md](streaming-messages.md)）都建立在它上面。
> 第三版依維護者 2026-09-03 指示：**pack 不管加密、只管格式與 CRC；拆包禁止 O(n) 掃描；加密好的 bytes 直接落在 data 段不再複製；
> 收發兩端都有同一套「封裝 → 送出 → 接收 → 拆包」的管線（池）；順序守則依 kind 分兩類。**
> 撰寫日期：2026-09-03。

## 1. 原則

1. **一個 pack 一件事**：一塊、一片、一個指令、一個回應。request 是 pack，response 也是 pack。
2. **一個欄位一個職權**：標頭給路由（明文、定長）；meta 給「要解讀這個 pack 的人」（變長 JSON）；data 是本體（bytes）。
3. **pack 不加密、不解密**。加密由發出端在封裝**之前**做、解密由接收端在拆包**之後**做；pack 只驗格式、算 CRC。
   `META_ENCRYPTED` 旗標只是告訴接收端「這段別當 JSON 直接讀」，pack 本身不動它。
4. **拆包是 O(1)**：所有欄位在固定偏移；變長段的長度寫在前面；不掃描、不找分隔符、不解析 JSON。
   唯一的線性成本是 CRC，用硬體指令。
5. **data 只存在一份**：封裝時 AEAD 直接寫進 pack 緩衝裡的 data 段；拆包回傳的是原緩衝上的切片，接收端在原地解密。
6. **沒有 base64**。

## 2. 版面

全部 big-endian。

```
offset  size  欄位          說明
0       1     version       0 = 未定義（拒收）；1 = v1
1       1     kind          基礎訊息類型（§3）
2       1     subtype       kind 之下的細分（§3）
3       1     flags         bit0 META_ENCRYPTED：meta 是密文，別當 JSON 讀
                            bit1 WANT_ACK：發送者要求對這個 pack 回 Ack
                            bit2 IS_RESPONSE：這是對 (id, seq) 那個請求的回應
                            bit3 IS_LAST：這個有序序列的最後一個（上傳的最後一塊、流的最後一片）
                            其餘保留，必須為 0
4       8     id            會話／物件識別（u64）：上傳的 upload id、流的 stream id；0 = 無
12      4     seq           序號（u32）；語意依 kind 的順序類別（§4）
16      4     meta_len      meta 的位元組數（可為 0）
20      m     meta          JSON bytes（明文或密文）
20+m    4     meta_crc      CRC-32C 算 offset 0 到 meta 結尾（標頭一起，標頭壞了也抓得到）
24+m    4     data_len      data 的位元組數（可為 0）
28+m    n     data          本體
28+m+n  4     data_crc      CRC-32C 只算 data
```

外框 32 bytes。兩個 CRC 分開：data 壞了但 meta 好，接收端知道是哪個 `(id, seq)` 的哪一塊壞，精準要重送。

- **CRC-32C（Castagnoli）**，不是 zlib 的 CRC-32：x86 的 SSE4.2 與 ARMv8 都有專用指令（`crc32c` crate 自動選），
  軟體 fallback 也有。zlib 那個沒有硬體指令。**這是效能上唯一該挑的點**：CRC 是 pack 處理裡唯一線性的成本。
- **拒收**：`version ≠ 1`、保留旗標非 0、長度與實際對不上、任一 CRC 不合 → 丟掉，回 `Control/Error`（§3）。
  同一連線連兩次壞就關連線，讓 client 重連。
- **上限**：`meta_len ≤ wbf_meta_max_bytes`（預設 64 KiB）、`data_len ≤ wbf_data_max_bytes`（預設 **16 MiB**，要放得下大塊，
  [chunked-upload.md](chunked-upload.md) §2.2）。
- 📎 **CRC 只抓傳輸損壞，不抓竄改**。竄改由 data 裡的 AEAD 標籤抓，那是收發兩端的事。TLS 之上再算 CRC 有一點重複，
  留著的理由是：解密前用一條硬體指令先篩掉壞塊，比解密失敗再猜便宜；而且 HTTP 路徑或未來別的傳輸不一定有 TLS 的完整性。

## 3. kind、subtype、meta

### 3.1 誰讀 meta

| kind | meta 給誰 | `META_ENCRYPTED` |
|---|---|---|
| `Control` | server | 0 |
| `Upload` | server（它要知道大小、位置） | 0 |
| `Download` | server | 0 |
| `Stream` | **對方 client**（server 只轉發） | 1 |
| 之後的 `Room`／`Query`（看房間、看第幾條訊息） | server | 0 |

規則一條：**server 要靠它動作的 meta 是明文；只是經過 server 的 meta 是密文。** `id` 與 `seq` 永遠在明文標頭，Ack 不讀 meta。

### 3.2 表

| kind | subtype | meta（JSON） | data |
|---|---|---|---|
| `0x01 Control` | `0x01 Hello` | `{ "protocol": 1, "client": "…", "features": [...] }` | 無 |
| | `0x02 Ack` | 各 kind 定的回應內容；`IS_RESPONSE = 1`，`id`、`seq` 抄請求 | 視 kind（`Download/Read` 的回應 data 是讀出的 bytes） |
| | `0x03 Error` | `{ "code": "…", "message": "…", "expected_seq"?: … }`；code：`UnsupportedVersion` `Corrupt` `UnknownKind` `TooLarge` `Unauthorized` `NotFound` `Conflict` `OutOfOrder` `Internal`；📄 §6.3 提案再加 `Forbidden`（憑證被拒）與 `RateLimited`（`retry_after_ms`） | 無 |
| | `0x04 Ping` / `0x05 Pong` | `{ "nonce": … }` | 無 |
| `0x02 Stream` | `Open` `Fragment` `Close` `Abandon` | [streaming-messages.md](streaming-messages.md) §4 | 密文本體 |
| `0x03 Upload` | `Create` `Chunk` `Status` `Seal` `Abort` | [chunked-upload.md](chunked-upload.md) §4 | 塊 bytes（`Chunk`） |
| `0x04 Download` | `Info` `Read` | [chunked-upload.md](chunked-upload.md) §5 | 回應的 data 是讀出的 bytes |
| `0x10 Session`（📄 提案，§6.3） | `0x01 Login` | Matrix `/login` 的請求體原樣：`{ "type": "m.login.password" \| "m.login.token", "identifier", "password" \| "token", "device_id"?, "initial_device_display_name"?, "refresh_token"?: bool }`；回應 `{ "user_id", "device_id", "access_token", "refresh_token"?, "expires_in_ms"? }` | 無 |
| | `0x02 Refresh` | `{ "refresh_token" }`；回應同 `Login` | 無 |
| | `0x03 Logout` | `{ "all"?: bool }`；回應 `{}`，之後這條連線回到未登入 | 無 |
| `0x14 Event` | `0x01 Recent` | `{ "limit": 10000, "cg_seq": <g_seq>?, "before": <g_seq>? }`；回應 `{ "returned": n, "latest_g_seq": <g_seq>, "complete": bool, "next": <g_seq> 或 null }` | 回應的 data 是事件的 JSON 陣列（含 `room_id`；每則 `unsigned` 帶 `org.wbftw.wbfuwunel.r_seq` 與 `…g_seq`），見 [room-seq-and-recent.md](room-seq-and-recent.md) §2 |
| `0x14 Event` | `0x02 Send` | `{ "room_id", "type", "txn_id", "attachments": [mxc…] }`；回應 `{ "event_id" }` | 事件 content 的 JSON（E2EE 就是 `m.room.encrypted` 的 content）。`attachments` 是 server 讀不到密文時唯一的引用來源，見 [media-attachments.md](media-attachments.md) |
| 其餘 | — | 拒收並回 `Error(UnknownKind)` | |

### 3.3 kind 的分配表（為之後把所有 HTTP 請求遷到 WS 預留）

維護者 2026-09-03 預告：**未來所有 Matrix client API 都會走這條通道**。kind 是 8 位（256 個）、subtype 8 位（每個 kind 256 個），
所以 kind 按 **API 領域**分、subtype 是領域內的操作；一個領域不超過 256 個操作，一個 kind 就夠。領域照 Matrix client-server 規格的章節切，
這樣遷移時一章對一個 kind，不用猜。先占號、不先定 subtype；**已分配的號不改**。

| kind | 領域 | 對應的 Matrix 章節 / 現在的 `src/api/client/` |
|---|---|---|
| `0x01` | Control | 通道自己的事 |
| `0x02` | Stream | 流式訊息（fork 自己的） |
| `0x03` | Upload | 分塊上傳（fork 自己的） |
| `0x04` | Download | 分塊下載（fork 自己的） |
| `0x05`–`0x0F` | 保留給 fork 自己的新功能 | |
| `0x10` | Session | login、logout、refresh、register（`session/`、`register/`）；📄 §6.3 提案先占 `0x01 Login`、`0x02 Refresh`、`0x03 Logout` |
| `0x11` | Account | account data、profile、3pid、password（`account/`、`account_data/`、`profile.rs`） |
| `0x12` | Sync | sync、filter（`sync/`、`filter.rs`） |
| `0x13` | Room | create、join、leave、invite、kick、ban、alias、directory、space（`room/`、`membership/`、`alias/`、`directory.rs`、`space.rs`） |
| `0x14` | Event | send、redact、state、context、relations、threads、messages（`send.rs`、`redact.rs`、`state.rs`、`context.rs`、`message.rs`、`relations.rs`、`threads.rs`）。已定：`0x01 Recent`、`0x02 Send` |
| `0x15` | Receipt | read marker、receipts、typing、presence（`read_marker/`、`typing.rs`、`presence.rs`） |
| `0x16` | Device | devices、to-device、dehydrated（`device/`、`to_device.rs`、`dehydrated_device.rs`） |
| `0x17` | Keys | E2EE keys、backup、cross-signing（`keys/`、`backup/`） |
| `0x18` | Push | pushers、push rules、notifications（`push/`） |
| `0x19` | Media | 舊的整檔上傳、縮圖、preview、config（`media.rs`）—— 與 `Upload`/`Download` 分開，這裡是相容路徑 |
| `0x1A` | Search | search、user directory（`search.rs`、`user_directory.rs`） |
| `0x1B` | Voip | TURN、rtc（`voip.rs`、`rtc.rs`） |
| `0x1C` | Misc | capabilities、versions、well-known、openid、thirdparty、report、tags（其餘） |
| `0x1D`–`0x1F` | 保留給 Matrix 尚未引進的章節 | |
| `0x20` | Admin | `!admin` 指令與 synapse admin API（`admin/`） |
| `0x21`–`0xEF` | 未分配 | |
| `0xF0`–`0xFF` | 實驗用，不保證穩定 | |

遷移時每個操作的 meta 就是它現在的 JSON body（ruma 的 request 型別直接 serde 成 meta），回應同理 —— 所以遷移是換外框，不是重寫語意。

## 4. 順序守則：依 kind 分兩類

WebSocket 本身保證到達順序，所以**順序錯一定是邏輯錯誤**，不是網路問題；發現就回 `Error(OutOfOrder)` 帶 `expected_seq`，
讓發送端從那裡重送，不要猜、不要自己重排。

| 類別 | 哪些 | `seq` 的意思 | 規則 |
|---|---|---|---|
| **有序（ordered）** | `Upload/Chunk`、`Stream/Fragment`、任何要 Ack 才能往下走的 | 同一個 `id` 之內嚴格遞增（+1） | 接收端記每個 `id` 的 `next_seq`；來的不是 `next_seq` → `Error(OutOfOrder)`，丟掉那一框，`next_seq` 不動。發送端可以連續送不等 Ack（滑動窗口），但送出的順序必須是遞增的 |
| **無序（unordered）** | `Download/Info`、`Download/Read`、`Upload/Status`、之後的看房間、看訊息 | 請求號，只用來把回應對回請求 | 可以亂序回；發送端用 `(kind, seq)` 對表。同一連線內 `seq` 由發送端自己保證不重複（單調遞增就好） |
| **事件驅動** | server 主動推的東西（別人的 `Stream/Fragment` 轉發、之後的通知） | 發送端的序號 | 接收端不守順序，收到就處理；掉了就掉了（§5 流式的送達語意） |

`Upload/Create`、`Seal`、`Abort`、`Stream/Open`、`Close` 這些**一次性指令**歸無序類（一個請求一個回應）。

## 5. 收發管線（兩端一樣）

每一端都有同一套四步，client 與 server 對稱；差別只在 handler。

```
封裝 encode   → 送出 send   →   接收 recv → 拆包 decode → 派發 dispatch
```

- **封裝**：`PackBuilder::new(kind, subtype, flags, id, seq)` → `.meta(bytes)` → `.data_slot(n)` 拿到 pack 緩衝裡 data 段的 `&mut [u8]`
  → 呼叫端把明文加密**直接寫進去**（AEAD 的 `encrypt_in_place`，或先 `copy` 明文再原地加密）→ `.finish()` 算兩個 CRC、回傳 `Bytes`。
  緩衝一次配好，大小 = 32 + m + n，之後不再搬。
- **送出**：一條連線一個發送佇列（`mpsc`），有序類的 pack 按 `seq` 入隊；佇列到 WebSocket sink 是單一 task，天然保序。
- **接收**：一條連線一個接收 task，讀到 binary message 就整包交給拆包，不在這裡做任何邏輯。
- **拆包**：`Pack::decode(&mut [u8]) -> Result<PackView<'_>>`：驗 version／flags／長度／兩個 CRC，回傳 `PackView { header, meta: &mut [u8], data: &mut [u8] }`
  —— **切片指回原緩衝**，沒有複製。接收端要解密就在 `data` 上原地解。
- **派發**：按 kind 查表（陣列索引，不是 match 字串）交給 handler；有序類先過 `next_seq` 檢查。handler 要送回應就走封裝那條。

**server 端**：一條 WebSocket 連線 = 一個接收 task ＋ 一個發送 task ＋ 一張 `id → 順序狀態` 表；handler 是 `handle_pack(user, PackView) -> Option<Pack>`，
HTTP 的 `POST /_wbf/v1/pack` 呼叫同一個 handler（一次一 pack，沒有連線狀態，有序類在 HTTP 上仍要 `seq` 正確 —— server 從 DB 讀 `next_seq`）。

**client 端**（給 SDK 之外的自寫部分）：同一個 `pack.rs`（它在 `core`，純函數、無 tokio 依賴，Android 與 Windows 的 Rust 核心直接編）。

**禁止**：在拆包路徑上 `for` 逐 byte 找東西、把 data `to_vec()`、為了讀一個欄位先解析整個 meta JSON。
meta 只在 handler 真的需要時才解析，而且 `Control/Ack` 這種熱路徑的 meta 是幾十 bytes。

## 6. 兩種送法

### 6.1 WebSocket（主要）

`GET /_wbf/v1/ws`（維護者 2026-09-03 定：自己的前綴，照 `/_名字/版本/功能` 的慣例；端點本身就跟上游切開了，不用借 `/_tuwunel/`），`Authorization: Bearer <access token>`，Upgrade，只接受 TLS。每個 binary message = 一個 pack。
一條連線同時跑很多 upload 與 stream，靠 `id` 分流；斷線後上傳進度在 DB（重連續傳）、流進入 abandoned 計時。
伺服器：axum `ws` feature，落點 `src/api/client/wbf/ws.rs`（B 支）。升級時驗 Bearer（錯了回 401，body 仍是 Error pack）；之後每個 binary message 一個 pack，依到達順序一個一個處理、回應依同一順序送回（client 可以連發不等 Ack）；字串 frame 回 `Error(Corrupt)`；到達的 pack 超過 `wbf_meta_max_bytes + wbf_data_max_bytes + 外框` 由 WebSocket 層拒收。連線**不保存任何上傳狀態**：`OutOfOrder` 一律由上傳服務判定，用的是 service 層**跨連線共用**的記憶體狀態（每個上傳一把鎖，DB 列為真相、txn 執行後才推進記憶體，見 [chunked-upload.md](chunked-upload.md) §3.1），不論塊從哪條連線或 HTTP 來；重連從 `Status` 接。（第一版曾放一張每連線的 `id → 下一塊` 表當捷徑，審查指出它在冪等重送後會倒退、在跨傳輸時會過期，先於 DB 否決合法的塊，整張拿掉。）沉默超過 `wbf_ws_idle_timeout`（預設 300 秒）server 關連線，上傳進度不受影響。「只接受 TLS」由前置代理保證，server 本身不檢查。

**連線背後的 session（2026-09-06，`wbf/auth-and-ws-lifetime`，[review-followups-2026-09-06.md](review-followups-2026-09-06.md) §2.3／§2.4）**：

- 升級時的驗證跟標準 client 路由一樣多：token 存在、沒到期、**帳號沒被鎖**（MSC3939，`M_USER_LOCKED` 401）。`POST /_wbf/v1/pack` 同一套。
- 升級時驗過的不算永久：server 記住 `Session { user, device, token }`，**每個 binary message 處理前重驗一次**（token 仍解到同一個 user 與 device、沒到期、沒被鎖），而且在**解碼之前**：已失效的 session 不能靠送壞封包讓連線活着（review，rumia）。
  登出、撤 device、到期、鎖帳號都在**下一個 pack** 生效：回 `Error(Unauthorized)`（帶那個 pack 的 `id`／`seq`），接著 server 送 Close `1008`（policy）關線。
  代價是每個 pack 多一次 token 點讀與一次鎖定讀，跟一個 HTTP 請求本來就付的一樣；🚫 不做「每 N 秒才驗」的快取，那是一份會過期的真相。
- 關機：server 進入 stopping 就對每條連線送 Close `1001`（going away；不用 IANA 的 `1012` service restart，.NET 的 `ClientWebSocket` 會把 1012 當協定錯誤斷線）並結束它的 task；`Services::stop` **等所有連線的 task 結束**才往下走
  （`Services.connections`），因為 handler 拿的 `State` 是 `Services` 的裸指標，升級後的 socket 活得比 request 久，不等就是 use-after-free。
  等最多 `JOIN_TIMEOUT`（15 秒）：連線的 loop 看到 stopping 就自己結束，會等到超時的只有卡在某個永不返回的呼叫裡的 task，那時 abort 它（drop future 連帶 drop 對 `Services` 的借用），不讓一條連線卡整個關機（review，rumia）。
  已經在關機的 server 拒絕新的升級（503，body 仍是 Error pack）。
- 之後的 `Login`／`Refresh`／`Logout` subtype（提案待寫）會建在同一個 `Session` 上：Login 就是換掉這條連線的 Session。

### 6.2 HTTP（選用，測試與腳本用）

`POST /_wbf/v1/pack`，`Content-Type: application/octet-stream`，**body 是一個 pack，回應 body 也是一個 pack**。
一個請求一個 pack；`id` 由 server 在 `Upload/Create` 的回應裡發，之後帶著它。它存在的理由是 curl 就能測；效能不是它的目標。
`Stream` kind 走 HTTP 沒意義（沒人連著收），回 `Error(Conflict)`。

### 6.3 📄 提案：`Login`／`Refresh`／`Logout` —— 在通道上取得與放掉 session

> 狀態：📄 提案，2026-09-07，等維護者同意。起因：維護者 2026-09-06 提出「登入應該有 WS 專用的 pack 格式；升級帶 Bearer 可以留著」。
> 建在 PR #28 的 `Session` 上（§6.1「連線背後的 session」）：Login 就是**換掉這條連線的 Session**，其餘機制（每個 message 重驗、關機 join）不變。

#### 6.3.1 為什麼要有

現在 client 要先走一次 HTTP `/login` 拿 token，才能開 WS。既然「未來所有 client API 都走這條通道」（§3.3），登入不該是唯一還得回 HTTP 的事。
帳密走 WS 明文送沒有比 `/login` 走 HTTPS 多一分風險：兩者都是 TLS 由前置代理保證（§6.1），信任假設一樣。

#### 6.3.2 三個 subtype（kind `0x10 Session`，§3.3 早就留給這一章；無序類，`WANT_ACK` 必帶）

不放進 `Control`：`Control` 是連線層的事（Hello、Ping、Ack、Error），登入是 Matrix 的一章，§3.3 的原則是一章一個 kind。
`Register` 也在這個 kind 下，號先不佔（§6.3.6）。

| subtype | meta（明文） | 成功的 Ack | 失敗 |
|---|---|---|---|
| `0x01 Login` | **Matrix `/login` 的請求體原樣**：`type`（`m.login.password` 或 `m.login.token`）、`identifier`、`password` 或 `token`、`device_id`?、`initial_device_display_name`?、`refresh_token`?: bool | `/login` 回應的欄位原樣：`user_id`、`device_id`、`access_token`、`refresh_token`?、`expires_in_ms`? | `Error(Forbidden)`（憑證錯、帳號停用；message 帶 Matrix errcode）、`Error(Unauthorized)`（帳號被鎖 `M_USER_LOCKED`）、`Error(RateLimited)` |
| `0x02 Refresh` | `{ "refresh_token" }` | 同 Login | `Error(Unauthorized)`（refresh token 不認、已用過、硬登出）、`Error(RateLimited)` |
| `0x03 Logout` | `{ "all"?: bool }`（`all` = 撤這個 user 的全部 device，對應 `/logout/all`） | `{}` | `Error(Unauthorized)`（這條連線本來就沒登入） |

**不自己發明欄位**：meta 直接餵給既有的 login／refresh／logout 邏輯，client 不用學第二套；server 端要做的是把 `login_route` 的本體（驗證 → 發 token → 建或更新 device）
抽到 `tuwunel_service`（例：`users::login::issue_session`），HTTP 路由與 WS handler 都叫它。🚫 不在 `api/client/wbf/` 裡複製一份登入流程。

**Login 的 `type` 只收 `m.login.password` 與 `m.login.token`**（後者是 SSO 流程最後一步的一次性 token）。SSO／OIDC 本身要瀏覽器重導，走 HTTP；
`m.login.application_service` 與 JWT 也留在 HTTP（appservice 不會用 WS 登入，JWT 之後有需要再加）。收到別的 `type` 回 `Error(Forbidden)`，message 說明。

#### 6.3.3 連線的狀態機

```
                Login ok                    Logout ok
 未登入 ────────────────▶ 已登入(Session) ──────────────▶ 未登入
   │  Hello/Ping/Login/Refresh 以外 → Error(Unauthorized)      │
   │  沉默或未登入超過 wbf_ws_unauthenticated_timeout → Close 1008
   ▼
 Login 失敗 → Error(Forbidden)，連線留著（受 §6.3.4 限速）
```

- **允許不帶 Bearer 升級**（這是提案裡唯一改升級行為的地方）：Session 是「未登入」，只接受 `Hello`、`Ping`、`Login`、`Refresh`；其他 kind 回 `Error(Unauthorized)`。
  帶 Bearer 升級照 §6.1 不變，直接是已登入。兩條路都在，client 自己選。
- **未登入的連線用短得多的 idle 上限**：新 config `wbf_ws_unauthenticated_timeout`（預設 10 秒），跟已登入的 `wbf_ws_idle_timeout`（300 秒）分開。
  否則任何人不登入就能佔一條連線 5 分鐘。超時送 Close 1008。
- **Login 成功後這條連線的 Session 換成新的**，之後每個 message 的重驗用新 token。同一條連線再送 Login：允許，換成另一個 session（舊 token 不撤，
  那是 Logout 的事）—— 這對「切帳號」有用，而且不允許的話 client 只是多開一條連線，防不到什麼。
- **Refresh 成功後 Session 的 token 換新**，user／device 不變；舊 access token 照 Matrix 語意失效。
- **Logout 成功後連線回到未登入**，不關線：client 可以立刻 Login 別的帳號。從這一刻起未登入超時開始算。`all: true` 撤全部 device，
  同一個 user 的**其他** WS 連線在下一個 message 的重驗就被關（§6.1 的機制，不用另外通知）。
- **重驗照舊每個 message 一次**；未登入狀態沒有 token，跳過重驗（沒東西可驗），只做 kind 白名單。

#### 6.3.4 限速（這條是必做，不是加分）

HTTP `/login` 現在**沒有**限速（只有 OIDC 端點有 `oidc_rc_per_second`／`oidc_rc_burst_count`）。開了 WS Login 等於多一個入口，而 WS 的每個 message 比一個 HTTP 請求便宜，
不限速就是給暴力破解開快車道。提案：

- 新 config `login_rc_per_second`／`login_rc_burst_count`，**同一個 token bucket 同時管 HTTP `/login`、`/refresh` 與 WS `Login`／`Refresh`**，key 是 client IP
  （WS 在升級時取 `ClientIp`，存進連線狀態）。形狀抄 OIDC 那組（既有實作可以直接用）。
- 預設值**由維護者定**。OIDC 那組預設 0（關），理由是「保留開放存取」；登入不同，我建議預設開：`login_rc_per_second = 1`、`login_rc_burst_count = 10`
  ——一個人手打密碼打不到這個速度，NAT 後面十個人同時登入也剛好夠。低了會傷到真人、高了等於沒有，這個數字該你拍板。
- 超過回 `Error(RateLimited)`，meta 多 `retry_after_ms`；HTTP 側回 429 `M_LIMIT_EXCEEDED`（Matrix 既有）。
- 🚫 不做「連錯 N 次鎖帳號」：那是讓攻擊者能鎖住別人帳號的 DoS 入口。

#### 6.3.5 `Hello`

`Hello.features` 多 `"login"`。client 據此決定要不帶 Bearer 升級再 Login，還是走舊路。

#### 6.3.6 不做的

- 不做 `Register`：註冊有 UIAA 互動流程、registration token、驗證信，那是一整章（Matrix spec §「Account registration」），等 §3.3 的 kind 分配到它再說。
- 不做 SSO／OIDC over WS：需要瀏覽器。
- 不做「登入後 server 主動推 sync」：那是另一個提案（串流訊息、Event 領域），跟登入無關。

#### 6.3.7 驗收（e2e7 新情境）

- 不帶 Bearer 升級 → `Hello` 可、`Ping` 可、`Upload/Create` 回 `Error(Unauthorized)`；沉默 10 秒 → Close 1008。
- `Login`（密碼）→ Ack 帶 token；接著 `Upload/Create` 可；同一 token 打 HTTP `/_wbf/v1/pack` 也可（同一個 session）。
- 錯密碼 → `Error(Forbidden)` 且連線仍開；連錯到超過 burst → `Error(RateLimited)` 帶 `retry_after_ms`；同一 IP 打 HTTP `/login` 也 429。
- `Refresh` → 新 access token，舊的打 HTTP 401；連線繼續可用。
- `Logout` → Ack；接著 `Upload/Create` 回 `Error(Unauthorized)`；再 `Login` 別的帳號成功。`Logout { all: true }` → 同 user 另一條已登入的 WS 在下一個 Ping 收到 `Error(Unauthorized)` ＋ Close 1008。
- 鎖帳號後 `Login` → `Error(Unauthorized)`（`M_USER_LOCKED`）。
- 回歸：e2e7 情境 1～3、e2e6。

#### 6.3.8 落點

| 什麼 | 哪裡 |
|---|---|
| 登入本體抽成 service 函式 | `src/service/users/login.rs`（新），`api/client/session/mod.rs` 的 `login_route` 改成呼叫它 |
| 三個 subtype 的 handler、未登入 Session、白名單 | `src/api/client/wbf/{mod,ws,login}.rs` |
| 限速 | `src/core/config/mod.rs`（`login_rc_*`）、既有 OIDC token bucket 抽成共用 |
| 未登入超時 | `src/core/config/mod.rs`（`wbf_ws_unauthenticated_timeout`）、`ws.rs` |
| 向量 | `wbf-vectors.json` 加 Login／Refresh／Logout 各一個 pack |

#### 6.3.9 待維護者定

1. 限速預設值（§6.3.4 我建議 1／10）。
2. Logout 後是「回到未登入」還是「直接關線」（§6.3.3 我選前者）。
3. 同一條連線重複 Login 是否允許（我選允許）。

## 7. 程式：一個型別，四個函式，先寫測試

落點 `src/core/wbf/pack.rs`：

```rust
pub struct PackHeader { version, kind, subtype, flags, id: u64, seq: u32 }
pub struct PackView<'a> { pub header: PackHeader, pub meta: &'a mut [u8], pub data: &'a mut [u8] }   // 零複製
pub struct PackBuilder { … }   // new → meta → data_slot → finish

impl PackBuilder {
    pub fn new(kind: Kind, subtype: u8, flags: Flags, id: u64, seq: u32) -> Self;
    pub fn meta(self, meta: &[u8]) -> Self;            // 明文 JSON 或已加密的 bytes，builder 不分
    pub fn data_slot(&mut self, len: usize) -> &mut [u8]; // 直接寫進去；AEAD encrypt_in_place 的目標
    pub fn finish(self) -> Bytes;                       // 算兩個 CRC-32C
}
pub fn decode(bytes: &mut [u8]) -> Result<PackView<'_>, PackError>;   // O(1) 偏移 ＋ 兩次硬體 CRC
```

單元測試：encode→decode 來回、meta 壞 CRC 被拒且說是 meta、data 壞 CRC 被拒且說是 data、截斷被拒、保留旗標非 0 被拒、
version 0 被拒、空 meta 與空 data 合法、`data_slot` 寫入後 `finish` 的 CRC 正確、decode 回傳的切片就是原緩衝（指標相等）。
再加一個 benchmark：64 KiB 與 4 MiB 的 pack，封裝與拆包各花多久 —— 目標是**只剩 CRC 的時間**。

## 8. 版本演進

- meta 的 JSON 加欄位：舊端忽略不認得的 key → **不升版本**。
- 改標頭版面、改 CRC 範圍、改 flags 語意 → `version` 升 2，server 兩版並收一段時間。
- kind／subtype 只增不改號。

## 9. 我不滿意或想再談的

1. **TLS 之上的 CRC** 有一點重複（§2 已說留著的理由）。如果之後量出來 CRC 是可見成本，可以加一個連線層的 `Hello` 協商「這條連線不算 data_crc」。現在先留。
2. **`seq` 32 位**：一個上傳最多 4G 塊，64 KiB 的塊就是 256 TiB，夠；流的片數也夠。如果哪天不夠，那是升 version 的事。
3. **HTTP 上的有序類**：每個請求都要從 DB 讀 `next_seq`，比 WebSocket 慢一截。既然 HTTP 只是測試路徑，接受。
