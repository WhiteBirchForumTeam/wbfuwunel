# 分塊上傳、續傳、range 下載（提案，第二版）

> **狀態：草案，等維護者同意。** 這是 [roadmap.md](roadmap.md) §2.1，核心設計
> [why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md) §5.2 的 server 端。
> 第二版依維護者 2026-09-03 的架構指示重寫：**以塊為單位加密、CRC 校驗、塊大小動態決定後固定、
> 預設 32 或 64 KiB、上傳先做 HTTP 再上 WebSocket、未完成上傳超時視為遺棄。**
> 第一版的 Merkle 樹與「塊存下來就是塊」都拿掉了，理由在 §2。
>
> 讀這份之前先知道：媒體的引用計數與刪除已經上線（[media-gc.md](media-gc.md)），
> 分塊媒體**仍是一個 mxc、一個計數、一個物件**，本提案不動那套。
> WebSocket 通道的外框在 [wbf-wire-format.md](wbf-wire-format.md)。

## 1. 要拿到什麼

| 性質 | 現在 | 之後 |
|---|---|---|
| 大檔案 | 一個請求塞整個檔（`max_request_size` 預設 24 MiB），斷線從頭來 | 固定大小的塊各自送；斷線只補缺的塊 |
| 加密 | client 整份加密一次（AES-CTR ＋ 整份一個 SHA-256），seek 不了 | **以塊為單位加密**（AEAD），每塊獨立可解 |
| 完整性 | server 不驗 | 傳輸層 CRC-32 抓損壞；內容層由每塊的 AEAD 標籤抓竄改（client 解密時驗） |
| 下載 | 整份給 | 帶 `pos` ＋ `len` 只讀需要的部分；跳轉播放 = 請求對的塊 |
| 事件大小 | 與檔案無關 | 不變，多帶塊大小、總長、金鑰資訊 |
| 刪除 | `media.delete()` | **完全不變**：sealed 之後就是一個普通媒體 |

## 2. 核心決定

### 2.1 塊是傳輸單位，不是儲存單位

64 KiB 的塊在 10 GB 的檔上是 16 萬個；一塊一個物件會把本地檔案系統與 object store 都拖垮。所以：

- 上傳中：塊寫進**一個暫存檔**（`{media_dir}/staging/{upload_id}`），第 i 塊寫在偏移 `i × chunk_size`。
  暫存檔是稀疏的，缺的塊就是洞；哪些塊到了記在 DB（§3）。
- seal：確認塊齊，把暫存檔**當一個物件**送進 storage provider（既有的 multipart `put` 就是為大物件做的），
  然後寫 `mediaid_file` ＋ `Init` 計數 —— 跟今天 `create()` 做的事一模一樣。
- 之後這個媒體跟其他媒體**沒有任何差別**：`get_stored`、縮圖、`delete()` 前綴刪、收集器、墓碑、migrate 全部原樣。

seek 靠 provider 的 **range get**：`object_store` 本來就有 `get_range`，只是 `src/service/storage/provider.rs` 沒露出來，加一個方法。
本地檔案系統後端的 range get 就是 `pread`，零成本。

### 2.2 塊大小：第一塊定下來，之後固定；預設 64 KiB

- client 在建立上傳時宣告 `chunk_size` 與 `total_len`；沒宣告 `chunk_size` 就用 server 預設 `media_chunk_size_default`（**64 KiB**；
  32 KiB 也在允許範圍內，§9 問維護者選哪個當預設）。
- 第一塊之後 `chunk_size` **不能改**。最後一塊可以短：120 KB 用 64 KiB 就是 64 ＋ 56。
- server 檢查：`media_chunk_size_min ≤ chunk_size ≤ media_chunk_size_max`（預設 16 KiB 到 1 MiB），
  且 `chunk_size ≤ max_request_size`、`≤ wbf_frame_max_bytes`。
- 塊數 = `ceil(total_len / chunk_size)`；`total_len` 若不知道（串流產生的檔）可以先給上限，seal 時給真值，
  但真值 ≤ 上限、且最後一塊之後不能再有塊。

**為什麼是 64 KiB 這個量級**：它是 AEAD 一次處理、一個 WebSocket 訊框、一次 range 讀取都舒服的大小；
每塊 16 bytes 的 AEAD 標籤在 64 KiB 上是 0.02% 的開銷。1 MiB 以上的塊會讓 seek 的最小讀取量變大、斷線重傳的損失變大，
沒有換到什麼。

### 2.3 以塊為單位加密，server 只看密文與 CRC

client 端：

```
key    = 每個檔案一把（隨機 256 bit），走既有的事件金鑰分發
nonce_i = base_nonce ‖ i（96 bit，i 是塊序號）
ct_i   = AEAD_encrypt(key, nonce_i, pt_i)        // AES-256-GCM 或 ChaCha20-Poly1305，含 16 byte 標籤
```

- 送上來的是 `ct_i`；server **不知道也不在乎**它是密文。塊在線上的大小 = `chunk_size + 16`（標籤），
  所以 server 存的物件比明文長 `16 × 塊數`；`total_len` 指的是**密文物件**的總長。
- 完整性：每塊各自的 AEAD 標籤，client 解密時驗；某一塊壞了只有那一塊解不開，不影響前後。
- 傳輸損壞：每塊帶 CRC-32（HTTP 走標頭 `X-Wbf-Crc32`，WebSocket 走外框），server 收到先驗 CRC，不合就拒收要重送。
  **CRC 不是完整性保證**，它只讓「網路把 bytes 弄壞」不必等到 client 解密時才發現。

**Merkle 樹拿掉的理由**：它的用處是「不信任 server 時驗證部分下載」。這裡 server 是維護者自己的，
而且每塊有 AEAD 標籤，client 拿到任一塊都能自己驗；再算一棵樹沒有多買到東西。

### 2.4 傳輸：先 HTTP，再 WebSocket；兩者同一套語意

同一個上傳可以混用：HTTP 送幾塊、WebSocket 送幾塊，server 看到的都是「第 i 塊到了」。
**HTTP 先做**（簡單、curl 就能測），WebSocket 在流式訊息的通道做好之後接上（§6）。

### 2.5 自己的命名空間 `/_wbf/media/v1/`

既有 `/_matrix/media/v3/upload` 與 `/_matrix/client/v1/media/download` **不動**：小檔照舊，舊 client 也能整份下載分塊媒體
（它就是一個普通物件）。

## 3. 資料

### 3.1 一個新的 column family

| CF | 鍵 | 值 | 說明 |
|---|---|---|---|
| `mediaid_upload` | `mxc` | `Cbor(Upload)` | 進行中的上傳：`chunk_size`、`total_len`（上限或真值）、`chunk_count`、`received`（bitmap，`chunk_count` 位）、`owner`、`content_type`、`content_disposition`、`created_at`、`last_chunk_at`、`state`（`Uploading` / `Sealing`） |

一個上傳一列，bitmap 放在值裡：10 GB／64 KiB = 16 萬位 = 20 KB，每收一塊改寫一次，可接受；
要更省就改成 `mxc ‖ index` 一塊一列，seek 前綴數。先用 bitmap。

TTL 設 `media_upload_ttl × 2` 當兜底，真正的清理是 §5 的 sweeper。**sealed 之後這列刪掉**，媒體只剩 `mediaid_file`。

### 3.2 既有表：seal 時寫，跟現在一樣

`mediaid_file`（一列，`dim = 0×0`）＋ `mxc_refcount` 的 `Init`，同一交易；`mediaid_user` 一列。
`mediaid_pending` **不用**（分塊上傳有自己的狀態列），但**共用額度**：`max_pending_media_uploads` 算的是 pending ＋ upload 的總數。

## 4. HTTP 端點

全部要 access token。`errcode` 沿用 Matrix 標準碼。

### 4.1 上傳

| 方法 | 路徑 | 做什麼 |
|---|---|---|
| `POST` | `/_wbf/media/v1/upload` | 建立。body：`total_len`（必填，可以是上限）、`chunk_size`（選填）、`content_type`、`filename`。回 `mxc`、`chunk_size`、`chunk_count`、`expires_at` |
| `PUT` | `/_wbf/media/v1/upload/{server}/{id}/{index}` | 送第 `index` 塊，body 是 bytes，標頭 `X-Wbf-Crc32`。長度必須等於 `chunk_size`，最後一塊可短。CRC 不合 → 400 重送；重送同塊 → 覆寫同一偏移，冪等；`index ≥ chunk_count` → 400 |
| `GET` | `/_wbf/media/v1/upload/{server}/{id}` | 狀態：`received` 是已到的 index 區間清單（`[[0,41],[43,43]]`），`missing` 同型。斷線重連先問這個 |
| `POST` | `/_wbf/media/v1/upload/{server}/{id}/seal` | 收尾。body：`total_len`（真值，選填）。server 檢查塊齊 → 暫存檔進 provider → `mediaid_file` ＋ `Init` → 刪 `mediaid_upload` 列 → 刪暫存檔。缺塊 → 409 附 `missing` |
| `DELETE` | `/_wbf/media/v1/upload/{server}/{id}` | 放棄：刪暫存檔與列。只有 owner 能 |

### 4.2 下載

| 方法 | 路徑 | 做什麼 |
|---|---|---|
| `GET` | `/_wbf/media/v1/info/{server}/{id}` | `total_len`、`content_type`；分塊媒體 seal 時記下的 `chunk_size` 也回（給 client 算塊邊界） |
| `GET` | `/_wbf/media/v1/download/{server}/{id}?pos=…&len=…` | 從 `pos` 起讀 `len` bytes，回 206；`len` 沒給就用 `media_download_default_len`（預設 1 MiB）；`pos` 沒給就 0。也接受標準 `Range` 標頭，語意相同 |

client 想解第 i 塊就要 `pos = i × (chunk_size + 16)`、`len = chunk_size + 16`；info 給了 `chunk_size` 就算得出來。
墓碑一樣擋在 `search_file_metadata`，已刪媒體回 410。

## 5. 超時與遺棄

- `media_upload_ttl`（預設 **86400** 秒）：從**最後一塊**到達起算，超過沒動就視為遺棄。每收一塊把 `last_chunk_at` 往後推，
  所以慢慢傳的上傳不會死，只有停掉的才會。
- sweeper：一個 worker（形狀照 `rooms/retention`）每小時掃 `mediaid_upload`，`last_chunk_at + ttl < now` → 刪暫存檔、刪列。
- 啟動時也掃一次：程序死掉重啟後，`staging/` 裡沒有對應列的檔案直接刪（列是真相，檔不是）。
- 與 migrate 的關係：進行中的上傳**沒有** `mediaid_file` 列，`get_all_mxcs()` 不會列出它，不會被當孤兒。
  seal 之後計數是 0、mtime 是剛剛 → 落在 `media_gc_migrate_skip_recent_seconds` 內，同樣不會被誤刪。這就是現在的「上傳不是引用」。

## 6. WebSocket 上傳（第二階段）

走 [wbf-wire-format.md](wbf-wire-format.md) 的通道，kind = `Upload`：

| subtype | payload | 對應 HTTP |
|---|---|---|
| `Chunk` | Cbor 標頭 `{ mxc, index }` ＋ 塊 bytes | `PUT …/{index}`；CRC 由外框驗，不另帶 |
| `Status` | `{ mxc }` → server 回 `{ received, missing }` | `GET …` |
| `Seal` | `{ mxc, total_len? }` → server 回 OK 或 `{ missing }` | `POST …/seal` |

建立與放棄仍走 HTTP（一次性的事）。Ack：`Chunk` 一律回 `Ack`（client 據此推進、或超時重送），
其他兩個本來就有回應。同一連線可以連續送很多塊不等 Ack（滑動窗口由 client 決定，server 不限制）。

## 7. 事件裡放什麼

```json
{
  "msgtype": "m.file",
  "body": "video.mkv",
  "url": "mxc://example/abc",
  "wbf.chunked": { "chunk_size": 65536, "total_len": 10737418240, "cipher": "aes-256-gcm", "nonce_base": "<base64 8 bytes>" },
  "file": { "key": …, "iv": … }
}
```

`url` 仍是 mxc，`list_content_mxc_uris` 找得到、引用計數照算。`wbf.chunked` 給自己的 client 看；不認識的 client 當普通檔案，
點 `url` 整份拿到（但解不開，因為加密方式不是 Matrix 的整份 CTR —— 這是**已知且接受的**：核心設計 §6 Phase 0 的退化策略是
「看得到一個連結」，不是「舊 client 也能看」）。E2EE 金鑰仍走既有的 `file.key` 欄位分發。

## 8. 設定

| 設定 | 預設 | 意義 |
|---|---|---|
| `media_chunk_size_default` | 64 KiB | client 沒宣告時用的塊大小 |
| `media_chunk_size_min` / `max` | 16 KiB / 1 MiB | 允許範圍 |
| `media_upload_ttl` | 86400 | 最後一塊之後多久沒動視為遺棄 |
| `media_upload_max_len` | 0（不限） | 單一上傳總長上限 |
| `media_download_default_len` | 1 MiB | `len` 沒給時一次回多少 |
| `wbf_frame_max_bytes` | 1 MiB | 外框上限（[wbf-wire-format.md](wbf-wire-format.md)） |

`max_pending_media_uploads`、`media_rc_create_*` 沿用。

## 9. 需要維護者定的

1. **預設塊大小 32 KiB 還是 64 KiB？** 我傾向 64（AEAD 標籤開銷減半、塊數減半），32 也在範圍內、client 可宣告。
2. **AEAD 選 AES-256-GCM 還是 ChaCha20-Poly1305？** 手機沒有 AES 硬體加速時 ChaCha 快；有的話 GCM 快。可以兩個都支援、事件裡標 `cipher`。
3. **`total_len` 可以先給上限、seal 再給真值**（§2.2）—— 為了「串流產生的檔」；接受這個複雜度嗎？不接受就必填真值。
4. **命名空間 `/_wbf/`** 這個字可以嗎？會進 client 程式碼。
5. **要不要給舊的整檔上傳設上限**把 client 推去分塊？我傾向現在不要。

## 10. 分幾支

| 支 | 內容 | 驗收 |
|---|---|---|
| A | CF、暫存檔、四個 HTTP 上傳端點、sweeper、provider `get_range`、`info`／`download?pos&len` | e2e：三塊上傳、故意漏一塊、查狀態、補上、seal；seal 後標準 download 拿到跟送上去一樣的 bytes；`refcount` 是 0；redact 後收集器刪掉；未完成上傳過期被清；`pos/len` 讀出的區段與原 bytes 一致 |
| B | WebSocket 通道（外框、Hello、Ack、Error）＋ `Upload` kind | e2e：同一上傳 HTTP 送前半、WS 送後半、seal 成功；CRC 壞的框被拒並重送成功 |

A 先，B 與流式訊息共用通道，誰先做都行。

## 11. 明確不在這支裡

- 客戶端（核心設計 Phase 2；[roadmap.md](roadmap.md) §4）。
- 去重。核心設計 §5.4 說了 E2EE 下不成立。
- 聯邦。分塊媒體不透過聯邦提供。
- 內容檢查。密文，做不到。
