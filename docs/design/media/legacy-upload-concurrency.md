# 傳統 Matrix 上傳的記憶體要有封頂：一個全站的 byte 預算

✅ **已實作**（issue #110）。維護者 2026-10-07 定了三件：只做上限（issue #110 的選項 2）、
🚫 不做「關掉傳統上傳」的開關（選項 1）、上限是**記憶體預算而不是固定並行數**（預設 1 GiB）。
設定名 **`matrix_upload_memory_budget`**。
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
⇒ 判準要是「這條路徑會把一個大 body 收進記憶體嗎」。

✅ **實作時的決定：認 Ruma 的請求型別，🚫 不認路徑字串。** `api/router/request.rs` 的
`is_whole_file_upload::<T>()` 比對 `TypeId`，命中兩個端點：

| 型別 | 它服務的路徑 |
|---|---|
| `create_content::v3::Request` | `POST /_matrix/media/r0/upload` **與** `/_matrix/media/v3/upload` |
| `create_content_async::v3::Request` | `PUT /_matrix/media/v3/upload/{server}/{id}`（＋ msc2246 的 unstable 路徑）|

⭐ **這樣「路徑有兩代」這個問題整個消失** —— 一個 Ruma 型別服務它那條路徑的**每一代**，
所以沒有一張要跟著維護的字串清單；而 repo 裡還有**手寫註冊**的路由（`api/router.rs` 的
`.route(...)`），字串判準連那些也要跟。📎 漏一條的代價不是報錯，是**預算靜靜地不生效**。

🚫 **`create_mxc_uri::v1::Request` 不在清單裡**：它的請求是 `Request {}`，沒有 body ——
⭐ 判準是「握著多少位元組」，不是「叫不叫上傳」。

🚨 **已知限制：預約發生在認證之前。** `Args::from_request` 的順序是先 `request::from`
（要讀 body 才能做 UIAA／JSON 認證）再 `auth` ⇒ 一個**沒有 token** 的請求也預約得到位元組。
⇒ 它把「無上限的記憶體」換成「有上限的記憶體 ＋ 上傳被佔住」，**沒有變糟，但也沒有全解**。
📎 真正的界是 `client_receive_timeout`（預設 75 秒）：停著不送的請求最多佔住預算那麼久。
要再收緊就得讓認證不依賴 body，那是另一支。


### 2.4 🚨 預算封的是 `Bytes`，封不住從它長出來的東西

⚠️ **第一版漏了這一格**（審查 2026-10-14，oliver 提、salvia 與 cirno 各自獨立重驗）：
`api/router/args.rs` 在 `request::from` 之後**無條件**把 body 當 JSON 解析一次
（`serde_json::from_slice::<CanonicalJsonValue>`），兩個上傳端點也不例外 ——
🚫 那一行上面的 `json_endpoint` 排除 `/media/` 只決定「要不要回 `M_NOT_JSON`」，**不跳過解析**。

| | |
|---|---|
| 🚨 記憶體 | 一個**形狀是合法 JSON** 的檔（例如 24 MiB 的 `[0,0,0,…]`）解出約一千二百萬個節點、每個約 32 B，再加 `Vec` 擴容 ⇒ 一個請求幾百 MB，而且存進 `Args::json_body`、整個 handler 期間不釋放 |
| 🚨 而且不需要 token | 解析在 `auth` **之前** |
| ⚠️ 連帶的資料損壞（**既有**，不是這支造成的）| `take_body` 看到解析結果是 **Object** 就把 body **重新序列化**成 canonical JSON ⇒ 上傳一個 JSON **物件**檔，存回來的內容空白被剝、key 被重排。📎 **陣列不受影響**（走 `mem::take`），所以「吃記憶體」的範圍比「改資料」大 |

⇒ ⭐ **所以 §5 原本寫的「實際尖峰大約是預算的兩倍」對這種形狀低估了一個數量級** ——
預算擋得住「我們收下多少位元組」，擋不住「我們拿那些位元組又造了什麼」。

✅ **修法：whole-file upload 不做那次解析。** 判準 `is_whole_file_upload::<T>()` 本來就已經算過，
直接拿來用。這條路上跳過是安全的，三件事各自成立：

1. 這兩個端點是 `AccessToken` 認證，而 `auth/dispatch.rs` 的 `AccessToken::dispatch` 參數是
   `_json_body` —— **沒有讀**。
2. 兩個 handler 只用 `body.file`。
3. 會讀 `Args::json_body` 的端點（`upload_signing_keys`、`membership/join`、`register`、
   `session`、`uiaa`）**沒有一個是上傳**。

📎 ⭐ 而同一行順手把上面那個資料損壞也消掉了：`json_body` 是 `None` ⇒ `take_body` 走
`mem::take`，原樣交出 raw body。`[5.8]` 釘住它（上傳一個刻意亂排、帶多餘空白的 JSON 物件，
下載回來逐 byte 相同）。

🔴 **可以帶走的規則**：問「什麼上限它」要問到**衍生物**，不只是入口那個緩衝區。

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

## 4. 測試

✅ **單元**（`service/media/upload_budget.rs` 六條、`api/router/request.rs` 三條）：
預約會扣、滿了拒絕、**丟掉就還回去**、比預算大的那條回的是「不要重試」而不是 429、
宣告 0 不扣、預算設 0 也不會變成「全部拒絕」；加上判準那三條（兩個上傳端點命中、
`create_mxc_uri` 不命中、非上傳端點不命中）。

✅ **e2e**（`tests/e2e/e2e10.ps1` 情境 5，預算設 **2 MiB**）：

| 要釘住的 | 怎麼測 | check |
|---|---|---|
| 設定低於 `max_request_size` 是警告不是拒絕啟動 | 讀啟動 log | `[5.0]` |
| 一般上傳不受影響 | 1 KiB 的檔 | `[5.1]` |
| 🔴 **滿了回 429 而不是吃掉記憶體** | 用 raw socket 送完 header 就停住（宣告 1.5 MiB），再送一個 1 MiB ⇒ **429** ＋ `retry_after_ms` | `[5.2]`／`[5.2b]` |
| 🔴 **不擋別的請求** | 佔住預算時 `/versions`、登入、**wbf 的分塊上傳宣告**照常成功 | `[5.3]` |
| 🚨 **中途斷線也還** | 把那個停住的 socket 關掉 ⇒ 下一個上傳**成功**（📎 這條同時證明「許可會還」，而且走的是比成功更難的那條路）| `[5.4]` |
| 比整個預算還大 ⇒ 🚫 不要重試 | 宣告 3 MiB ⇒ **413**，不是 429 | `[5.5]` |
| 兩代路徑共用一個預算 | 從 `r0` 那個拼法佔住，`v3` 要被拒 | `[5.6]` |
| 放開之後又能用 | 再上傳一次 ⇒ 200 | `[5.7]` |

⚠️ **為什麼 2 MiB 而不是 1**：`max_request_size` 低於 10 MB 會**拒絕啟動**（§3），所以預算只能是
兩者中較小的那個；而預算要大到裝得下一個上傳、又小到裝不下兩個，才測得出「滿了」。
📎 這也順便讓 `[5.0]` 那條警告有東西可驗。


## 5. ✅ 預設 1 GiB（維護者 2026-10-07）

⭐ **byte 預算比固定 N 好的地方就在這裡：最壞值就是那個設定的數字本身，不用乘。**

| 預算 | 24 MiB 大檔同時 | 1 MiB 小檔同時 |
|---|---|---|
| **1 GiB**（預設）| ~42 | ~1000 |
| 256 MiB | ~10 | ~256 |

⚠️ **但「最壞值就是那個數字」只對我們收下的位元組成立，衍生物另外算。** 兩件事要寫明：

1. **ruma 的那一份複本**：把 body 解成 `create_content::v3::Request` 的 `file: Vec<u8>` 時可能再
   一份 ⇒ 📌 **實際尖峰大約是預算的兩倍**。
   ⭐（要真的只有一倍，得讓 ruma 從已經收好的 bytes **借**而不複製 —— 那是上游型別的事，🚫 不在這支。）
2. ✅ **而 JSON 解析那一份已經不存在了**（§2.4）：它本來會讓「形狀是合法 JSON」的上傳長出一棵
   幾百 MB 的樹，比兩倍大一個數量級。⇒ 現在上傳端點不做那次解析。

🚫 不要讓讀者以為 1 GiB 預算就是 1 GiB 封頂。

⚠️ 429 的代價是**真實的**（標準 client 會顯示上傳失敗），所以預設寧可寬不要緊 ——
📌 1 GiB 在「幾十人的站」幾乎踩不到，而最壞值仍然是一個說得出來的數字。