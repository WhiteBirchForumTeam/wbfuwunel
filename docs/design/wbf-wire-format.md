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
4       8     id            會話／物件識別：[id_type 1 byte] ‖ [值 7 byte]，型別表在 §2.2
                            0x00 = 無（整個 id 必須是 0）、0x01 client 的會話號、
                            0x02 事件位置 g_seq、0x03 上傳 id
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
  關不關連線見 §2.1。
- **上限**：`meta_len ≤ wbf_meta_max_bytes`（預設 64 KiB）、`data_len ≤ wbf_data_max_bytes`（預設 **16 MiB**，要放得下大塊，
  [chunked-upload.md](chunked-upload.md) §2.2）。
- 📎 **CRC 不抓竄改**（那是 data 裡的 AEAD 標籤的事），**也不是在替 TLS 補位**。維護者 2026-09-10 問「CRC 對不上的機會有多高」，
  答案要講清楚，否則下一個人會以為它在防網路雜訊：

  | 情境 | CRC 真的會不合嗎 |
  |---|---|
  | TLS 之下的傳輸損壞 | ❌ **實質是零**。TLS 1.2／1.3 是 AEAD，一個位元被改 record 的 tag 就不合，**連線直接斷**，那個 pack 到不了解碼器 |
  | 明文那一跳（前置代理 → server；TLS 由代理保證，server 自己不檢查） | ⚠️ 極低但非零。TCP 的 checksum 只有 16-bit，弱到有量測（Stone & Partridge 2000）估出每 1600 萬～100 億個封包會漏一個 |
  | **編碼端的 bug**（`meta_len` 算錯、offset 少一、buffer 被重用、第三方 client 照規格自己實作寫錯） | ✅ **這才是它每天在抓的東西** |
  | 存下來又讀回去的 pack（向量檔、log 重放、之後的別種傳輸） | ✅ 不在 TLS 的保護範圍裡 |

  ⭐ 所以它是**格式的自我檢查**，不是網路的完整性機制：TLS 保證「你收到的位元組跟對面送出的一樣」，
  但完全不管對面送出的那串位元組**是不是一個合法的 pack**。少了 CRC，一個算錯的 `meta_len` 會變成拿後面的位元組當長度去讀，
  而不是一句 `Corrupt`。
- 📎 **`data_crc` 還有一件只有它能做的事**：E2EE 的塊本身有 AEAD 標籤，但那是**收檔的 client** 驗的，server 解不開也驗不了。
  沒有 `data_crc`，server 只能把壞掉的密文原樣存起來，等幾個月後有人下載才發現。有它，當下就能拒收那一塊。
  成本方面不必擔心：硬體指令大約每核 10–20 GB/s，小 pack 是奈秒級，只有 16 MiB 的大塊會到毫秒級。

### 2.1 連線健康計數器：什麼時候關掉一條講不通的連線

（維護者 2026-09-10 指定，取代原本的「連兩次壞就關連線」——⚠️ 那條**從來沒被實作過**，`ws.rs` 只回一個 `Corrupt` 就繼續讀下一個 frame。
所以這不是改行為，是第一次把行為定下來。）

每條 WS 連線一個計數器，純記憶體、起始 **0**：

| 收到什麼 | 計數器 |
|---|---|
| **解不開的框**：CRC 不合、`version` 不對、保留旗標非 0、長度對不上、文字 frame（即 `Corrupt` 與 `UnsupportedVersion`） | **−1** |
| **解得開的 pack** | **歸零** |
| ⚠️ **`decode` 拒掉、但框本身是好的**：kind 位未分配（`UnknownKind`）、區段長度超上限（`TooLarge`） | **歸零**（跟解得開的 pack 同一邊） |

`counter ≤ −wbf_ws_corrupt_budget`（預設 **8**）→ 送完最後那個 `Error` 之後 Close **1002**（protocol error）關線。

- ⭐ **只有「框」的錯扣分。** pack 解得開就歸零 —— 即使 handler 之後回 `InvalidRequest`、`Unauthorized`、`NotFound`：
  那些代表對方**看得懂這個協議**，只是這一個請求不對。計數器量的是「你會不會講這個協議」，不是「你有沒有做錯事」。
- ⚠️ **「被 `decode` 拒掉」不等於「框壞了」**：`decode` 是先讀 kind 位再驗 CRC（`pack.rs`），
  所以一個**結構完好、CRC 正確、只是 kind 未分配**的 pack 也會從 `decode` 出來帶著錯。它的發信方**會講 wbf**，
  扣它的分等於把一個正常的 client 送了八個新 kind 就關掉。⭐ **實作上一律拿 `RejectCode::for_pack_error(&error).is_undecodable_frame()` 當閘門**，
  不要在計數點再寫一份「哪些算框錯」的列表（審查者 cirno／ rumia／salvia 2026-09-10：PR #38 的第一版就是漏了這道閘門）。
- 📎 **為什麼是 8 而不是 2**：連續 8 個解不開的框，實務上只有兩種可能 —— **對面根本不是在講 wbf**（協議錯、當成別的東西連進來），
  或者**某一端的編碼有 bug**。正常的 client 認得協議，除非自己壞掉，否則一輩子碰不到這個門檻。
  2 太嚴苛：一個偶發的壞框（見上面那張表的明文那一跳）就把一條好好的連線踢掉，而重連付的代價比那個壞框大得多。
- 📎 **為什麼「好的就歸零」而不是慢慢回血**：這個計數器管的是**協議層的認不認得**，不是流量。
  有人用「7 個壞、1 個好」的節奏繞過它，換到的也只是「每個壞框換一個 `Error` 回應」—— 那是**限速**該管的事（§6.3.4），不是這裡。
  🚫 不要把兩件事塞進同一個計數器：一個變數同時管兩種閾值，兩邊都會調不準。
- **HTTP 路徑沒有這個計數器**：一個請求一個 pack、沒有連線可關，壞框就回一個 `Error`（§6.2）。
- ✅ **實作到位**：`ws.rs` 的 `FrameHealth`（計的是「連續幾個解不開的框」，跟上面的負數計法等價：連續 8 次 ⇔ 計數器到 −8），
  旋鈕是 `wbf_ws_corrupt_budget`。測試：「8 個壞框關線」、「中間夾一個好 pack 就重新算」、「預算 0 也至少給一個框」。

### 2.2 `id` 的第一個 byte 是型別（維護者 2026-09-12）[BREAKING]

> **狀態**：📄 提案，等維護者同意。這一節同意之後才動程式。

```
offset=4  size=8   id  ＝ [id_type: 1 byte] ‖ [值: 7 byte 大端]
```

⭐ **一個 id 自己就說得出它是什麼。** 在這之前，同一個欄位裝著三種來源完全不同的東西 —— client 自己挑的會話號、server 隨機鑄的上傳 id、資料庫給的事件位置 `g_seq` —— 要**先看 `kind`** 才分得出來。那代表兩件事：

- client 若拿一張以 `id` 為鍵的表記自己的會話，兩種不同的東西會**悄悄共用一格**；
- server 收到一個對不上的 id 只能回「找不到」，說不出「你把上傳 id 填到訂閱那一格了」。

型別 byte 讓這兩件事都變成**讀得出來的錯**。

| `id_type` | 誰鑄 | 值是什麼 | 用在 |
|---|---|---|---|
| `0x00` | — | **整個 id 必須是 0** | 沒有會話的包：`Control/Hello`、`Ping`、`Session/*`、`Download/*`（用 meta 的 mxc 定位）、`Upload/Create`、`Stream/Draft`（還沒有錨） |
| `0x01` | **client** | 它自己挑的會話號 | `Event/Recent`、`Event/Subscribe`／`Unsubscribe`、`Device/Subscribe`／`Unsubscribe`／`Fetch`／`ItemsDestroy` |
| `0x02` | **server**（資料庫） | 事件位置 `g_seq` | `Stream/Abandon`／`Keypoint`／`Delta`／`Append`／`Demand`（草稿的錨） |
| `0x03` | **server** | 上傳 id。⚠️ **去掉型別 byte 就是 mxc 的 media id** | `Upload/Chunk`／`Status`／`Seal`／`Abort` |
| `0x04`–`0xEF` | — | 未分配 | 🚨 照 §3.4 的同一條鐵律：**先在這張表加一列，才能發** |
| `0xF0`–`0xFE` | — | 保留 | |
| `0xFF` | — | **延伸**：真正的型別寫在 meta 裡 | 逃生門。之後真的出現一種這張表沒想到的 id，用它，而不必動 frame |

**規則**

1. 🚨 **server 驗型別與 `(kind, subtype)` 對不對得上**，不符 → `InvalidRequest`。不驗的話型別只是裝飾。
2. **需要會話的包填 `0`** → `InvalidRequest`；**不需要會話的包填了非 0** → `InvalidRequest`。
   📎 後者今天就已經是事實（`Hello`、`Download/*`、`Upload/Create` 一律填 0，向量檔可證），所以這條是把既有慣例寫成規則，不是新負擔。
3. **值只剩 56 bit。** `g_seq` 與上傳 id 都遠在這之下（2⁵⁶ ≈ 7.2 × 10¹⁶）；真的超過就**拒絕**（fail closed），🚫 不截斷。
4. **型別不重用、號碼不回收** —— 跟 `code_id` 同一條（§3.4）。
5. ⭐ **鑄了 id 的 `Ack` 要把組好的 id 回給 client**（`Upload/Create` 的 `{"id": …}`、`Stream/Draft` 的 `{"id": …}` ＋ 既有的 `g_seq`）。
   不然「把型別併進值裡」這個位元運算，每個 client、每種語言都要自己寫對一次。

⚠️ **server 保證的與不保證的，寫清楚**（維護者 2026-09-12 定 A 方案）：`0x01` 是 client 自己鑄的，
所以 server 驗的是**型別對不對**，🚫 **不驗**「同一條連線內有沒有重複」—— 它看不到 client 的表。
⭐ 因此**跨型別**的碰撞（上傳 id 撞到某個 `g_seq`）由協議擋住；**同型別內**的重複是**發的人自己的責任**：
誰發的誰負責不撞。這條要明寫，免得有人以為 server 保證了它其實沒檢查的事。

**mxc 怎麼辦**：media id 是**拔掉型別之後的 7 byte**，十六進位 **14 個字元**（`{值:014x}`）。
Matrix 對 media id 只要求 1–255 個 `[A-Za-z0-9_-]`，所以**不需要 padding**。
📎 既有媒體的 16 字元 media id 不受影響（它們是字串，不會跟 14 字元的新 id 相撞），而「上傳 id 的十六進位**就是** media id、兩者之間沒有對照表」這個性質仍然成立 —— 只是換成低 7 byte。
⚠️ 上傳 id 的隨機位元從 64 降到 56。那裡本來就有「撞到就重抽」的迴圈（`is_upload_id_free`），所以影響只是極罕見地多抽一次。

**為什麼型別佔滿一個 byte**（討論過 4 bit 與 2 bit）：

- 剩下的值要維持**整數個 byte** —— mxc 才能直接印成 14 個字元。4 bit 會剩 60 bit＝7.5 byte，media id 變成 15 個字元、要定一條補位規則，⭐ **而那正是會被下一個實作者漏掉的那種規則**。
- 讀型別是**一次固定偏移的 byte 讀取**，不是位移＋遮罩；這段每個 client 都要重寫一次。
- 2 bit 只有 4 個值，**今天就用光**（無、client、`g_seq`、upload），下次要加就又是一次 breaking。
- 🚫 **不在這個 byte 裡放版本號**：pack 的第 0 個 byte 已經是版本，兩套版本機制之後會有「誰說了算」的問題；
  而且 `header_id_seq()` 必須在 **CRC 還沒驗過**時用固定偏移把 `id` 挖出來回 `Error`，佈局隨版本而變會讓那件事變成「要先知道版本才讀得懂」。

**這支要跟的東西**（都是 [BREAKING]）：`wbf-vectors.json` 重出、全部 e2e 腳本、`chunked-upload-spec.md` 的 id 敘述，以及 client repo 的協議同步 issue。

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
| | `0x03 Error` | `{ "code_id": <序號>, "code": "…", "message": "…" }` ＋ 該 code 定義的欄位；程式比對 `code_id`，`code` 是它的名字；**完整清單在 §3.4**，那張表是唯一的來源 | 無 |
| | `0x04 Ping` / `0x05 Pong` | `{ "nonce": … }` | 無 |
| `0x02 Stream` | `Open` `Fragment` `Close` `Abandon` | [streaming-messages.md](streaming-messages.md) §4 | 密文本體 |
| `0x03 Upload` | `Create` `Chunk` `Status` `Seal` `Abort` | [chunked-upload.md](chunked-upload.md) §4 | 塊 bytes（`Chunk`） |
| `0x04 Download` | `Info` `Read` | [chunked-upload.md](chunked-upload.md) §5 | 回應的 data 是讀出的 bytes |
| `0x10 Session`（§6.3） | `0x01 Login` | Matrix `/login` 的請求體原樣：`{ "type": "m.login.password" \| "m.login.token", "identifier", "password" \| "token", "device_id"?, "initial_device_display_name"?, "refresh_token"?: bool }`；回應 `{ "user_id", "device_id", "access_token", "refresh_token"?, "expires_in_ms"? }` | 無 |
| | `0x02 Refresh` | `{ "refresh_token" }`；回應同 `Login` | 無 |
| | `0x03 Logout` | `{ "all"?: bool }`；回應 `{}`，緊接 server 送 Close 1000 關線 | 無 |
| `0x14 Event` | `0x01 Recent` | `{ "limit": 320?, "cg_seq": <g_seq>?, "before": <g_seq>?, "batch": 10? }`，**`id` 由 client 選**（回應抄它）；回應是一串 `0x03 Batch`，不是 `Ack`；**只走 WS**，HTTP 回 `Error(Unsupported)` | 無 |
| `0x14 Event` | `0x03 Batch`（只有 server → client） | `{ "tc", "bc", "fs", "ls", "r" }`：這一窗總則數、這批則數、這批最新／最舊的 g_seq、這批之後還剩幾則；`r = 0` 就是這窗結束。`id` 抄 `Recent`，`seq` 從 0 嚴格 +1 | `bc` 則事件，每則 u32 大端長度 ＋ 事件 JSON（含 `room_id`；`unsigned` 帶 `org.wbftw.wbfuwunel.r_seq` 與 `…g_seq`），新到舊，見 [room-seq-and-recent.md](room-seq-and-recent.md) §2、[wbf-pack-pipeline.md](wbf-pack-pipeline.md) §6 |
| `0x14 Event` | `0x04 Subscribe` | `{ "rooms"?: ["!…"], "cg_seq"?: <g_seq> }`，**`id` 由 client 選**（之後每個 `Push` 抄它）；沒帶 `rooms` = 帳號層（所有加入的房，含之後加入的）；回應 `{ "latest_g_seq", "joined", "skipped": […] }`；**只走 WS** | 無 |
| | `0x05 Unsubscribe` | `{ "rooms"?: ["!…"] }`；沒帶 = 全退；回應 `{}`；退不存在的是 no-op。**退訂退的是當下的 channel，不是黑名單**：帳號層訂閱者（`Subscribe` 沒帶 `rooms`）點名退掉某房之後，**再加入那個房時仍會被自動加回來** | 無 |
| | `0x06 Push`（只有 server → client） | `{ "bc", "fs", "ls", "gap": bool }`；`id` 抄 `Subscribe`，`seq` 每推一次 +1；`gap: true` = 前面有推送被丟，用 `Recent` 補；事件驅動類，不 Ack、不重送，見 [wbf-event-push.md](wbf-event-push.md) | `bc` 則事件，跟 `Batch` 同一個長度前綴切法 |
| `0x14 Event` | `0x02 Send` | `{ "room_id", "type", "txn_id", "attachments": [mxc…] }`；回應 `{ "event_id" }` | 事件 content 的 JSON（E2EE 就是 `m.room.encrypted` 的 content）。`attachments` 是 server 讀不到密文時唯一的引用來源，見 [media-attachments.md](media-attachments.md) |
| `0x16 Device` | `0x04 Subscribe` | `{ "device_id", "cd_seq"? }`，**`id` 由 client 選**（之後每個 `Push` 抄它）；`device_id` 必須是這條連線 session 的那個，否則 `Error(Forbidden)`；回應 `{ "latest_cd_seq" }`；**只走 WS**。⚠️ 一個裝置同時只有一條連線在收，而且**後來的接手** —— 被接手的那條收到 `Error(Superseded)`（1505），見 [wbf-to-device.md](wbf-to-device.md) §4 | 無 |
| | `0x05 Unsubscribe` | `{}`；回應 `{}`；沒訂也是 no-op。退訂**同時解除裝置綁定**，下一條連線不必靠搶佔就拿得到 | 無 |
| | `0x01 Fetch` | `{ "cd_seq": <count>, "limit": 1000? }`，**`id` 由 client 選**；回應是一串 `0x02 Batch`，不是 `Ack`；**只走 WS**。📎 `limit: 0` 就是「不要」，回一則空 `Batch` | 無 |
| | `0x02 Batch`（只有 server → client） | `{ "tc", "bc", "ot", "nt", "counts": [u64…], "r" }`：這一窗總則數、這批則數、這批**最舊／最新**的 count、逐則的 count、之後還剩幾則；`r = 0` 就是這窗結束。⚠️ 名字是 `ot`／`nt` 不是 `fs`／`ls`，因為**順序相反**：to-device 是**舊到新**（client 照這個順序匯入再銷毀） | `bc` 則項目，跟 `Event/Batch` 同一個長度前綴切法 |
| | `0x06 Push`（只有 server → client） | `{ "bc", "ot", "nt", "counts": [u64…], "gap": bool }`；`id` 抄 `Subscribe`，`seq` 每推一次 +1；`gap: true` = 前面有推送被丟，用 `Fetch` 補 | 同上 |
| | `0x03 ItemsDestroy` | `{ "tc" }`；**data 是 `tc` 個 u64 大端的 count**（不是 JSON、不是前綴，就是 `tc × 8` byte）；`tc` 與 data 長度對不上 → `Error(InvalidRequest)`，**一則都不刪**；只有持有這個佇列的連線能發，否則 `Error(Forbidden)`。回應**先** `Control/Ack`（只表示收到命令），**再**一則 `0x07 ItemsDestroyed` | `tc × 8` byte |
| | `0x07 ItemsDestroyed`（只有 server → client） | `{ "tc", "bc" }`：命令裡有幾則、**真的沒了**幾則；data 是那 `bc` 個 count。⚠️ 一則都沒刪也會回（空清單），否則「沒刪掉」跟「server 沒回應」在 client 眼裡一樣。已經不在的**算銷毀成功** —— client 要的是狀態，不是事件 | `bc × 8` byte |
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
| `0x10` | Session | login、logout、refresh、register（`session/`、`register/`）；§6.3 已佔 `0x01 Login`、`0x02 Refresh`、`0x03 Logout` |
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

### 3.4 錯誤詞表（`Control/Error` 的 `code`）

`code` 是**給程式讀的**：client 照它決定重試、重登、還是放棄。`message` 是給人讀的，措辭隨時會變，
🚫 **client 不要 parse `message`**。這張表是唯一的來源。

✅ **實作到位（2026-09-10）**：`RejectCode`（`src/core/wbf/error_code.rs`）是這張表在程式裡的投影 ——
`Reject::code` 只收它的變體，呼叫點**沒辦法**再現編一個字串；每個 `Error` 的 meta 帶 `code_id` 與 `code`。
⭐ 那個列舉就是這張表的閘門 —— 沒有它的那段日子，`Conflict` 就長到了十一個呼叫點。

每個 code 有一個**序號**（`code_id`）和一個**名字**（`code`），兩個都在線上，一對一，**都不重用**（維護者 2026-09-10 定）。
序號是給程式比對的（不必比字串、看範圍就知道是哪一家），名字是給人看 log 與向量檔的。

| `code_id` | code | 什麼意思 | 誰產生 | 額外欄位 | client 該怎麼辦 |
|---|---|---|---|---|---|
| 1001 | `UnsupportedVersion` | pack 的版本位不是這個 server 支援的 | pack 解碼 | | 升級 client；重試沒有用（這個跟 `Corrupt` 一樣扣 §2.1 的計數器） |
| 1002 | `Corrupt` | **這個 pack 解不開**：CRC 對不上、被截斷、保留旗標有值、送的是文字 frame | pack 解碼 | | 重連**一次**就好；再來一次就是編碼端的 bug（實務上多半是，見 §2 的 📎），往上報，🚫 不要一直重連 |
| 1101 | `UnknownKind` | 這個 `(kind, subtype)` 沒有 handler | pack 解碼（kind 位未分配）或准入表（subtype 沒 handler） | | 這個 server 不會做這件事；別重試。📎 兩條路都**不**扣 §2.1 的計數器 —— 框是好的 |
| 1102 | `Unsupported` | 有 handler，但**不走這個傳輸**（例：`Recent`／`Subscribe`／`Session` 只走 WS） | 准入表 | | 換傳輸（開 WS），不是重試 |
| 1103 | `TooLarge` | 超過 `wbf_meta_max_bytes`／`wbf_data_max_bytes`，或上傳宣告的大小上限 | 准入表、上傳 | | 切小再送 |
| 1201 | `InvalidRequest` | pack 解得開，但 **meta／data 不是這個 subtype 要的**：JSON 壞、型別錯、缺欄位、值超出範圍、mxc 解不出來 | 各 handler | | client 的 bug；照 `message` 修，重送同樣的東西一定再錯 |
| 1301 | `Unauthorized` | 沒登入，或身分**不再**有效（token 到期、被撤、帳號被鎖） | session 閘門 | `soft_logout`?（Refresh） | 重新登入；WS 通常隨即被關（Close 1008） |
| 1302 | `Forbidden` | 身分驗過了但**不准**：憑證錯、帳號停用、不支援的登入 `type` | 登入 | | 不要自動重試；`message` 帶 Matrix 的 errcode |
| 1401 | `RateLimited` | 太快了 | 限速閘門 | `retry_after_ms` | 等那麼久再試，🚫 不要立刻重打 |
| 1402 | `TooManyConnections` | 這個 device 的 WS 名額滿了（`wbf_ws_max_connections_per_device`） | `admit` 閘門 | `max_connections` | 關掉一條舊的再連；這條連線隨即被關 |
| 1501 | `NotFound` | 指名的東西不存在（上傳 id、媒體） | 各 handler | | 重建那個東西，或放棄 |
| 1502 | `Conflict` | 請求**合法**，但跟 server 目前的狀態衝突（例：這個上傳已經封存／已經完成） | 上傳等有狀態的 kind | | 先讀狀態（`Upload/Status`）再決定；重送同一個請求還是會衝突 |
| 1503 | `OutOfOrder` | 有序類的 `seq` 不是接收端等的那個（§4） | 有序類 | `expected_seq` | 從 `expected_seq` 重送，🚫 不要自己重排 |
| 1504 | `Truncated` | 上傳觸到大小上限，被封成不完整（可以 Seal） | 上傳 | `received`、`total_len`、`finished`、`truncated` | 照那幾個數字決定 Seal 還是 Abort |
| 1505 | `Superseded` | **這個訂閱被同一裝置後來的連線接手了**，它到此為止（`0x16 Device`，見 [wbf-to-device.md](wbf-to-device.md) §4） | 獨佔型訂閱的註冊表 | | 這條連線的那個訂閱已經沒了，別再等它的 `Push`；要收就重訂（但那會把現在那條踢掉）。⚠️ 連線本身沒關，它的其他訂閱照常 |
| 1901 | `Internal` | server 自己的錯 | 任何地方 | | 可以退避重試；連續發生就是 server 的 bug |

**序號怎麼編**（新增的照這個範圍放，範圍本身就說了是哪一家）：

| 範圍 | 家族 | 共同點 |
|---|---|---|
| 1000–1099 | pack 層 | 框壞了，還沒進到「這是什麼請求」 |
| 1100–1199 | 路由與准入 | 框好的，但這裡不受理 |
| 1200–1299 | 請求內容 | 受理了，但內容不對 |
| 1300–1399 | 身分與權限 | 你是誰、准不准 |
| 1400–1499 | 節流與名額 | 現在不行，等一下或讓出資源 |
| 1500–1599 | 狀態 | 請求對，但跟 server 現在的狀態對不上 |
| 1900–1999 | server 自己的錯 | 不是 client 的問題 |

- 🚫 **`0` 永遠不是合法的 code_id。** 序號從 1000 起就是為了這個：欄位漏了、反序列化拿到預設值的 `0`，
  必須是「不認得」而不是某個真的碼 —— 佔位值要一眼看得出是佔位值。
- 🚫 **序號與名字都不重用。** 一個 code 退役就留在表上標 `retired`，號碼不給下一個用；
  否則舊 client 會拿新語意去套舊行為，而它不會知道。

⚠️ **`Error` 幾乎都是某個請求的回應，`Superseded`(1505) 是唯一的例外**：它是 server **主動**送的，
`id` 是**那條連線自己當初 `Subscribe` 用的 id**、`seq` 接在那段會話的後面、帶 `IS_LAST` —— 也就是
「你那段會話到此結束」，不是「你剛剛那個請求失敗了」。client 因此不必為它準備第二套解析：照原本的
`id` 對回自己的訂閱就好。⭐ 會這樣設計是因為**靜默失效更糟** —— 被接手的那條若什麼都沒收到，
它會一直以為自己還在收金鑰。

🚨 **code 一律事先定義**（維護者 2026-09-10 指定）：要發一個新的錯誤，順序是**先在這張表加一列**（序號、名字、意思、client 該怎麼辦），
再改程式。🚫 呼叫點不准現編一個碼 —— 那正是 `Conflict` 長到 11 個呼叫點、`Corrupt` 被拿去講「JSON 格式錯」的原因：
沒有人一次看過全部的碼，也就沒有人發現它們在說謊。⭐ 程式那邊的列舉是這條規則的閘門：加不了新變體，就編不出新碼。

🚨 **client 收到不認得的 code：不認得就不認得**（維護者 2026-09-10 指定）——
當成「這個請求失敗了，而且**不知道**能不能重試」：不重試、往上報，把 `code_id`、`code` 與 `message` 原樣留在 log 裡。

- 🚫 不要落到 `Internal` 那條「退避重試」的路徑 —— 猜錯的方向是對 server 無限重打。
- 🚫 不要**只**看序號範圍就自己補一套行為。範圍是給人除錯與分類看的，**不是**「同一家就照同一套處理」的授權；
  真的要照家族處置，那個處置必須先寫進這張表，那它就不是「不認得的 code」了。
- ⭐ 因此**加新 code 永遠是相容的**：舊 client 會安全地不懂它，而不是誤解它。這也是 `message` 存在的理由 ——
  不認得 code 的時候它是唯一能給人看的東西，所以 message 要像句人話；但仍然 🚫 不給程式 parse。

⭐ **兩組最容易混的，分界寫在這裡**：

- `Corrupt` vs `InvalidRequest` —— **框壞了**（解不開這個 pack）對上**內容不對**（pack 好好的，裡面的 JSON 不是這個操作要的）。
  前者 client 沒辦法自己修，後者是 client 的 bug。
- `InvalidRequest` vs `Conflict` —— **請求本身錯**對上**請求對、但現在不行**。判準：把 server 的狀態換一個，這個請求會不會變成合法的？
  會 → `Conflict`；不會（怎麼樣都錯）→ `InvalidRequest`。

📎 **`InvalidRequest` 是後來補的**（維護者 2026-09-10 定）：在那之前 meta 解析失敗回的是 `Conflict`（少數幾處回 `Corrupt`），
兩個都在說謊 —— 一個把 client 的格式錯講成狀態衝突，一個把它講成傳輸壞掉，而 client 對這三種的處置**完全不同**。
👉 這是 wbf 通道上**看得見的改動**，client 端要跟（見 wbf-matrix-client 的協議同步）。

**歸位過的呼叫點**（✅ 都已改，留著這張表是為了下一個問「這條為什麼是這個碼」的人）：

| 本來 | 現在 | 哪裡 |
|---|---|---|
| `Conflict` | `InvalidRequest` | `parse_meta`（`Subscribe`／`Unsubscribe` 等所有走它的）、`Event/Send` 的 meta 與 data、`Recent` 的 `cg_seq`／`before` 型別與範圍、`Upload/Create` 的 `EncryptedFileInfo` 解碼、`invalid mxc`（兩處）、`chunk index too large`、`position too large`、`mxc is required` |
| `Corrupt` | `InvalidRequest` | `Session` 的 `Login`／`Refresh`／`Logout` meta 形狀不對（`session.rs` 三處）——**pack 沒壞，是內容不對** |
| `Conflict` | `InvalidRequest` | `From<Error>` 裡 `StatusCode::BAD_REQUEST` 的那一條 |
| `Conflict` | 不動 | `UploadError::Conflict`（上傳狀態衝突）——這是這個 code 收窄後**唯一**該留的用法 |
| `Corrupt` | 不動 | pack 解碼失敗、文字 frame（`ws.rs`）——框壞了 |

✅ **測試**：`error_code.rs` 有「序號與名字一對一、沒有重複」與「沒有一個序號小到會跟預設值撞」兩條表格測試；
`mod.rs` 有「解不開的 pack 是 `Corrupt`、格式錯的請求是 `InvalidRequest`」的分界斷言；e2e8／e2e9 各有一條線上實測。
⚠️ **不是每個 code 都有專屬的情境斷言**（`UnsupportedVersion`、`Truncated`、`Internal` 沒有）——
這裡本來寫著「每個 code 至少一條」，那句話當時就寫得比做得到的滿；缺口留著，不要把它讀成已經蓋滿。

✅ **向量檔已整批重生**：`error_rate_limited`／`error_out_of_order`／`error_unsupported`／`error_too_many_connections`
的 meta 都多了 `code_id`，並新增 `error_invalid_request`。
👉 這就是 client 端**必須同步**的那一刻：舊 client 讀新向量會對不上，而它讀不到 `code_id` 時的行為就是上面那條「不認得就不認得」。

## 4. 順序守則：依 kind 分兩類

WebSocket 本身保證到達順序，所以**順序錯一定是邏輯錯誤**，不是網路問題；發現就回 `Error(OutOfOrder)` 帶 `expected_seq`，
讓發送端從那裡重送，不要猜、不要自己重排。

| 類別 | 哪些 | `seq` 的意思 | 規則 |
|---|---|---|---|
| **有序（ordered）** | `Upload/Chunk`、`Stream/Fragment`、任何要 Ack 才能往下走的 | 同一個 `id` 之內嚴格遞增（+1） | 接收端記每個 `id` 的 `next_seq`；來的不是 `next_seq` → `Error(OutOfOrder)`，丟掉那一框，`next_seq` 不動。發送端可以連續送不等 Ack（滑動窗口），但送出的順序必須是遞增的 |
| **無序（unordered）** | `Download/Info`、`Download/Read`、`Upload/Status`、之後的看房間、看訊息 | 請求號，只用來把回應對回請求 | 可以亂序回；發送端用 `(kind, seq)` 對表。同一連線內 `seq` 由發送端自己保證不重複（單調遞增就好） |
| **事件驅動** | server 主動推的東西（別人的 `Stream/Fragment` 轉發、之後的通知） | 發送端的序號 | 接收端不守順序，收到就處理；掉了就掉了（§5 流式的送達語意） |

`Upload/Create`、`Seal`、`Abort`、`Stream/Open`、`Close` 這些**一次性指令**歸無序類（一個請求一個回應）。

### 4.1 `seq` 屬於**會話**，不屬於 kind、也不屬於連線

⭐ **`id` 是會話的名字，`seq` 是它裡面的計數**（PR #42 定案，實作在 `service/streams/subscribers.rs`）。
會話由開啟它的那個指令命名 —— `Subscribe`、`Fetch`、`Recent`、`Upload/Create` —— 之後屬於它的每一包都抄那個 `id`，
`seq` 只在**那段會話內**遞增。

| 🚫 不是這樣 | 為什麼不行 |
|---|---|
| 每個 kind 一個計數 | 同一條連線上同 kind 的兩段會話會共用一個序列（e2e7 就有兩個上傳交錯），兩邊都看不出哪一包是自己的 |
| 每條連線一個計數 | 一條連線同時背房間推送與 to-device 推送（這是**正常形狀**，不是特例），互相看起來就像對方漏了號 |

所以：**一條連線上可以同時有好幾段會話在跑**，各自數各自的。
📎 `Subscribe` 帶**不同的 `id`** 重訂就是**開一段新會話**：`seq` 歸零、`gap` 清掉。
⚠️ 但**它已經進了哪些 topic 不會因此重來**（退訂是 `Unsubscribe` 的事）——「新的 id」講的是編號，不是成員資格。

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

**server 端**（實作見 [wbf-pack-pipeline.md](wbf-pack-pipeline.md)，2026-09-07 起）：一條 WebSocket 連線 = 一個接收 loop ＋ 一個發送 task（有界 `mpsc`，`wbf_ws_send_queue_len`）；
**沒有**每連線的順序狀態表（有序類由上傳服務從 DB 判，§6.1）。handler 的契約是 `(services, ctx, view, reply) -> Result<SessionChange, Failure>`，
回應全部經 `Reply` 進發送佇列，可以送 0..n 個 pack（`Recent` 的 Batch 串流就是 n 個）；HTTP 的 `POST /_wbf/v1/pack` 呼叫同一條派發，`Reply` 在 HTTP 上只裝得下一個 pack，
串流型的 kind 由准入表擋在 HTTP 之外（`Error(Unsupported)`）。有序類在 HTTP 上仍要 `seq` 正確 —— server 從 DB 讀 `next_seq`。

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
- `Login`／`Refresh`／`Logout` subtype（§6.3）建在同一個 `Session` 上：Login 就是換掉這條連線的 Session。
- **每個 (user, device) 最多 `wbf_ws_max_connections_per_device` 條 WS**（預設 4；[pack-pipeline](wbf-pack-pipeline.md) §2.1，維護者 2026-09-07 定）：帶 Bearer 升級超過 → **不升級**，429 ＋ `Error(TooManyConnections)`；
  匿名連線不算，`Login`／`Refresh` 拿到身份那刻才算，超過 → `Error(TooManyConnections)` 然後 Close 1008，**而且不發任何 token**（名額在 users service 寫 token 之前的 `admit` 閘門檢查，被拒的登入不會換掉該裝置其他連線的 token）；
  同一連線同一裝置再登入不多佔一格；連線結束名額就回來（RAII，`Services.connections` 的 `ConnectionSlot`）。HTTP 不算。踢的永遠是新的那條。

### 6.2 HTTP（選用，測試與腳本用）

`POST /_wbf/v1/pack`，`Content-Type: application/octet-stream`，**body 是一個 pack，回應 body 也是一個 pack**。
一個請求一個 pack；`id` 由 server 在 `Upload/Create` 的回應裡發，之後帶著它。它存在的理由是 curl 就能測；效能不是它的目標。
`Stream` kind 走 HTTP 沒意義（沒人連著收），回 `Error(Unsupported)`。`Session` kind（§6.3）也不走 HTTP pack：它的語意是「換這條連線的 Session」，HTTP 沒有連線可換，回 `Error(Unsupported)`；HTTP 登入照舊用 `/login`。
（📎 這兩處本來寫的是 `Conflict`，但准入表從 pack-pipeline 那支開始回的就是 `Unsupported` —— 文件停在舊字，程式沒錯。§3.4 的分界：不走這個傳輸是 `Unsupported`。）

### 6.3 `Login`／`Refresh`／`Logout` —— 在通道上取得與放掉 session

> 狀態：✅ 已實作，PR #30 2026-09-07 合併（提案 #29，§6.3.9 的點維護者都定了）。起因：維護者 2026-09-06 提出「登入應該有 WS 專用的 pack 格式；升級帶 Bearer 可以留著」。
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
| `0x02 Refresh` | `{ "refresh_token" }` | 同 Login | `Error(Unauthorized)`（到期、重放、帳號被鎖；帶 Matrix 的 `soft_logout` 旗標）、`Error(Forbidden)`（格式錯或不認得）、`Error(RateLimited)` |
| `0x03 Logout` | `{ "all"?: bool }`（`all` = 撤這個 user 的全部 device，對應 `/logout/all`） | `{}`，緊接 Close `1000` 關線 | `Error(Unauthorized)`（這條連線本來就沒登入） |

**不自己發明欄位**：meta 直接餵給既有的 login／refresh／logout 邏輯，client 不用學第二套；server 端要做的是把 `login_route` 的本體（驗證 → 發 token → 建或更新 device）
抽到 `tuwunel_service`（例：`users::login::issue_session`），HTTP 路由與 WS handler 都叫它。🚫 不在 `api/client/wbf/` 裡複製一份登入流程。

**Login 的 `type` 只收 `m.login.password` 與 `m.login.token`**（後者是 SSO 流程最後一步的一次性 token）。SSO／OIDC 本身要瀏覽器重導，走 HTTP；
`m.login.application_service` 與 JWT 也留在 HTTP（appservice 不會用 WS 登入，JWT 之後有需要再加）。收到別的 `type` 回 `Error(Forbidden)`，message 說明。

#### 6.3.3 連線的狀態機（維護者 2026-09-07 定）

```
                 Login ok                          Logout ok
 未登入 ─────────────────▶ 已登入(Session) ─────────────────▶ Ack，然後 Close 1000（關線）
   │  Hello/Ping/Login/Refresh 以外 → Error(Unauthorized)     │  再 Login → 換 Session（允許）
   │  升級後 30 秒內沒登入 → Close 1008（Ping 不延長）
   ▼
 Login 失敗 → Error(Forbidden)，連線留著（受 §6.3.4 限速與那 30 秒）
 Login 時該裝置的 WS 名額已滿 → Error(TooManyConnections)，然後 Close 1008（§6.1；沒發 token）
```

- **允許不帶 Bearer 升級**（這是提案裡唯一改升級行為的地方）：Session 是「未登入」，只接受 `Hello`、`Ping`、`Login`、`Refresh`；其他 kind 回 `Error(Unauthorized)`。
  帶 Bearer 升級照 §6.1 不變，直接是已登入。兩條路都在，client 自己選。
- **未登入的連線最多活 30 秒**：新 config `wbf_ws_unauthenticated_timeout`（預設 30 秒），**從升級起算，不是 idle**：送 Ping 不延長。到時還沒登入送 Close 1008。
  理由（維護者）：連上但沒授權的連線不能佔著什麼都不做；帳密本來就是先打好才開連線送的，30 秒夠（註冊也一樣）；斷了 client 重連就好，自動重連是 client 的事。
  已登入的連線照舊用 `wbf_ws_idle_timeout`（300 秒）。
- **Login 成功後這條連線的 Session 換成新的**，之後每個 message 的重驗用新 token。同一條連線再送 Login：允許，換成另一個 session（舊 token 不撤，
  那是 Logout 的事）。切帳號用這條，不需要先 Logout。
- **Refresh 成功後 Session 的 token 換新**，user／device 不變；舊 access token 照 Matrix 語意失效。
- **Logout 成功後 server 送 Ack、再送 Close 1000 關線**（維護者定：換帳號重開一條 WS 就好）。`all: true` 撤全部 device，
  同一個 user 的**其他** WS 連線在下一個 message 的重驗就被關（§6.1 的機制，不用另外通知）。
- **重驗照舊每個 message 一次**；未登入狀態沒有 token，跳過重驗（沒東西可驗），只做 kind 白名單。
- **server 的責任只有即時回應**。client 的登入重試（維護者建議：3 秒沒回應重送、連續 3 次算伺服器無回應）是 client 的事，這裡不規定。

#### 6.3.4 限速（這條是必做，不是加分）

HTTP `/login` 現在**沒有**限速（只有 OIDC 端點有 `oidc_rc_per_second`／`oidc_rc_burst_count`）。開了 WS Login 等於多一個入口，而 WS 的每個 message 比一個 HTTP 請求便宜，
不限速就是給暴力破解開快車道。提案：

- 新 config `login_rc_per_second`／`login_rc_burst_count`，**同一個 token bucket 同時管 HTTP `/login`、`/refresh` 與 WS `Login`／`Refresh`**，key 是 client IP
  （WS 在升級時取 `ClientIp`，存進連線狀態）。形狀抄 OIDC 那組（既有實作可以直接用）。
- 預設值（維護者 2026-09-07 同意）：`login_rc_per_second = 1`、`login_rc_burst_count = 10`，**預設開**。OIDC 那組預設 0（關）是「保留開放存取」；
  登入不同：一個人手打密碼打不到這個速度，NAT 後面十個人同時登入也剛好夠。
- 超過回 `Error(RateLimited)`，meta 多 `retry_after_ms`；HTTP 側回 429 `M_LIMIT_EXCEEDED`（Matrix 既有）。
- **限速先於憑證檢查**：洪水不該還花一次 DB 查找。所以 bucket 空時，鎖定帳號的 Login 也回 `RateLimited` 而不是 `M_USER_LOCKED`。
- ⚠️ **限速的 key 是 client IP，它的效力前提是那個 IP 可信**（review，rumia／salvia）。沒設 `ip_source` 時 `ClientIp` 先讀 `X-Forwarded-For` 等 header 才退回 socket 位址，攻擊者每次換 header 就拿到一個新 bucket。
  **部署須知**：直接向公網的伺服器要設 `ip_source`（`rightmost` 加受信代理 subnet），或者確定反向代理覆寫 XFF。這是 OIDC 限速既有的前提，登入限速預設開、所以要講明。
- 🚫 不做「連錯 N 次鎖帳號」：那是讓攻擊者能鎖住別人帳號的 DoS 入口。

#### 6.3.5 `Hello`

`Hello.features` 多 `"login"`。client 據此決定要不帶 Bearer 升級再 Login，還是走舊路。

#### 6.3.6 不做的

- 不做 `Register`：註冊有 UIAA 互動流程、registration token、驗證信，那是一整章（Matrix spec §「Account registration」），等 §3.3 的 kind 分配到它再說。
- 不做 SSO／OIDC over WS：需要瀏覽器。
- 不做「登入後 server 主動推 sync」：那是另一個提案（串流訊息、Event 領域），跟登入無關。

#### 6.3.7 驗收（e2e7 新情境）

- 不帶 Bearer 升級 → `Hello` 可、`Ping` 可、`Upload/Create` 回 `Error(Unauthorized)`；一直 Ping 但不 Login，到 `wbf_ws_unauthenticated_timeout` → Close 1008（e2e 把它壓到 3 秒）。
- `Login`（密碼）→ Ack 帶 token；接著 `Upload/Create` 可；同一 token 打 HTTP `/_wbf/v1/pack` 也可（同一個 session）。
- 錯密碼 → `Error(Forbidden)` 且連線仍開；連錯到超過 burst → `Error(RateLimited)` 帶 `retry_after_ms`；同一 IP 打 HTTP `/login` 也 429。
- `Refresh` → 新 access token，舊的打 HTTP 401；連線繼續可用。
- `Logout` → Ack 緊接 Close 1000；那個 token 打 HTTP 401。已登入的連線再 `Login` 別的帳號 → Ack，之後的 pack 以新帳號計。`Logout { all: true }` → 同 user 另一條已登入的 WS 在下一個 Ping 收到 `Error(Unauthorized)` ＋ Close 1008。
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

#### 6.3.9 維護者 2026-09-07 定的

1. 限速預設 1／10，預設開。
2. **Logout 直接關線**（Ack 後 Close 1000）；換帳號重開一條 WS，或直接再 Login。
3. 同一條連線重複 Login 允許（換 Session，不強制中斷）。
4. **未登入的連線 30 秒就斷**，從升級起算、不是 idle。
5. client 的登入重試是 client 的事；server 只要即時回應。

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
