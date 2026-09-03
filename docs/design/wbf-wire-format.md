# wbf 封包（pack）格式：通道與 HTTP 共用的二進位外框

> **狀態：草案，等維護者同意。** 這份文件定義 fork 自己的即時通道（WebSocket over TLS）上**每一個二進位訊框**，
> 也是 HTTP 測試路徑的 body 格式 —— 同一個 pack，兩種送法。分塊上傳（[chunked-upload.md](chunked-upload.md)）與
> 流式訊息（[streaming-messages.md](streaming-messages.md)）都建立在它上面，差別只在 kind 與 meta 的內容。
> 第二版依維護者 2026-09-03 指示：**以 WebSocket 為主，HTTP 只是選用的測試路徑，body 就是 pack 本身；
> pack 分固定標頭、變長 meta、變長 data 三段，meta 與 data 各自有 CRC；不做 JSON 加 base64。**
> 撰寫日期：2026-09-03。

## 1. 原則

- **一個 pack 一件事**：一塊、一片、一個指令、一個回應。
- **一個欄位一個職權**：標頭給 server 路由用（明文、定長）；meta 給「要解讀這個 pack 的人」（變長 JSON）；data 是本體（bytes）。
- **server 只看標頭**。meta 給誰看由 kind 決定（§3.1）；data 永遠不看。
- **沒有 base64**。二進位就是二進位，HTTP 也一樣。
- 外框開銷 32 bytes（含兩個 CRC），跟一個 UDP/IP 標頭（28 bytes）同量級。

## 2. 版面

全部 big-endian。

```
offset  size  欄位          說明
0       1     version       0 = 未定義（拒收）；1 = v1
1       1     kind          基礎訊息類型（§3）
2       1     subtype       kind 之下的細分（§3）
3       1     flags         bit0 META_ENCRYPTED：meta 是密文，server 不解讀
                            bit1 WANT_ACK：發送者要求對這個 pack 回 Ack
                            其餘保留，必須為 0
4       8     id            會話／物件識別（u64）：上傳的 upload id、流的 stream id、指令回應的對應 id；0 = 無
12      4     seq           序號（u32）：塊的 index、片的序號、指令的請求號；Ack 用 (id, seq) 對回去
16      4     meta_len      meta 的位元組數（可為 0）
20      m     meta          JSON（明文或密文，看 flags）
20+m    4     meta_crc      CRC-32（IEEE，同 zlib）算 offset 0 到 meta 結尾
24+m    4     data_len      data 的位元組數（可為 0）
28+m    n     data          本體 bytes
28+m+n  4     data_crc      CRC-32 只算 data 那一段
```

- **兩個 CRC 分開**：meta 壞了整個 pack 丟；data 壞了但 meta 好，接收端知道是哪個 id/seq 的哪一塊壞了，可以精準要重送。
- **拒收規則**：`version ≠ 1`、`flags` 保留位非 0、長度與實際對不上、任一 CRC 不合 → 丟掉，回 `Control/Error`（§3.2）。
  同一連線連兩次壞就關連線，讓 client 重連。
- **上限**：`meta_len ≤ wbf_meta_max_bytes`（預設 64 KiB）、`data_len ≤ wbf_data_max_bytes`（預設 1 MiB）。
- 📎 **CRC 只抓傳輸損壞，不抓竄改。** 竄改由 data 裡的 AEAD 標籤抓（client 端解密時驗）；server 看不到明文，也不該負責這件事。
  所以**不用** SHA 一類的密碼雜湊。

## 3. kind、subtype、meta

### 3.1 誰讀 meta

| kind | meta 給誰 | `META_ENCRYPTED` |
|---|---|---|
| `Control` | server | 0 |
| `Upload` | server（它要知道大小、位置） | 0 |
| `Download` | server | 0 |
| `Stream` | **對方 client**（server 只轉發） | 1 |

規則就一條：**server 要靠它動作的 meta 是明文；只是經過 server 的 meta 是密文。** 把 `Stream` 的 meta 加密是因為它裡面的東西
（本文、序號以外的語意）不關 server 的事；`Upload` 的 meta 不能加密，因為 `total_len`、`chunk_size` 不給 server 就沒人切得了。
`id` 與 `seq` 永遠在明文標頭，所以 Ack 不需要讀 meta。

### 3.2 表

| kind | subtype | 方向 | meta（JSON） | data |
|---|---|---|---|---|
| `0x01 Control` | `0x01 Hello` | 雙向，連上第一框 | `{ "protocol": 1, "client": "…", "features": ["stream","upload"] }` | 無 |
| | `0x02 Ack` | 回應 | `{ "ok": true }` 或帶回應內容（各 kind 定） | 視 kind |
| | `0x03 Error` | 回應 | `{ "code": "…", "message": "…" }`；code：`UnsupportedVersion` `Corrupt` `UnknownKind` `TooLarge` `Unauthorized` `NotFound` `Conflict` `Internal` | 無 |
| | `0x04 Ping` / `0x05 Pong` | 雙向 | `{ "nonce": … }` | 無 |
| `0x02 Stream` | `0x01 Open` `0x02 Fragment` `0x03 Close` `0x04 Abandon` | 見 [streaming-messages.md](streaming-messages.md) §4 | 密文 | 密文本體（Fragment） |
| `0x03 Upload` | `0x01 Create` `0x02 Chunk` `0x03 Status` `0x04 Seal` `0x05 Abort` | 見 [chunked-upload.md](chunked-upload.md) §4 | 明文 | 塊 bytes（Chunk） |
| `0x04 Download` | `0x01 Info` `0x02 Read` | 見 [chunked-upload.md](chunked-upload.md) §5 | 明文 | 回應的 data 是讀出的 bytes |
| `0x05`–`0xFF` | — | — | 拒收並回 `Error(UnknownKind)` | |

回應（`Ack` / `Error`）的 `id`、`seq` **抄請求的**，發送者據此對回。新增 kind 就加一列；舊 client 遇到不認得的 kind 只丟那一框。

## 4. 兩種送法

### 4.1 WebSocket（主要）

- `GET /_wbf/v1/ws`，`Authorization: Bearer <access token>`，Upgrade。只接受 TLS。
- 每個 WebSocket binary message = 一個 pack。一條連線同時跑很多 upload 與 stream，靠 `id` 分流。
- 斷線：上傳進度在 DB（重連續傳），流進入 abandoned 計時。
- 伺服器：axum 的 `ws` feature（目前 **沒開**，router 裡沒有 WebSocket），落點 `src/api/client/wbf/ws.rs`。

### 4.2 HTTP（選用，測試與腳本用）

- `POST /_wbf/v1/pack`，`Content-Type: application/octet-stream`，**body 就是一個 pack**，回應 body 也是一個 pack（`Ack` 或 `Error`，
  `Download/Read` 的回應 data 是讀出的 bytes）。一個請求一個 pack，沒有連線狀態；`id` 由 server 在 `Upload/Create` 的回應裡發，
  之後的請求帶著它，server 從 DB 認得。
- 它存在的理由是 curl 就能測、腳本好寫；**效能不是它的目標**，所以不會為它另做 JSON 端點。
- `Stream` kind 走 HTTP 沒意義（沒人連著收），server 回 `Error(Conflict)`。

## 5. 程式：一個型別，三個函式

落點 `src/core/wbf/pack.rs`（core，因為 api 與未來的測試工具都要用；client 端的 Rust 也能直接拿去）：

```rust
pub struct Pack {
    pub version: u8, pub kind: Kind, pub subtype: u8, pub flags: Flags,
    pub id: u64, pub seq: u32,
    pub meta: Vec<u8>,   // JSON bytes，明文或密文
    pub data: Vec<u8>,
}

impl Pack {
    pub fn encode(&self) -> Vec<u8>;                                  // 算兩個 CRC、組 bytes
    pub fn decode(bytes: &[u8]) -> Result<Pack, PackError>;           // 驗版本、旗標、長度、CRC；錯了說是哪一段
    pub fn with_json_meta(kind, subtype, id, seq, meta: &serde_json::Value, data: Vec<u8>) -> Pack;
    pub fn meta_json(&self) -> Result<serde_json::Value, PackError>;  // 只在 META_ENCRYPTED = 0 時有意義
}
```

單元測試：encode→decode 來回、meta 壞 CRC 被拒且說是 meta、data 壞 CRC 被拒且說是 data、截斷被拒、保留旗標非 0 被拒、
version 0 被拒、空 meta 與空 data 合法。這些是純函數，先寫測試再寫實作。

## 6. 版本演進

- meta 的 JSON 加欄位：舊端忽略不認得的 key → **不升版本**。
- 改標頭版面、改 CRC 範圍、改 flags 語意 → `version` 升 2，server 兩版並收一段時間。
- kind／subtype 只增不改號。
