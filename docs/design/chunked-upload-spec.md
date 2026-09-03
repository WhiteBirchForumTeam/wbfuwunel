# 分塊上傳／下載 規格書（給 client 開發者）

> 這是**線上規格**：byte 怎麼排、每個訊息帶什麼、server 會怎麼回。設計理由與取捨在
> [chunked-upload.md](chunked-upload.md)，pack 通用格式在 [wbf-wire-format.md](wbf-wire-format.md)。
> 兩份有出入時，以本文為準；本文與 server 程式有出入時，那是 bug，請開 issue。
>
> 狀態：2026-09-03，對應 `media/chunked-upload-a` 分支（PR #16）。

## 0. 一句話

client 把檔案切成**明文固定大小**的塊，每塊自己加密，一塊一個 pack 送上來；server 只記「收到多少、放在哪」，
不看內容、不判斷密文長度；下載時把每一塊**照原樣**交回去，client 自己解。檔名、MIME、金鑰等描述**加密後**放在
`Create` 的 data 段，server 原樣存、原樣還。

## 1. 傳輸

| | WebSocket（主要） | HTTP（測試與腳本） |
|---|---|---|
| 端點 | `GET /_wbf/v1/ws`，Upgrade | `POST /_wbf/v1/pack` |
| 認證 | `Authorization: Bearer <access_token>` 在升級請求上；錯了回 401，body 是 Error pack，不升級 | 同 |
| request | 一個 binary message = **一個 pack** | body = **一個 pack**，`Content-Type: application/octet-stream` |
| response | 一個 binary message = 一個 pack，**依請求到達的順序**送回 | body = 一個 pack；HTTP 一律 200（沒 token 是 401，body 仍是 pack） |
| 連發 | 可以：送幾個都行、不等回應，回應依序回來（同一個 `id` 的 `Chunk` 自己要送對順序） | 一請求一包 |
| 限制 | 字串 frame 回 `Error(Corrupt)`；單個 message 超過 `wbf_meta_max_bytes + wbf_data_max_bytes + 外框`（預設約 16.06 MiB）連線被關 | 同樣的 pack 上限 |

沒有 JSON 外框、沒有 base64、沒有 multipart。多個上傳可以在同一條連線交錯，靠 pack 標頭的 `id` 分流。
同一個上傳可以一半走 HTTP、一半走 WebSocket：進度在 server 的 DB，不在連線上。

連上 WebSocket 後建議先送一個 `Hello`（kind `0x01` Control、subtype `0x01`，meta JSON `{ "protocol": 1, "client": "…", "features": [] }`），
server 回 Ack meta `{ "protocol": 1, "server": "<server name>", "features": ["upload", "download"], "chunk_size_default": 65536, "chunk_size_large": 1048576, "data_max_bytes": 16781312 }`。
`Ping`（subtype `0x04`，meta 任意）回 `Pong`（subtype `0x05`）把 meta 原樣還回。

## 2. pack

所有整數 **big-endian**。固定外框 32 byte，加上 meta 與 data。

| 偏移 | 大小 | 欄位 | 值 |
|---|---|---|---|
| 0 | 1 | `version` | `0x01` |
| 1 | 1 | `kind` | `0x01` Control、`0x03` Upload、`0x04` Download |
| 2 | 1 | `subtype` | 見 §3、§4 |
| 3 | 1 | `flags` | bit0 `META_ENCRYPTED`(0x01)、bit1 `WANT_ACK`(0x02)、bit2 `IS_RESPONSE`(0x04)、bit3 `IS_LAST`(0x08)；其餘必須 0 |
| 4 | 8 | `id` | 上傳 id；`Create` 與所有 Download 請求為 0 |
| 12 | 4 | `seq` | `Chunk`：**塊索引（0 起）**；其他請求：請求號（client 自訂，回應抄回） |
| 16 | 4 | `meta_len` | 可 0 |
| 20 | n | `meta` | 見各訊息 |
| 20+n | 4 | `meta_crc` | CRC-32C（Castagnoli）蓋 **偏移 0 到 meta 結尾** |
| 24+n | 4 | `data_len` | 可 0 |
| 28+n | m | `data` | 見各訊息 |
| 28+n+m | 4 | `data_crc` | CRC-32C 只蓋 data（空 data 時為 `00 00 00 00`） |

上限（server 預設，config 可調）：`meta_len ≤ 65536`，`data_len ≤ 16 MiB + 4096`。超過整個 pack 拒收（`TooLarge`）。

回應 pack：`kind = 0x01`，`subtype = 0x02 Ack` 或 `0x03 Error`，`flags` 帶 `IS_RESPONSE`，`id` 與 `seq` **抄請求的**。
Ack 的 meta 是 JSON（各訊息定義）；Error 的 meta 是 JSON `{ "code": "...", "message": "...", ...其他欄位 }`。

## 3. 上傳（kind `0x03`）

### 3.1 `Create`（subtype `0x01`，id 0）

**meta = `EncryptedFileInfo`，固定 16 byte 二進位，不是 JSON：**

| 偏移 | 大小 | 欄位 | 意義 |
|---|---|---|---|
| 0 | 8 | `file_size` | **明文**總長，byte |
| 8 | 4 | `chunk_size` | **明文**塊大小，byte；`0` = 用 server 預設（64 KiB） |
| 12 | 4 | `chunk_count` | 總塊數，**必須等於 `ceil(file_size / chunk_size)`** |

**串流模式**（邊生成邊傳、大小未知）：`file_size = 0` **且** `chunk_count = 0`。之後沒有宣告的結尾，**最後一塊帶 `IS_LAST` 才結束**；塊數上限仍由單檔上限推出。只有一個是 0 → `Conflict`。

**data = client 加密後的檔案描述**（可空）。server 不讀、不解、不驗，原樣存，`Info` 時原樣回。上限 64 KiB。

server 檢查（任一不過就 `Error`，一個 byte 都還沒收）：

| 條件 | code |
|---|---|
| `meta_len ≠ 16` | `Conflict` |
| `file_size = 0` 而 `chunk_count ≠ 0`（反之亦然） | `Conflict` |
| `file_size > media_upload_max_len`（預設 10 GiB） | `TooLarge` |
| `chunk_size` 不在 `[4 KiB, 16 MiB]` | `Conflict` |
| `chunk_count ≠ ceil(file_size / chunk_size)` | `Conflict` |
| data 超過 64 KiB | `TooLarge` |
| 這個使用者進行中的上傳（含舊式 pending）≥ `max_pending_media_uploads`（預設 5） | `TooLarge` |

Ack meta：

```json
{ "id": 1234605616436508552, "mxc": "mxc://example.org/1122334455667788",
  "chunk_size": 65536, "chunk_max_bytes": 69632, "expires_at": 1788516156 }
```

- `id`：之後每個 pack 標頭的 `id`。64-bit 隨機，server 發號前確認沒和進行中的上傳、既有媒體、墓碑撞到。
- `mxc`：**同一個值**的另一種寫法，media id = `id` 的 16 位小寫 hex。房間事件的 `url` 用它。
- `chunk_max_bytes` = `chunk_size + media_chunk_overhead_max`（預設 4 KiB）：這個上傳每塊 data 的上限。
- `expires_at`：Unix 秒；每收一塊往後推 `media_upload_ttl`（預設 86400 秒）。

### 3.2 `Chunk`（subtype `0x02`，id = 上傳 id，**seq = 塊索引**）

meta 空。**data = 第 `seq` 塊的密文**，多長 client 自己定，每塊可以不同；server 只要求 `1 ≤ data_len ≤ chunk_max_bytes`。
最後一塊（`seq = chunk_count − 1`）可以帶 `IS_LAST`；帶在別塊上會被拒。**串流模式**下 `IS_LAST` 是必要的：帶在哪一塊，上傳就在那一塊結束。

順序：同一個 `id` 之內 `seq` 必須 0, 1, 2, … 依序到。可以連續送不等回應（滑動窗口 client 自訂）。

| 情況 | 回應 |
|---|---|
| `seq` = server 正在等的那塊 | Ack `{ "received": n, "chunk_count": c, "total_len": bytes, "finished": bool, "truncated": false }` |
| `seq` < 已收塊數（重送、丟了 Ack） | 同上的 Ack，**不重寫**，冪等 |
| `seq` > 已收塊數 | `Error` `OutOfOrder`，帶 `"expected_seq": <該送的塊>` |
| `seq ≥ chunk_count`、已完成後再送、`IS_LAST` 帶錯塊、空 data | `Error` `Conflict` |
| `data_len > chunk_max_bytes` | `Error` `TooLarge` |
| `data_crc` 不對 | `Error` `Corrupt`（標頭仍抄回 `id`/`seq`，可直接重送） |
| 累計 bytes 會超過 `media_upload_max_len` | **這塊不收**，上傳強制結束：`Error` `Truncated`，帶 `received`、`total_len`、`finished: true`、`truncated: true`；之後仍可 `Seal`，媒體帶 `truncated` 標記 |

收到第 `chunk_count − 1` 塊（串流：收到帶 `IS_LAST` 的塊），`finished` 變 `true`。回應裡 `chunk_count` 在串流模式為 `null`。

### 3.3 `Status`（subtype `0x03`）

meta、data 皆空。Ack meta：

```json
{ "received": 3, "chunk_count": 5, "total_len": 49216, "finished": false, "truncated": false,
  "chunk_size": 16384, "file_size": 65550 }
```

**續傳**：斷線重連後問一次 Status，從 `seq = received` 接著送。或者直接送，吃 `OutOfOrder` 的 `expected_seq`。

### 3.4 `Seal`（subtype `0x04`）

meta 空。**data 可選**：非空就拿它取代 `Create` 時的加密描述（串流到結尾才知道大小、雜湊），上限 64 KiB。要 `finished`（含 `truncated`）。Ack meta `{ "mxc": "mxc://…" }`。
seal 後這個上傳 id 就不存在了（`Status` 回 `NotFound`），媒體以 mxc 存取。

### 3.5 `Abort`（subtype `0x05`）

meta、data 皆空。刪掉進行中的一切。Ack meta `{ "ok": true }`。
不 Abort 也不會卡住：`media_upload_ttl` 內沒有新塊，server 自動清。

### 3.6 完整流程

```
Create(EncryptedFileInfo, 加密描述)  → Ack{id, mxc}          // 串流：file_size=0, chunk_count=0
Chunk(id, seq=0, ct_0)               → Ack{received:1}
Chunk(id, seq=1, ct_1)               → Ack{received:2}
…
Chunk(id, seq=c-1, ct_{c-1}, IS_LAST) → Ack{received:c, finished:true}
Seal(id[, 新的加密描述])            → Ack{mxc}
把 mxc 與解密所需資料放進房間的加密事件
```

## 4. 下載（kind `0x04`，id 一律 0，mxc 放 meta）

### 4.1 `Info`（subtype `0x01`）

meta：`{ "mxc": "mxc://…" }`。Ack：

- meta：`{ "total_len", "file_size", "chunk_size", "chunk_count", "truncated", "content_type", "read_len", "chunk_size_large" }`
  - `file_size`／`chunk_size`／`chunk_count`／`truncated`：分塊媒體才有，整檔媒體（舊上傳）為 `null`；串流上傳的 `file_size` 也是 `null`（server 不知道，真值在加密描述裡），`chunk_count` 是實際收到的塊數
  - `total_len`：物件在 server 上的總長（分塊媒體 = 所有密文塊加總）
  - `content_type`：整檔媒體才有；分塊媒體為 `null`（server 不知道）
- **data = `Create` 時那份加密描述**，原樣（整檔媒體為空）

### 4.2 `Read`（subtype `0x02`）

meta：`{ "mxc": "…", "chunk": i }` 或 `{ "mxc": "…", "pos": p }`（`pos` 是**明文位置**；兩個都給以 `chunk` 為準）。

分塊媒體：一次回**整整一塊**，照上傳時的 bytes。給 `pos` 時 server 算 `i = pos / chunk_size`。Ack：

- meta：`{ "chunk": i, "pos": i × chunk_size, "len": <這塊密文長度>, "chunk_size", "chunk_count", "total_len" }`
- data = 第 i 塊密文

client 解完整塊後，要的位置在塊內偏移 `要的 pos − 回的 pos`。`chunk ≥ chunk_count` → `Error` `Conflict`。

整檔媒體（沒塊）：`{ "mxc", "pos"?, "len"? }` 讀任意範圍，`len` 沒給用 `read_len`。Ack meta `{ "pos", "len", "total_len" }`，data = 讀出的 bytes。

墓碑（已刪媒體）→ `Error` `NotFound`。標準的 `GET /_matrix/client/v1/media/download/{server}/{id}` 對分塊媒體照樣整份給。

### 4.3 下載流程

```
Info(mxc)            → Ack{file_size, chunk_size, chunk_count, data=加密描述}   // 解描述拿金鑰等
Read(mxc, chunk=0)   → Ack{…, data=ct_0} → 解密
Read(mxc, pos=p)     → Ack{chunk=i, pos=i×chunk_size, data=ct_i} → 解密，跳到 p − pos
```

## 5. 錯誤碼

| code | 意思 | client 該做什麼 |
|---|---|---|
| `Unauthorized` | token 無效（HTTP 401） | 重新登入 |
| `Corrupt` | CRC 不對，帶 `DataCrc{expected, actual}` 或 `MetaCrc` | 重送同一個 pack |
| `OutOfOrder` | 帶 `expected_seq` | 從 `expected_seq` 重送 |
| `Conflict` | 請求與上傳狀態矛盾（見各表） | 通常是 client 邏輯錯，看 `message` |
| `TooLarge` | 超過某個上限 | 看 `message`；`Create` 時就會知道 |
| `Truncated` | 單檔上限到了，上傳被強制結束 | 決定要不要 `Seal` 這個不完整的檔 |
| `NotFound` | 沒這個上傳／不是你的／媒體已刪 | 重新 `Create`，或放棄 |
| `UnknownKind` | server 不認得這組 kind/subtype | client 版本問題 |
| `Internal` | server 錯 | 稍後重試 |

## 6. server 知道與不知道的

知道（都是明文事實或收到的紀錄）：`file_size`、`chunk_size`、`chunk_count`、每塊密文落在物件哪裡多長、線上總長、有沒有截斷。

不知道、也不判斷：任何一塊「該」多長、內容、檔名、MIME、金鑰、雜湊。**完整性只有兩個定義：CRC 對、序列沒漏。**
密文對不對、長度對不對，是 client 解密時的事。

## 7. 加密描述：client 自己定，這裡只是建議

`Create` 的 data 是 client 的密文，server 不規定格式。建議放的內容（解密後）：

```json
{ "name": "video.mkv", "mimetype": "video/x-matroska", "size": 132056,
  "cipher": "chacha20-poly1305", "nonce_base": "<8 bytes base64>",
  "chunk_size": 65536, "sha256": "<整檔明文雜湊，選用>" }
```

- 金鑰**不要**放這裡（描述本身是用什麼加密的？）：金鑰走房間事件的 `file.key`（Matrix 既有的分發），或你們自己的 key 機制。
- 塊加密建議：每檔一把 key，`nonce_i = nonce_base ‖ i`（u32 BE），`ct_i = AEAD(key, nonce_i, pt_i)`；每塊密文 = 明文 + 16 byte 標籤，剛好落在 `chunk_max_bytes` 內。
- 描述本身用同一把 key、獨立的 nonce 加密即可。

## 8. 上限與預設（server config）

| 名稱 | 預設 | 作用 |
|---|---|---|
| `media_chunk_size_default` | 64 KiB | `chunk_size = 0` 時 |
| `media_chunk_size_min` / `max` | 4 KiB / 16 MiB | `chunk_size` 範圍 |
| `media_chunk_overhead_max` | 4 KiB | 每塊 data 可比 `chunk_size` 多出多少 |
| `media_upload_max_len` | 10 GiB | 單檔上限（線上 bytes）；0 = 不限 |
| `media_upload_ttl` | 86400 秒 | 最後一塊後多久沒動視為遺棄 |
| `max_pending_media_uploads` | 5 | 每人同時進行中的上傳數 |
| `wbf_meta_max_bytes` / `wbf_data_max_bytes` | 64 KiB / 16 MiB + 4 KiB | 單包硬上限 |

## 9. 實際 bytes（由 e2e 的組包函式印出）

`Create`，`file_size = 132056 (0x203D8)`，`chunk_size = 65536`，`chunk_count = 3`，data 36 byte：

```
header   : 01 03 01 00  00 00 00 00 00 00 00 00  00 00 00 01
meta_len : 00 00 00 10                                         = 16
meta     : 00 00 00 00 00 02 03 d8  00 01 00 00  00 00 00 03   file_size ‖ chunk_size ‖ chunk_count
meta_crc : 86 d7 58 8d
data_len : 00 00 00 24                                         = 36
data     : <36 bytes of client ciphertext>
data_crc : 76 c4 e1 11
```

`Chunk`，id `0x1122334455667788`，seq 0，data 5 byte：

```
header   : 01 03 02 00  11 22 33 44 55 66 77 88  00 00 00 00
meta_len : 00 00 00 00
meta_crc : 8b 51 da 90
data_len : 00 00 00 05
data     : de ad be ef 01
data_crc : 8e 69 88 7a
```

最後一塊（seq 2，`IS_LAST`）：`header: 01 03 02 08 11 22 33 44 55 66 77 88 00 00 00 02`，其餘同上。

`Status`／`Seal`／`Abort`：32 byte，只有標頭不同（subtype `03`／`04`／`05`），meta、data 皆空，`data_crc = 00 00 00 00`。

`Read` by position：

```
header   : 01 04 02 00  00 00 00 00 00 00 00 00  00 00 00 08
meta_len : 00 00 00 39
meta     : {"mxc":"mxc://localhost/1122334455667788","pos":131079}
meta_crc : …
data_len : 00 00 00 00
data_crc : 00 00 00 00
```

Ack to Read（回應：kind 01、subtype 02、flags 04、id/seq 抄回）：

```
header   : 01 01 02 04  00 00 00 00 00 00 00 00  00 00 00 08
meta     : {"chunk":2,"chunk_count":3,"chunk_size":65536,"len":1000,"pos":131072,"total_len":132104}
data     : <整塊密文，1000 bytes>
```

CRC-32C 自檢向量：`"123456789"` → `E3069283`。

## 10. 與舊 client

分塊媒體是逐塊 AEAD 密文串起來，舊 client 走標準下載拿得到、解不開。只有**單塊**可能相容，條件是那一塊用 Matrix 標準附件加密（AES-256-CTR + SHA-256）並在事件帶標準 `file` 欄；server 不用為此做任何事。
