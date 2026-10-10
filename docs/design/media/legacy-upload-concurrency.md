# 傳統 Matrix 上傳的記憶體要有封頂：一個全站的 byte 預算

📄 **提案，尚未實作**。維護者 2026-10-07 定了三件：只做上限（issue #110 的選項 2）、
🚫 不做「關掉傳統上傳」的開關（選項 1）、上限是**記憶體預算而不是固定並行數**（預設 1 GiB）。
相關：[/docs/design/media/chunked-upload.md](chunked-upload.md)（wbf 自己那條路，沒有這個問題）。

## 1. 問題

`POST /_matrix/media/v3/upload`（以及先 `/create` 再 `PUT /upload/{server}/{media_id}`）**一個請求送完整個檔**，
而 `src/api/router/request.rs` 的 `axum::body::to_bytes(body, max_request_size)` 把整個 body 收進記憶體再交給 ruma
解成 `create_content::v3::Request`（`file: Vec<u8>`）。

現有的兩道閘門都是**單一請求**的：

| | 管什麼 | 🚫 不管什麼 |
|---|---|---|
| `DefaultBodyLimit::max(max_request_size)`（`layers.rs`）| 一個請求最多多大（預設 24 MiB）| 同時有幾個 |
| `RequestBodyTimeoutLayer`（`layers.rs`）| 一個請求可以收多久 | 同時有幾個 |
| `max_pending_media_uploads`（預設 5）| **每個使用者**還沒完成的非同步上傳數 | ⚠️ 不是並行上限，也不是全站的 |

⇒ 🚨 **最壞情況是「同時上傳的人數 × `max_request_size`」**，解析時可能再多一份複本。
100 個人同時傳上限大小 ⇒ 2.4–4.8 GB，**沒有封頂**。
📎 上傳要登入、註冊要 token，所以實際風險不高 —— ⭐ 但「不高」不是「有上限」。

📎 **wbf 的分塊上傳沒有這個問題**：一塊最多 1 MiB，server 手上永遠只有一小塊。這一份只講傳統那條。

## 2. 做法：一個全站的 **byte 預算**，在 `to_bytes` 之前預約

維護者 2026-10-07：

> N = Limit / Current MiB … 如果限制 500MiB，有辦法計算當前吃多少嗎？或是直接拿現在這個 process 吃多少 RAM？default 1 G。

⭐ **要的是記憶體預算，而不是固定的並行數量 —— 對，而這個 repo 已經有這個形狀。**
🚫 **但不是用「量 process 吃多少」來做**（§2.2 說為什麼）。

新設定 **`matrix_upload_memory_budget`**（預設 **1 GiB**）：一個全站的
`Semaphore`，**permit 就是 byte**。每個傳統上傳請求在**讀 body 之前**預約它要佔的位元組，
請求結束時歸還；預約不到就 **429 `M_LIMIT_EXCEEDED`** ＋ `retry_after_ms`。

⇒ ⭐ **N 自己浮動**：預算 1 GiB 下，24 MiB 的大檔同時 ~42 個，1 MiB 的小檔同時 ~1000 個 ——
📌 **小檔不會被大檔餓死**，而這正是「N = Limit / 每個的大小」。

⭐ **現成的形狀在 `streams/mod.rs` 的 `PackQueue`**（🚫 不自己發明第二種）：

| | |
|---|---|
| `budget: Arc<Semaphore>` ＋ `try_acquire_many_owned(bytes)` | permit ＝ byte 的預約 |
| `capacity_bytes` 留著 | ⭐ **比整個預算還大的東西直接拒絕**，而不是永遠等一個等不到的許可 |
| `OwnedSemaphorePermit` 跟著東西走 | ⚠️ 🚫 **不要手動 release**：中途斷線、逾時、解析失敗、handler panic，每一條路都要還，而 `Drop` 是唯一不會漏掉任何一條的方式 |

📎 那個檔案自己的註解就寫著這條原則：「**the byte budget is what decides the memory**」。

### 2.1 🚨 預約多少 —— 這一格決定「封頂」是真的還是希望

預約量取自 **`Content-Length`**，而 client 可以**謊報小的**。⇒ 要讓預約量**就是**實際上限：

```
reserve = min(Content-Length, max_request_size)        // 沒有 Content-Length（chunked）⇒ 用 max_request_size
to_bytes(body, reserve)                                 // ⭐ 上限用「預約到的那個數」，不是 max_request_size
```

⇒ 🚨 **謊報 1 MiB 卻送 24 MiB 的人，在他自己宣告的 1 MiB 就被截斷**（`to_bytes` 回 `TooLarge`）。
⭐ **所以封頂不是靠相信 client，是由構造成立**：同時佔用的位元組 ≤ 預算，永遠。
📎 缺 `Content-Length` 時預約最壞值，是 fail closed 的那一邊。

### 2.2 🚫 為什麼不是「量 process 吃多少 RAM」

技術上拿得到（`core/alloc/je.rs` 的 `memory_stats`），但**當閘門是錯的**，四個理由各自致命：

| | |
|---|---|
| 🚨 **跟上傳無關的記憶體會吃掉預算** | RocksDB 的 block cache 本來就會長到它的設定上限 ⇒ process RSS 幾乎永遠在高水位 ⇒ 上傳被莫名拒絕，而原因跟上傳無關 |
| 🚨 **這台機器上那個閘門等於不存在** | `memory_stats` 只有 jemalloc 版本有；`core/alloc/default.rs` 的版本**永遠回 `None`**，而 Windows 建不了 jemalloc ⇒ ⭐ **開發與測試的平台上它靜默失效**，那是最糟的形狀 |
| 🚨 **量測沒有「預約」語意** | 讀到 900 MiB → 十個請求同時讀到同一個數 → 十個都放行 → 爆。預約是原子的，量測不是 |
| ⚠️ **測不起來** | 要讓 process 真的吃到 1 GiB 才驗得到。byte 預算只要把預算設成 2 MiB 就測完了（§4）|

📎 jemalloc 的 `resident` 還含**未歸還 OS 的 arena** ⇒ 高估；而 `memory_stats` 回的是一大段**文字**，要 parse。

### 2.3 ⚠️ 只擋傳統上傳那幾條路，不是每個請求

🚫 **不能把閘門放在「所有請求」上** —— 那會讓一個慢上傳擋住登入、送訊息、聯邦。
⇒ 判準要是「這條路徑會把一個大 body 收進記憶體嗎」。📌 實作時要決定怎麼認：
看 method ＋ path 前綴（`/_matrix/media/*/upload`、`/_matrix/client/v1/media/upload`）是最直接的，
而 ⚠️ **路徑有兩代（legacy 與 v1）** —— 認漏一條就等於沒做。

## 3. 🚫 不做：「關掉傳統上傳」的開關

維護者 2026-10-07 問了一個好問題：**「開關應該就是把上傳限額設定成 0 byte 對嗎？如果參數支援就不需要額外寫開關。」**

⚠️ **現在辦不到，而且不是「參數不吃 0」這麼簡單**：

| | |
|---|---|
| `max_request_size` **不是上傳專用的** | 它同時是 `layers.rs` 的 `DefaultBodyLimit::max` 與 `request.rs` 的 `to_bytes` 上限 ⇒ 設 0 會擋掉**每一個帶 body 的請求**（登入、送訊息、聯邦），整台 server 不能用 |
| 而且**設不下去** | `config/check.rs`：低於 10 MB **直接拒絕啟動**（理由寫著聯邦需要）|
| 沒有媒體專用的上限 | `m.upload.size` 兩處回的都是 `max_request_size`（`api/client/media.rs`、`media_legacy.rs`）|

⭐ **但那個直覺指向一個比布林開關更好的形狀**，如果哪天要做就做這個：一個**媒體專用**的大小上限
（例如 `max_matrix_upload_size`，預設 ＝ `max_request_size`），`0` ＝ 關閉。
一個參數同時表達「關掉」與「只收小檔」，而且 `m.upload.size` 本來就該回它 ——
📎 那個欄位的語意是「上傳上限」，不是「請求上限」（請求還要裝 header 與 multipart 的框，
所以現在回 `max_request_size` 本來就偏大）。

📌 **寫在這裡的理由**：下一個人（或三個月後的我）看到「關掉上傳」會先想「設 0 不就好了」——
⚠️ 而那會變成一台不能登入的 server。

## 4. 測試計畫

| 要釘住的 | 怎麼測 |
|---|---|
| 🔴 **滿了回 429 而不是吃掉記憶體** | 上限設 1，兩個上傳同時進來 ⇒ 第二個 **429** ＋ `retry_after_ms` |
| 🔴 **許可會還** | 第一個做完之後第二個**成功** —— ⭐ 這條守的是「`Drop` 真的還了」，而它是這支最容易寫錯的一行 |
| 🚨 **中途斷線也還** | 送一半就斷 ⇒ 下一個上傳**不被擋**（📎 這條才是 `Drop` 不可替代的證據）|
| 🔴 **不擋別的請求** | 上限設 1、佔住它，然後登入／送訊息／`/versions` **照常成功** |
| 兩代路徑都算 | legacy（`/_matrix/media/v3/upload`）與 v1 兩條各自被算進同一個上限 |
| 分塊上傳不受影響 | wbf 的 `Upload` 一塊 1 MiB，不經過這道閘 |

## 5. ✅ 預設 1 GiB（維護者 2026-10-07）

⭐ **byte 預算比固定 N 好的地方就在這裡：最壞值就是那個設定的數字本身，不用乘。**

| 預算 | 24 MiB 大檔同時 | 1 MiB 小檔同時 |
|---|---|---|
| **1 GiB**（預設）| ~42 | ~1000 |
| 256 MiB | ~10 | ~256 |

⚠️ **但要寫明一件事：解析的複本不在預算裡。** ruma 把 body 解成 `create_content::v3::Request`
的 `file: Vec<u8>` 時可能再一份複本 ⇒ 📌 **實際尖峰可能是預算的兩倍左右**。
🚫 不要讓讀者以為 1 GiB 預算就是 1 GiB 封頂。
⭐（要讓它真的只有一倍，得讓 ruma 從已經收好的 bytes **借**而不複製 —— 那是上游型別的事，🚫 不在這支。）

⚠️ 429 的代價是**真實的**（標準 client 會顯示上傳失敗），所以預設寧可寬不要緊 ——
📌 1 GiB 在「幾十人的站」幾乎踩不到，而最壞值仍然是一個說得出來的數字。