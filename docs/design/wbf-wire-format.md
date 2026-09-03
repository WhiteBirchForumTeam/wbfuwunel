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
| | `0x03 Error` | `{ "code": "…", "message": "…", "expected_seq"?: … }`；code：`UnsupportedVersion` `Corrupt` `UnknownKind` `TooLarge` `Unauthorized` `NotFound` `Conflict` `OutOfOrder` `Internal` | 無 |
| | `0x04 Ping` / `0x05 Pong` | `{ "nonce": … }` | 無 |
| `0x02 Stream` | `Open` `Fragment` `Close` `Abandon` | [streaming-messages.md](streaming-messages.md) §4 | 密文本體 |
| `0x03 Upload` | `Create` `Chunk` `Status` `Seal` `Abort` | [chunked-upload.md](chunked-upload.md) §4 | 塊 bytes（`Chunk`） |
| `0x04 Download` | `Info` `Read` | [chunked-upload.md](chunked-upload.md) §5 | 回應的 data 是讀出的 bytes |
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
| `0x10` | Session | login、logout、refresh、register（`session/`、`register/`） |
| `0x11` | Account | account data、profile、3pid、password（`account/`、`account_data/`、`profile.rs`） |
| `0x12` | Sync | sync、filter（`sync/`、`filter.rs`） |
| `0x13` | Room | create、join、leave、invite、kick、ban、alias、directory、space（`room/`、`membership/`、`alias/`、`directory.rs`、`space.rs`） |
| `0x14` | Event | send、redact、state、context、relations、threads、messages（`send.rs`、`redact.rs`、`state.rs`、`context.rs`、`message.rs`、`relations.rs`、`threads.rs`） |
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
伺服器：axum `ws` feature（目前**沒開**），落點 `src/api/client/wbf/ws.rs`。

### 6.2 HTTP（選用，測試與腳本用）

`POST /_wbf/v1/pack`，`Content-Type: application/octet-stream`，**body 是一個 pack，回應 body 也是一個 pack**。
一個請求一個 pack；`id` 由 server 在 `Upload/Create` 的回應裡發，之後帶著它。它存在的理由是 curl 就能測；效能不是它的目標。
`Stream` kind 走 HTTP 沒意義（沒人連著收），回 `Error(Conflict)`。

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
