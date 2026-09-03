# 分塊上傳、續傳、range 下載（提案，第三版）

> **狀態：維護者 2026-09-03 同意（PR #15），實作中（分支 `media/chunked-upload-a`）。** §9 的答案已寫回各節。
> 這是 [roadmap.md](roadmap.md) §2.1，核心設計
> [why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md) §5.2 的 server 端。
> 第三版依維護者 2026-09-03 的指示：**以 WebSocket 通道為主，所有請求與回應都是同一種二進位 pack
> （[wbf-wire-format.md](wbf-wire-format.md)）；HTTP 只是「把一個 pack 用 POST 送一次」的選用測試路徑，
> 不另做 JSON 加 base64 的端點。** 塊的加密、CRC、塊大小、超時等第二版的決定不變。
>
> 讀這份之前先知道：媒體的引用計數與刪除已經上線（[media-gc.md](media-gc.md)），
> 分塊媒體**仍是一個 mxc、一個計數、一個物件**，本提案不動那套。

## 1. 要拿到什麼

| 性質 | 現在 | 之後 |
|---|---|---|
| 大檔案 | 一個請求塞整個檔（`max_request_size` 預設 24 MiB），斷線從頭來 | 固定大小的塊各自送；斷線只補缺的塊 |
| 加密 | client 整份加密一次，seek 不了 | **以塊為單位加密**（AEAD），每塊獨立可解 |
| 完整性 | server 不驗 | 傳輸層 CRC-32 抓損壞；內容層由每塊的 AEAD 標籤抓竄改（client 解密時驗） |
| 下載 | 整份給 | 帶 `pos` ＋ `len` 只讀需要的部分；跳轉播放 = 請求對的塊 |
| 傳輸 | 每塊一個 HTTP 請求 | 一條 WebSocket 連線，每塊 32 bytes 外框 |
| 刪除、縮圖、計數 | — | **完全不變**：sealed 之後就是一個普通媒體 |

## 2. 核心決定

### 2.1 塊是傳輸單位，不是儲存單位

64 KiB 的塊在 10 GB 的檔上是 16 萬個；一塊一個物件會把檔案系統與 object store 都拖垮。所以：

- 上傳中：塊寫進**一個暫存檔**（`{media_dir}/staging/{upload_id}`），第 i 塊寫在偏移 `i × wire_chunk_size`（§2.3）。暫存檔是稀疏的，缺的塊是洞；哪些塊到了記在 DB（§3）。
- seal：確認塊齊，把暫存檔**當一個物件**送進 storage provider（既有的 multipart `put`），寫 `mediaid_file` ＋ `Init` 計數 —— 跟今天 `create()` 一模一樣。
- 之後這個媒體跟其他媒體**沒有任何差別**。

seek 靠 provider 的 **range get**：`object_store` 有 `get_range`，`src/service/storage/provider.rs` 露出一個方法即可；本地檔案系統就是 `pread`。

### 2.2 塊大小：兩種規格，`Create` 時定下來，之後固定

同一套架構，兩種預設：

| 規格 | `chunk_size`（明文塊） | 用在 | 理由 |
|---|---|---|---|
| **small** | **64 KiB** | 小檔、線路差、要細粒度 seek 的媒體 | 一次 AEAD、一個 pack、一次 range 讀取都舒服；斷線重傳損失小；標籤開銷 0.02% |
| **large** | **1 MiB** | 大檔、好線路 | 塊數少 16 倍，Ack 數量也少；1 MiB 在記憶體裡加密一次幾 ms（維護者 2026-09-03：4 MiB 太大，改 1 MiB） |

client 預設只有這兩個數字，64 KiB 是預設。但那是 client 的選擇：server 允許 4 KiB 到 16 MiB 之間任何值，由上傳者決定，server 只記下它選了多大、收了幾塊。

- client 在 `Create` 宣告 `chunk_size`（**明文**塊的精確大小）；沒宣告就用 server 的 `media_chunk_size_default`（small，64 KiB）。**之後不能改**。
- `total_len` **不宣告**。server 不需要知道檔有多大：塊一塊一塊來，server 累計；最後一塊帶 `IS_LAST` 旗標，到那時大小才確定。串流產生的檔因此與一般檔沒有分別：client 的 buffer 滿 64 KiB 就發一塊，最後剩下的（例如 30 KiB）就是最後一塊。
- **server 不檢查塊的長度是否「對」**。data 段是 client 封裝（加密、標籤、nonce、框架）之後的密文，封裝後多大是 client 的事；精確的 64 KiB 在**解封裝**那端檢查，解密出來不是 64 KiB 就是那一塊壞了。server 對「完整」的定義只有兩樣：`data_crc` 對、序列沒漏。
- **封裝後每一塊多長都可以不同**，server 不記住任何「應該多長」、不拿前一塊檢查後一塊；它只**記錄**每一塊落在哪、多長（`mxc_chunk`，§3.1）。server 知道的是明文塊大小（宣告的），而最後一塊的明文允許比它小。
- server 檢查的是**上限**（防攻擊，不是定義）：`media_chunk_size_min ≤ chunk_size ≤ media_chunk_size_max`（預設 **4 KiB** 到 **16 MiB**）；每一塊 `data_len ≤ chunk_size + media_chunk_overhead_max`（預設冗餘 4 KiB，蓋標籤與封裝），超過就 `Error(TooLarge)`，所以 client 宣告了塊大小就不能中途把塊變大；`chunk_size + media_chunk_overhead_max ≤ wbf_data_max_bytes`（單包硬上限，meta 與 data 各一個，不讓任何一段無限長）。空塊拒收。
- **一個塊正好是一個 pack 的 data 段**：client 把第 i 塊明文加密後直接寫進 pack 的 `data_slot`（[wbf-wire-format.md](wbf-wire-format.md) §5），
  沒有第二次加密、沒有複製。

### 2.3 以塊為單位加密，server 只看 CRC

client 端：`key` 每個檔案一把；`nonce_i = base ‖ i`；`ct_i = AEAD(key, nonce_i, pt_i)`，含 16 byte 標籤。
送上來的是 `ct_i`，它**就是** pack 的 data 段；暫存檔與最終物件存的是密文，`total_len` 指**密文物件**總長（server 累計出來的）。
每塊各自的標籤在 client 解密時驗；壞一塊只壞一塊。CRC 由 pack 的 `data_crc` 驗（硬體 CRC-32C），不合就拒收要重送。
**pack 不碰加密**：加密在封裝前、解密在拆包後，都在同一塊緩衝上原地做。server 收到 `Chunk` 是把 data 切片直接 `pwrite` 進暫存檔，也不複製。

**Merkle 樹拿掉**：server 是維護者自己的，每塊有 AEAD 標籤、client 拿到任一塊都能自驗，樹沒多買到東西。

### 2.4 所有互動都是 pack

上傳的建立、送塊、查狀態、封存、放棄，下載的查詢與讀取，**每一個都是一個 pack 進、一個 pack 出**。
WebSocket 上是一框一 pack；HTTP 上是 `POST /_wbf/v1/pack` 一次一 pack。server 端一個入口函式 `handle_pack(user, Pack) -> Pack`，
兩種送法都呼叫它，所以**沒有兩套語意**。

### 2.5 自己的前綴

`/_wbf/v1/ws` 與 `/_wbf/v1/pack`（維護者 2026-09-03 定：照 `/_名字/版本/功能` 的慣例；端點本身就跟上游切開了，不借 `/_tuwunel/`）。既有的 `/_matrix/media/v3/upload` 與 `/_matrix/client/v1/media/download` **不動**：小檔照舊，舊 client 也能整份下載分塊媒體（它就是一個普通物件）。

## 3. 資料

### 3.1 一個新的 column family

| CF | 鍵 | 值 | 說明 |
|---|---|---|---|
| `mxc_chunk` | `(mxc, index)` | `Cbor(ChunkSpan)`：`offset`、`len` | 每一塊落在物件的哪裡、多長，**收到時寫**（記錄，不是檢查）。server 解不了密、看不出塊的分界，下載要把第 i 塊照原樣交回去只能靠這列；一次點讀。跟媒體同壽命，abort／sweep／刪媒體時整個前綴刪 |
| `mxc_chunked` | mxc 字串 | `Cbor(ChunkedMedia)` | seal 後的分塊媒體長什麼樣：`chunk_size`（明文）、`chunk_count`、`total_len`（線上總長）。整檔上傳沒有這列。刪媒體時一起刪 |
| `mediaid_upload` | `upload_id`（u64 BE） | `Cbor(Upload)` | 進行中的上傳：`mxc`、`owner`、`chunk_size`、`total_len`（目前累計，也就是暫存檔長度與下一塊的偏移）、`received_count`（下一塊的 index）、`finished`（收到 `IS_LAST` 了沒）、`content_type`、`filename`、`created_at`、`last_chunk_at`。沒有 bitmap：有序序列的「已收」永遠是 0..n 連續，一個數字就夠，每塊一次列更新也因此是常數大小。沒有 `state` 欄：兩個 seal 撞在一起靠冪等收尾（計數的 `Init` 對既有列不動、物件與媒體列覆寫、第二次刪列刪檔容忍不存在），不靠狀態機 |

鍵是 `upload_id` 而不是 mxc，因為 pack 的標頭帶的是 `id`（u64），server 一個點讀就找到，不用解 meta。
`upload_id` 由 server 在 `Create` 時隨機發（64 bit），mxc 同時建好。

TTL 設 `media_upload_ttl × 2` 當兜底，真正的清理是 §6 的 sweeper。**sealed 之後這列刪掉**。

### 3.2 既有表：seal 時寫，跟現在一樣

`mediaid_file`（一列）＋ `mxc_refcount` 的 `Init`，同一交易；`mediaid_user` 一列。`mediaid_pending` **不用**，但**共用額度**：`max_pending_media_uploads` 算 pending ＋ upload 的總數。

## 4. 上傳：kind = `Upload`

標頭：`id = upload_id`（`Create` 時為 0），`seq` = 塊 index（`Chunk`）或請求號（其他）。meta 明文（server 要讀）。

| subtype | 請求 meta | 請求 data | 回應（`Ack` 的 meta） |
|---|---|---|---|
| `0x01 Create` | `{ "chunk_size"?: …, "content_type"?: "…", "filename"?: "…" }` | 無 | `{ "id": <upload_id>, "mxc": "mxc://…", "chunk_size": …, "chunk_max_bytes": …, "expires_at": … }`（`chunk_max_bytes` = `chunk_size + media_chunk_overhead_max`，這個上傳每塊 data 的上限） |
| `0x02 Chunk` | 無（`id`、`seq` 在標頭就夠） | 第 `seq` 塊的 bytes，長度由 client 定，每塊可以不同；最後一塊標頭帶 `IS_LAST` | `{ "received": <已到塊數>, "total_len": <累計 bytes>, "finished": bool }`。`data_crc` 不合 → `Error(Corrupt)`；`data_len > chunk_max_bytes` → `Error(TooLarge)`；空塊、或 `IS_LAST` 之後還有塊 → `Error(Conflict)`；`seq < received_count`（重送）→ 不重寫、再 Ack 一次，冪等 |
| `0x03 Status` | 無 | 無 | `{ "received": <已到塊數>, "total_len": …, "finished": bool, "chunk_size": … }`；續傳從 `seq = received` 接著送 |
| `0x04 Seal` | 無 | 無 | `{ "mxc": "…" }`。還沒收到 `IS_LAST` → `Error(Conflict)` 帶目前塊數與 bytes |
| `0x05 Abort` | 無 | 無 | `{ "ok": true }` |

`Chunk` 是**有序類**（[wbf-wire-format.md](wbf-wire-format.md) §4）：同一個 `id` 之內 `seq` 必須 0, 1, 2, … 遞增，server 記 `next_seq`；來的不是 `next_seq`
→ `Error(OutOfOrder)` 帶 `expected_seq`，client 從那裡重送。WebSocket 保證到達順序，所以順序錯一定是 client 邏輯錯。
可以連續送不等回應（滑動窗口 client 決定）；要逐塊確認就帶 `WANT_ACK`。續傳（斷線重連）：先 `Status` 拿到 `received`，從那個 index 當 `seq` 接著送；
或直接送，送錯了 server 的 `Error(OutOfOrder)` 就是「把第 `expected_seq` 塊再給我」。兩條都不需要 server 記任何除了 `received_count` 以外的東西。`Create`、`Status`、`Seal`、`Abort` 是無序類（一問一答）。
只有 owner 能碰自己的上傳；`id` 對不上或不是 owner → `Error(NotFound)`（不區分，避免探測）。

## 5. 下載：kind = `Download`

標頭：`id = 0`（mxc 在 meta 裡，因為它是字串），`seq` = 請求號。meta 明文。

| subtype | 請求 meta | 回應 |
|---|---|---|
| `0x01 Info` | `{ "mxc": "…" }` | `Ack` meta：`{ "total_len": …, "content_type": "…"｜null, "chunk_size": …｜null, "chunk_count": …｜null, "read_len": …, "chunk_size_large": … }`。兩個 `chunk_*` 只有分塊媒體有（`mxc_chunked` 列）；`read_len` 是整檔媒體 `Read` 沒給 `len` 時的預設；`chunk_size_large` 是 server 建議的大檔塊大小 |
| `0x02 Read` | `{ "mxc": "…", "chunk"?: i, "pos"?: …, "len"?: … }` | **分塊媒體**：一次回**整整一塊**，照上傳時的樣子，一個 byte 不差。給 `chunk` 就是那一塊；給 `pos`（沒給 `chunk`）—— **`pos` 是明文位置**，server **seek** 出含那個位置的塊（`pos / chunk_size`），client 不必知道任何封裝後的數字；`len` 不理。`Ack` meta：`{ "chunk": i, "pos": <這塊在物件裡的起點>, "len": <這塊長度>, "chunk_size": …, "chunk_count": …, "total_len": … }`，**data = 那一塊**。`chunk ≥ chunk_count`（含 `pos` 算出來的）→ `Error(Conflict)`。**整檔媒體**（沒加密、沒塊）：`pos`/`len` 讀任意範圍，`len` 沒給用 `media_download_default_len`，`Ack` meta `{ "pos", "len", "total_len" }` |

分塊媒體的塊邊界是收到時記下的（`mxc_chunk`，§3.1），不是算的：封裝後每塊多長 server 不知道也不管，所以要把第 i 塊照原樣交回去，只能靠當初記下的那一列。墓碑一樣擋在 `search_file_metadata`，已刪媒體回 `Error(NotFound)`；
標準的 `/_matrix/client/v1/media/download` 對分塊媒體照樣整份給，也照樣 410。

## 6. 超時與遺棄

- `media_upload_ttl`（預設 **86400** 秒）從**最後一塊**到達起算；每收一塊把 `last_chunk_at` 往後推。慢慢傳不會死，停掉的才會。
- sweeper：worker（形狀照 `rooms/retention`）每小時掃 `mediaid_upload`，過期 → 刪暫存檔、刪列。啟動時也掃一次，`staging/` 裡沒有對應列的檔直接刪（列是真相，檔不是）。
- 與 migrate：進行中的上傳沒有 `mediaid_file` 列，不會被當孤兒；seal 後計數 0、mtime 是剛剛，落在 `media_gc_migrate_skip_recent_seconds` 內。這就是「上傳不是引用」。

## 7. 事件裡放什麼

```json
{
  "msgtype": "m.file",
  "body": "video.mkv",
  "url": "mxc://example/abc",
  "wbf.chunked": { "chunk_size": 65536, "total_len": 10737418240, "cipher": "aes-256-gcm", "nonce_base": "<8 bytes>" },
  "file": { "key": …, "iv": … }
}
```

`url` 仍是 mxc，引用計數照算。不認識的 client 當普通檔案整份拿到但解不開 —— 已知且接受（核心設計 §6 Phase 0 的退化策略是「看得到一個連結」）。金鑰仍走既有的 `file.key` 分發。

## 8. 設定

| 設定 | 預設 | 意義 |
|---|---|---|
| `media_chunk_size_default` | 64 KiB（small） | `Create` 沒宣告時用 |
| `media_chunk_size_large` | 1 MiB | client 選 large 規格時的建議值（`Info` 回給 client 參考） |
| `media_chunk_size_min` / `max` | 4 KiB / 16 MiB | 允許範圍，範圍內由上傳者決定 |
| `media_chunk_overhead_max` | 4 KiB | 每塊 data 可以比宣告的 `chunk_size` 多出多少（標籤、nonce、封裝框架）；超過就 `TooLarge` |
| `media_upload_ttl` | 86400 | 最後一塊之後多久沒動視為遺棄 |
| `media_upload_max_len` | 0（不限） | 單一上傳總長上限 |
| `media_download_default_len` | 1 MiB | `Read` 沒給 `len` 時一次回多少 |
| `wbf_meta_max_bytes` / `wbf_data_max_bytes` | 64 KiB / 16 MiB + 4 KiB | 單包硬上限，meta 與 data 各一個（[wbf-wire-format.md](wbf-wire-format.md)） |

`max_pending_media_uploads`、`media_rc_create_*` 沿用。

## 9. 維護者定的（2026-09-03）

1. **塊大小**：範圍可以；small 64 KiB、large 4 MiB 照§2.2。client 的預設值另外量。
2. **AEAD**：維護者要求「選一個效能好的」—— 選 **ChaCha20-Poly1305**：沒有 AES 硬體加速的手機上它快得多，有加速的桌機上兩者都遠快於網路；
   事件裡仍標 `cipher`，之後要加 GCM 不用改格式。server 不碰加密，這條只影響 client。
3. **`total_len` 不宣告**（2026-09-03 維護者改定，取代原本的「先給上限、seal 給真值」）：塊數與總長由 server 累計，最後一塊帶 `IS_LAST`。同時定下：塊大小在 `Create` 定死、不能中途改；server 不檢查封裝後的長度是否「對」（那是解封裝那端的事），只檢查上限（`chunk_size + media_chunk_overhead_max`）防攻擊；meta 與 data 各有單包硬上限。第一版實作走了固定幾何（塊長必須等於格子），review 時發現它讓串流上傳送不出尾塊，整個拔掉。
4. **命名**：`/_wbf/v1/…`（§2.5）。
5. **舊的整檔上傳不設上限**，之後再說。
6. **分塊媒體不做縮圖**：都加密了，縮不了。
7. **下載交回去的必須是上傳時的那一塊**（2026-09-03）：server 解不了密、不能重新切塊。維護者明講：**封裝後每塊多長會浮動，server 不能記住第一塊多長拿來檢查後面的，server 從不檢查，檢查是 client 解碼的事**；server 知道的只有明文塊大小，最後一塊的明文可以比它小。所以邊界不是算的，是每塊收到時**記錄**下來的（`mxc_chunk`，記錄不是檢查）；seal 存 `chunk_size`、`chunk_count`、`total_len`。client 用**明文位置** `pos` 要，server 算 `pos / chunk_size` 找到那塊，回起點、這塊長度、明文塊大小、總塊數。（中間曾有一版「第 0 塊定長度、後面每塊一樣長、邊界用乘的」，維護者否決：那是 server 在檢查它不該懂的東西。）最小塊 4 KiB、最大 16 MiB，範圍內上傳者決定；client 預設只有 64 KiB（預設）與 1 MiB。
8. **預告**：未來所有 HTTP 請求都會遷到 WS，kind 的分配見 [wbf-wire-format.md](wbf-wire-format.md) §3.3。

## 10. 分幾支

| 支 | 內容 | 驗收 |
|---|---|---|
| A | `core/wbf/pack.rs`（Pack 型別、encode／decode、單元測試）；`mediaid_upload` CF；暫存檔；`handle_pack` 的 `Upload` 與 `Download` 兩個 kind；`POST /_wbf/v1/pack`；sweeper；provider `get_range` | e2e（用 HTTP 送 pack，curl 或腳本就行）：三塊上傳、故意漏一塊、`Status` 看到缺、補上、`Seal`；seal 後標準 download 拿到跟送上去一樣的 bytes；`refcount` 是 0；redact 後收集器刪掉；未完成上傳過期被清；`Read` 的 `pos/len` 區段與原 bytes 一致；壞 CRC 被拒且說是 data |
| B | WebSocket 通道（`/_wbf/v1/ws`、Hello、Ping、連線內多 id 分流），同一個 `handle_pack` 接上 | e2e：同一上傳 HTTP 送前半、WS 送後半、seal 成功；WANT_ACK 逐塊確認 |

A 先，因為它把 pack 與儲存定下來；B 與流式訊息共用通道。

## 11. 明確不在這支裡

- 客戶端（[roadmap.md](roadmap.md) §4）。
- 去重（核心設計 §5.4：E2EE 下不成立）。
- 聯邦。分塊媒體不透過聯邦提供。
- 內容檢查。密文，做不到。
