# 分塊上傳、續傳、range 下載（提案）

> **狀態：草案，等維護者同意。** 這是 [roadmap.md](roadmap.md) §2.1，核心設計
> [why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md) §5.2 的 server 端。
> 撰寫日期：2026-09-03。§9 列了需要維護者定的事，定了才開分支。
>
> 讀這份之前先知道：媒體的引用計數與刪除已經上線（[media-gc.md](media-gc.md)），
> 分塊媒體**仍是一個 mxc、一個計數**，本提案不動那套，只在它底下長出「一個 mxc 很多塊」。

## 1. 要拿到什麼

| 性質 | 現在 | 之後 |
|---|---|---|
| 大檔案 | 一個請求塞整個檔（`max_request_size` 預設 24 MiB），斷線從頭來 | 固定大小切塊、各自上傳；斷線只補缺的塊 |
| 完整性 | server 不驗 | client 送 Merkle root，server 用自己收到的塊重算比對；**雜湊的是密文，E2EE 不受影響** |
| 下載 | 整份給 | HTTP `Range` 只讀需要的塊；每塊可附 Merkle 證明；跳轉播放 = 請求對的塊 |
| 事件大小 | 與檔案無關（Matrix 本來就只放 mxc） | 不變，多帶 root、塊大小、總長 |
| 刪除 | `media.delete()` 前綴刪 | 同一條路，塊跟著走 |

## 2. 核心決定

### 2.1 塊存下來就是塊，收尾不重組

上傳完成後**不把塊拼成一個大檔**。理由：

- 重組要再讀寫一次全部 bytes，10 GB 的檔就是 10 GB 的 IO，換來的只是「跟現在一樣的一個物件」。
- range 下載要的正是「按塊取」；拼成一個物件之後反而要靠 provider 的 range get，而現在的 provider 抽象**沒有** range get（`provider.rs` 只有 `get`／`load`／`head`／`put`／`delete`），加了也是每個後端各自實作。
- 塊各自是物件，刪除走既有的前綴刪除，不用新路徑。

代價：分塊媒體的縮圖與 `get_stored` 要能「串接讀塊」。§4.3 處理。

### 2.2 Server 驗每一塊，收尾只算樹

塊送到時 server 邊寫邊算 sha256，**存到 DB 的是塊的雜湊**，bytes 進 storage。收尾時 Merkle 樹從 DB 裡的葉子雜湊算，**不再讀 bytes**。10 GB／1 MiB = 10240 片葉子 × 32 bytes = 320 KB，記憶體裡算完。

同一塊重送：雜湊相同 → 冪等，回 OK；雜湊不同 → 拒絕（409），因為 client 自己前後不一致，不是 server 該猜的事。

### 2.3 塊大小由 client 選，server 給範圍

核心設計 §7 待驗 3 說 1 MiB 或 4 MiB 都可行、要量。這裡不替它定：client 在建立上傳時宣告 `chunk_size`，
server 只檢查它在 `[media_chunk_size_min, media_chunk_size_max]` 內、是 2 的冪、且 ≤ `max_request_size`。
預設範圍 256 KiB 到 16 MiB。量出來之後改的是 client 的預設值，不是 server。

### 2.4 自己的端點命名空間 `/_wbf/media/v1/`

不塞進 `/_matrix/media/`：這是 fork 自己的協定，與 Matrix 相容不是目標，也不想跟上游未來的媒體端點撞名。
既有的 `/_matrix/media/v3/upload` 與 `/_matrix/client/v1/media/download` 都**不動**，小檔照舊走那邊；
分塊媒體也能從標準 download 端點整份下載（§4.3），舊 client 不會壞。

### 2.5 Merkle 樹的形狀

葉 = `sha256(塊 bytes)`；內部節點 = `sha256(左 ‖ 右)`；**奇數節點直接上提，不複製**（Certificate Transparency 的做法；
複製會讓兩種不同的塊序列算出同一個 root）。單塊的 root 就是那片葉子。證明 = 從葉到根每層的兄弟雜湊，附方向位元。
這是純函數，有完整單元測試，包含奇數塊數、單塊、證明驗證。

## 3. 資料

### 3.1 兩個新的 column family（`src/database/maps.rs`）

| CF | 鍵 | 值 | 說明 |
|---|---|---|---|
| `mediaid_chunked` | `mxc` | `Cbor(ChunkedUpload)` | 一個分塊上傳的狀態：`chunk_size`、`total_len`（收尾前是 client 宣告的上限）、`chunk_count`、`state`（`Uploading` / `Sealed`）、`root`（sealed 後）、`content_type`、`content_disposition`、`owner`、`created_at`、`expires_at` |
| `mediaid_chunk` | `mxc ‖ index(u32 BE)` | `sha256` 32 bytes | 收到的每一塊。前綴 seek 就是「收到哪些」；缺的 = 不在的 index |

`mediaid_chunked` 設 TTL（`media_chunked_upload_ttl` 的兩倍，兜底），但真正的清理靠 §5 的 sweeper，TTL 只是保險。
`mediaid_chunk` 不設 TTL：sealed 之後它是**永久的**索引，下載的 range 對應與證明都要它。

### 3.2 既有表怎麼接

- **`mediaid_file`**：sealed 時寫一列，跟現在 `create_file_metadata` 一樣（鍵 `mxc ‖ dim ‖ disposition ‖ content_type`），
  同一交易 `Init` 計數 —— 所以引用計數、收集器、墓碑、migrate **全部不用改**，它們看到的就是一個普通媒體。
- **`mediaid_user`**：同上，sealed 時寫。
- **`mediaid_pending`**：**不用**。分塊上傳有自己的狀態列。

### 3.3 物件命名

現在一個媒體一個物件，名字 `sha256(key)`。分塊媒體的塊放在 `{sha256(key)}/{index:08}`。
`remove_media_file` 要多一條：對分塊媒體按前綴列出再刪（`object_store` 有 `list(prefix)`；provider 要多一個 `list_prefix`）。

## 4. 端點

全部要 access token，跟媒體其他端點一樣。錯誤碼沿用 Matrix 的 `errcode`，client 只認標準碼。

### 4.1 上傳

| 方法 | 路徑 | 做什麼 |
|---|---|---|
| `POST` | `/_wbf/media/v1/upload` | 建立。body：`chunk_size`、`total_len`、`content_type`、`filename`。回 `mxc`、`expires_at`。受 `max_pending_media_uploads` 與既有的 create 限流管 |
| `PUT` | `/_wbf/media/v1/upload/{server}/{id}/chunk/{index}` | 送一塊，body 是 bytes。最後一塊可以短。server 算 sha256、寫 storage、寫 `mediaid_chunk`。回 `{ "sha256": … }`。重送同塊：同雜湊 200，不同 409 |
| `GET` | `/_wbf/media/v1/upload/{server}/{id}` | 狀態：`received` 是已收到的 index 清單（或壓成區間），`missing` 是缺的。斷線重連先問這個 |
| `POST` | `/_wbf/media/v1/upload/{server}/{id}/seal` | 收尾。body：`root`、`total_len`。server 檢查塊齊、長度合、root 相等 → 寫 `mediaid_file` ＋ `Init` ＋ 改 state。root 不合 → 409，上傳留著讓 client 查 |
| `DELETE` | `/_wbf/media/v1/upload/{server}/{id}` | 放棄，刪塊與列。只有 owner 能 |

只有 owner 能碰自己的上傳（跟 `upload_pending` 一樣的檢查）。

### 4.2 下載

| 方法 | 路徑 | 做什麼 |
|---|---|---|
| `GET` | `/_wbf/media/v1/info/{server}/{id}` | `chunk_size`、`total_len`、`chunk_count`、`root`、`content_type` |
| `GET` | `/_wbf/media/v1/download/{server}/{id}` | 支援 `Range: bytes=a-b` → 206，只讀涵蓋的塊，切頭尾。沒有 `Range` → 200 整份串流 |
| `GET` | `/_wbf/media/v1/chunk/{server}/{id}/{index}` | 一整塊，回應標頭 `X-Wbf-Merkle-Proof`（base64 的兄弟雜湊序列＋方向）。client 拿到就能驗這塊屬於 root |

墓碑一樣擋在 `search_file_metadata`，所以這些端點對已刪媒體也回 410，不用另外處理。

### 4.3 既有端點對分塊媒體的行為

- `/_matrix/client/v1/media/download/…`：`get_stored` 看到 `mediaid_chunked` 有列且 sealed → 依序讀塊串接後回。**整份進記憶體**這件事跟現在一樣（現在也是 `Vec<u8>`），大檔在這條路上本來就不該走；要 range 就去 §4.2。
- 縮圖：分塊媒體**不做縮圖**，回 404。E2EE 下 server 只看得到密文本來就做不出來；明文的大檔（影片）縮圖要整份讀，不值得。§9 問維護者。

## 5. 未完成上傳的清理

一個 worker（形狀照 `rooms/retention`）每小時走 `mediaid_chunked`，state 是 `Uploading` 且 `expires_at` 過了 →
刪塊物件、刪 `mediaid_chunk` 前綴、刪狀態列。`expires_at` = 建立時間 ＋ `media_chunked_upload_ttl`（預設 24 小時），
每收到一塊就往後推（活著的上傳不會被清）。

與 migrate 的關係：進行中的上傳**沒有** `mediaid_file` 列，`get_all_mxcs()` 不會列出它，migrate 不會把它當孤兒。
sealed 之後才有列，那時計數是 0、mtime 是剛剛 → 落在 `media_gc_migrate_skip_recent_seconds` 內，同樣不會被誤刪。
這條跟現在的「上傳不是引用」是同一件事，不用新規則。

## 6. 事件裡放什麼

```json
{
  "msgtype": "m.file",
  "body": "video.mkv",
  "url": "mxc://example/abc",
  "wbf.chunked": { "root": "<hex sha256>", "chunk_size": 4194304, "total_len": 10737418240 }
}
```

`url` 仍是 mxc，所以 `list_content_mxc_uris` 找得到、引用計數照算。`wbf.chunked` 是給自己的 client 看的，
不認識的 client 當普通檔案：點 `url` 走標準 download，整份拿到。E2EE 下 `url` 在 `file` 裡，一樣。

## 7. 設定

| 設定 | 預設 | 意義 |
|---|---|---|
| `media_chunk_size_min` | 256 KiB | client 宣告的 `chunk_size` 下限 |
| `media_chunk_size_max` | 16 MiB | 上限；同時受 `max_request_size` 管 |
| `media_chunked_max_len` | 0（不限） | 單一上傳總長上限 |
| `media_chunked_upload_ttl` | 86400 | 未完成上傳多久沒動就清 |

`max_pending_media_uploads`、`media_rc_create_*` 沿用，分塊上傳與 pending 上傳算同一個額度。

## 8. 分幾支

| 支 | 內容 | 驗收 |
|---|---|---|
| A | Merkle 純函數＋單元測試；兩個 CF；provider 的 `list_prefix`；上傳四個端點＋狀態＋sweeper | e2e：三塊上傳、故意漏一塊、查狀態、補上、seal 成功；錯 root 被拒；標準 download 拿到跟原檔一樣的 bytes；`refcount` 是 0；redact 後收集器刪掉三個塊物件；未完成上傳過期被 sweeper 清 |
| B | `info`、range 下載、單塊＋證明 | e2e：`Range` 的 bytes 與原檔對應區段相同；中間一塊的證明能驗到 root；跨塊邊界的 range 正確 |

A 先，因為它把「塊存在哪、怎麼刪」定下來；B 只是讀。兩支各自能合併、各自有用。

## 9. 需要維護者定的

1. **塊大小範圍**：256 KiB 到 16 MiB 這個範圍可以嗎？（client 預設值另外量，§2.3）
2. **不重組**（§2.1）：接受「分塊媒體走標準 download 是串接讀塊」嗎？
3. **分塊媒體不做縮圖**（§4.3）：接受嗎？還是明文影片要縮圖？
4. **命名空間 `/_wbf/media/v1/`**：這個字可以嗎？它會出現在 client 的程式碼裡，之後不好改。
5. **要不要給舊的整檔上傳設上限**把 client 推去分塊（例如 `max_request_size` 之外再一個 `media_legacy_upload_max`）？我傾向**現在不要**，等自己的 client 有了再說。

## 10. 明確不在這支裡

- 客戶端（核心設計 Phase 2）。這裡只定協定與 server。
- 去重。核心設計 §5.4 說了 E2EE 下不成立，不當賣點。
- 上傳中的塊做內容檢查。密文，做不到。
- 聯邦。分塊媒體不透過聯邦提供；`allow_federation` 開著時遠端拿到的是標準 download 的整份。
