# wbf 二進位封包格式（WebSocket 通道共用）

> **狀態：草案，等維護者同意。** 這份文件定義 fork 自己的即時通道（WebSocket over TLS）上**每一個二進位訊框**的外框。
> 分塊上傳（[chunked-upload.md](chunked-upload.md)）與流式訊息（[streaming-messages.md](streaming-messages.md)）
> 都走這個外框，差別只在訊息型別與 payload。外框刻意留了版本與型別位，之後改變不用換通道。
> 撰寫日期：2026-09-03。

## 1. 為什麼要自己的外框

WebSocket 已經給了訊息邊界與 TLS，所以外框**不做**分段、不做加密、不做重組。它只回答四件事：
這是哪一版、這是什麼、多長、有沒有在路上壞掉。四件事加起來 12 bytes，跟一個 UDP 標頭差不多，
比每則訊息一個 HTTP 請求便宜得多。

## 2. 版面

全部 big-endian。

```
offset  size  欄位
0       1     version     0 = 未定義（拒收）；1 = v1
1       1     kind        基礎訊息類型（§3）
2       2     subtype     kind 之下的細分／旗標（§3；沒有就 0）
4       4     length      payload 的位元組數（u32）
8       len   payload     內容；是不是密文由 kind 決定，外框不管
8+len   4     crc32       從 offset 0 到 payload 結尾的 CRC-32（IEEE 802.3，跟 zlib 一樣）
```

- **收到 version 不是 1** → 丟掉並回 `Error(UnsupportedVersion)`（§3），不猜。
- **length 與訊框實際長度對不上、或 crc 不合** → 丟掉並回 `Error(Corrupt)`。連兩次壞就關連線，讓 client 重連。
- **上限**：`length` 不得超過 `wbf_frame_max_bytes`（config，預設 1 MiB）。超過的訊框整個丟。
  分塊上傳的塊本來就 ≤ 64 KiB 這個量級（[chunked-upload.md](chunked-upload.md) §2.2），流式訊息更小。
- CRC 用 32 位元，不用 16：多 2 bytes，誤判率從 1/65536 到 1/4G。

📎 **CRC 只抓傳輸損壞，不抓竄改。** 竄改由 payload 裡的 AEAD 標籤抓（client 端解密時驗），
server 本來就看不到明文，也不該負責這件事。所以這裡**不用** SHA 一類的密碼雜湊。

## 3. kind 與 subtype

| kind | 名字 | subtype | payload |
|---|---|---|---|
| `0x00` | 保留 | — | 拒收 |
| `0x01` | `Control` | `0x0001 Hello`、`0x0002 Ack`、`0x0003 Error`、`0x0004 Ping`、`0x0005 Pong` | Cbor（§4） |
| `0x02` | `Stream` | `0x0001 Open`、`0x0002 Fragment`、`0x0003 Close`、`0x0004 Abandon` | Cbor 標頭 ＋ 密文（[streaming-messages.md](streaming-messages.md) §4） |
| `0x03` | `Upload` | `0x0001 Chunk`、`0x0002 Status`、`0x0003 Seal` | Cbor 標頭 ＋ 塊 bytes（[chunked-upload.md](chunked-upload.md) §6） |
| `0x04`–`0xFF` | 未定 | — | 拒收並回 `Error(UnknownKind)` |

新增 kind 就在這張表加一列；舊 client 收到不認得的 kind 只會丟掉那一框，不會斷線。

## 4. Control 的 payload（Cbor）

```text
Hello   { protocol: 1, client: "…", features: ["stream", "upload"] }   // 連上後第一框，雙向
Ack     { kind, subtype, id, seq }        // 對某一框的確認；id/seq 是那一框自己的識別
Error   { code, message, id?, seq? }      // code：UnsupportedVersion | Corrupt | UnknownKind | TooLarge | Unauthorized | NotFound | Conflict | Internal
Ping/Pong { nonce }
```

**Ack 是選用的**：每種 kind 自己決定要不要等 Ack（流式訊息的規則在它的文件 §5）。外框不強制。

## 5. 通道本身

- 端點：`GET /_wbf/ws/v1`，帶 access token（`Authorization: Bearer`，跟其他端點一樣），Upgrade 成 WebSocket；
  只接受 TLS（反向代理或自己的 TLS 都行，跟現有部署一致）。
- 一個連線可以同時跑很多個 stream 與 upload，靠 payload 裡的 `id` 分流。
- 連線斷了：server 端的 stream 進入 abandoned 計時，upload 的進度在 DB 裡（[chunked-upload.md](chunked-upload.md) §3），重連續傳。
- 伺服器實作：axum 的 `ws` feature（目前 `Cargo.toml` **沒開**，router 裡沒有任何 WebSocket），
  落點 `src/api/client/wbf/ws.rs`，路由註冊在 `src/api/router.rs`。

## 6. 版本演進的規則

- payload 裡加欄位：Cbor 可以加，舊端忽略不認得的 key → **不用**升版本。
- 改欄位語意、改外框版面 → `version` 升到 2，server 兩版並收一段時間。
- kind／subtype 只增不改號。
