# E2EE 下的媒體引用：送訊息時宣告 attachments、計數 0 由後台掃描清

> **這份文件回答：server 讀不到訊息內容時，媒體的引用計數從哪裡來；沒人來指的媒體怎麼辦。**
> 狀態：📄 提案，2026-09-06，維護者在對話中已定方向（§2），等同意文件後開實作分支。
> 上位文件：[media-gc.md](media-gc.md)（計數、收集器、墓碑、哨兵都不變）；
> 核心設計 [why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md) §5.4。
> client 條款同步寫在 [chunked-upload-spec.md](chunked-upload-spec.md) §12。

## 0. 一句話

引用計數的**機制**（merge operator、同交易 ±1、收集器、墓碑、每 mxc 一鎖）全部保留；換掉的是 **+1 的來源**。
明文房間 server 照舊自己讀 content；**E2EE 房間 server 讀不到，所以 client 在送訊息的請求裡宣告 `attachments`**，
server 記在自己的表裡、同交易 +1，redact／purge／retention 查自己的表 −1。**計數 0 的媒體由後台掃描清掉**：新上傳被指到之前
計數是 0，保護期（至少 7 天）過後就會被清；既存媒體維持哨兵、不計不刪，**不做遷移**。

## 1. 破口

`media_ref.rs` 讀的是 `content.url`、`content.file.url`、縮圖那幾個 key。E2EE 房間送到 server 的事件是 `m.room.encrypted`，
`content` 只有 `ciphertext`，那些 key 全在密文裡。所以 E2EE 房間的附件：

- 上傳寫 `Init(0)`，沒有任何事件替它 +1 → 收集器只在 −1 後動手，它永遠不會被刪（漏水）；
- 但 `migrate-references` 重算會把「本地、≤ 0、超過 10 分鐘」當孤兒**刪掉** —— 那是活著的附件。

兩個方向都錯。根因不是 redact（redaction 是明文事件、由 server 執行，server 永遠知道哪則被撤），而是**建立指針那一步 server 看不到**：
上傳時媒體沒綁任何訊息，指針要等訊息來建立，E2EE 把它藏進密文。靠 content 重算對 E2EE 是死路，遷移工具救不了它。

## 2. 維護者定的方向（2026-09-06）

1. **既存媒體不管、不計、不刪。** 哨兵（`i64::MIN`）照舊，遷移無效、不做。
2. **不另設 TTL。計數 0 的媒體由後台掃描清掉**，不分通道、不分房間是否加密。新上傳 `Init(0)` 起算，**被指到之前計數是 0，
   保護期（至少 7 天）過後隨時可能被清理**。（現在沒有週期掃描，收集器只在 −1 後動手；這支要加，見 §4.3。）
3. **明文 or 附件 mxc ⇒ 計數。** 明文房間 server 自己讀 content；E2EE 房間 server 讀不到，指針由送訊息的請求夾帶
   （`attachments: [mxc, …]`），不放進事件本體。兩者取聯集。
4. 刪除觸發不變：`MIN < 計數 ≤ 0` 就刪。
5. 非 wbf client 在 E2EE 房間傳媒體時，**server bot 私訊一條英文警告**（§6）。

「約定會漏」的答案：漏掉的兩種後果都是安全的方向。漏宣告 → 那份媒體被後台掃描清掉，client 自己會發現附件不見（不是刪到別人的）；
漏解綁 → 多留一份。**永遠不會刪到有人還指著的東西。**

## 3. 連結放哪裡：請求旁邊，不進事件

| 位置 | 進雜湊／聯邦 | 備註 |
|---|---|---|
| 事件 `content` 明文 key | 是 | 別家 client 與別站看得到；污染事件 |
| **送訊息的請求旁邊** | **否** | server 反正知道這件事，讓它記在自己的表裡。事件乾淨 |
| 上傳時綁房間 | 否 | 壽命變成房間級，撤訊息不會刪媒體；否決 |

選第二種。兩個入口，同一套 server 端處理：

- **wbf pack `Event/Send`**（kind `0x14`、subtype `0x02`，無序類）：meta `{ "room_id", "type", "txn_id", "attachments": [mxc…] }`，
  data = 事件 content 的 JSON bytes（E2EE 就是 `m.room.encrypted` 的 content）。這是 wbf 通道接管 client API 的**第一個操作**，
  理由正好是它：綁定要跟送訊息**同一個請求、同一筆交易**，否則 client 在兩個請求之間掛掉，就留下一則密文指著一個計數 0 的媒體。
- **舊的 HTTP `PUT /_matrix/client/v3/rooms/{room}/send/{type}/{txn}`** 加一個 header `X-Wbf-Attachments: mxc1,mxc2`。
  給還在用 matrix-rust-sdk 送訊息的 client 過渡用；header 不是事件的一部分，server 端走同一條路。

## 4. server 端

### 4.1 新表 `eventid_mxcs`：`event_id → [mxc]`

- 寫入：`append_pdu` 的同一筆交易。內容 = 宣告的清單 ∪ `list_content_mxc_uris(content)`（明文房間讀得到的）。去重。
- 讀取：redact、`purge_history`、retention 丟原文備份 —— 現在讀 content 的三處改成**先查這張表**，查不到再退回讀 content
  （舊事件沒有列；它們指的媒體本來就是哨兵，退回讀 content 只是為了明文房間的舊事件仍能正確 −1）。
- 事件被刪（`del_event`、purge）時連這列一起刪。
- **為什麼不寫進 `unsigned`**：`r_seq`／`g_seq` 是給 client 看的；attachments 是 server 自己的帳，不該送出去，也不該進 redact 要剝的地方。

### 4.2 宣告的驗證（fail closed）

server 收到 `attachments` 逐一驗：是 `mxc://`、本站的、`search_file_metadata` 找得到、**上傳者是 sender**、沒墓碑。
任何一個不過 → **整則拒送**（`Error(Conflict)` 帶哪個 mxc 為什麼），不寫事件。理由：留下一則指著空媒體的訊息，比讓 client 的 bug 當場浮出來糟。
限制：一則最多 N 個（config，預設 32）。

### 4.3 後台掃描：計數 0、非哨兵、過了寬限期就清

- 現況：收集器只在 −1 交到手上時動手；`migrate-references` 是手動離線工具。**沒有東西會去掃計數 0 的媒體**，這支要加。
- 做法：`media_refs` 服務多一個週期性任務（間隔 config `media_gc_sweep_interval`，預設 3600 秒），掃 `mxc_refcount` 裡
  **本地、非哨兵、計數 = 0、建立時間超過保護期** 的 mxc，持每 mxc 一鎖、鎖下重讀計數仍為 0 → `media.collect(Unreferenced)`。
  跟 migrate 刪孤兒同一個決策形狀（rumia 在 #12 抓過的：決策要在鎖下）。
- **保護期：新 config `media_unreferenced_grace_seconds`，預設 604800（7 天），維護者 2026-09-06 定「至少 7 天」。**
  不沿用 `media_gc_migrate_skip_recent_seconds`（600）—— 那是 migrate 避開「剛上傳還沒送出」的窗口，這裡是「上傳了但一直沒人指」的判定，
  兩個問題不同，數量級也不同。設定值低於 7 天啟動時 warn 並夾成 7 天（fail closed：寧可多留）。
- 為什麼不用區分「從沒被指」跟「被指過又歸零」：後者收集器當下就刪了，能在 0 停留超過保護期的只有前者。
- 頭像不受影響：`set_avatar_ref` 是 server 看得到的引用，照舊 +1。縮圖跟原檔同一個 mxc，前綴刪除一起走。

### 4.4 `migrate-references` 的角色縮小

重算只對「有 `eventid_mxcs` 列或 content 讀得到」的事件有意義。它**不再**把計數 ≤ 0 的當孤兒刪 —— 那個工作交給 4.3 的掃描，
規則只有一條、時間可預期。migrate 保留為「重算計數」的工具，dry-run 印差異。
順手：`media_gc_migrate_skip_recent_seconds` 預設 600 太緊，維護者 2026-09-06 建議改成 86400（一天）；這支一起改預設值。
保護期的判定方式兩邊一樣：**要刪的那一刻**看媒體的建立時間，近期的跳過，不是另外記一個到期時間。

## 5. client 條款（也寫在 spec §12）

- **凡是 server 讀不到 content 的訊息（E2EE），送出時必須宣告 `attachments`**，否則附件會被後台掃描清掉（保護期 7 天後）。
- 上傳 → 拿到 mxc → 把 mxc 放進加密內容 → **同一個送訊息請求**帶 `attachments`。不要分兩個請求。
- 編輯（`m.replace`）若換附件：新事件宣告新的；舊事件不動（它自己被 redact 時才 −1）。
- 轉傳（forward）同一個 mxc 到另一則：再宣告一次，計數 +1；各自 redact 各自 −1。
- 明文房間可以不宣告，server 自己讀；宣告了也沒關係（聯集、去重）。
- `Hello.features` 多 `attachments`，client 據此決定要不要帶。

## 6. 對非 wbf client 的警告：bot 私訊一條英文

server 認得出的訊號：事件是 `m.room.encrypted`、從舊 HTTP `send` 進來、**沒有** `X-Wbf-Attachments`、而 sender 有計數 0 的新上傳。
三個同時成立就是「Element 之類在 E2EE 房間傳了附件」。

做法（維護者 2026-09-06 定）：server bot（admin room 發話的那個使用者）對這個 user 開 DM，送一則明文 `m.room.message`，
**每個 user 只發一次**（記一列 `userid_attachmentwarned`），不每則吵。不拒送、不加 config 選項。訊息全文（定稿，改字要回到這裡）：

> This server keeps an uploaded file only while a message is known to use it. Your client just sent an encrypted message
> without telling the server which uploaded files it attaches, so files you attach in encrypted rooms from this client
> will be deleted by the server's cleanup shortly after upload. To keep attachments in encrypted rooms, use a client that
> supports this server's attachment declaration (wbf).

## 7. 待維護者定

1. ~~掃描的寬限期~~ 已定：另開 `media_unreferenced_grace_seconds`，至少 7 天。
2. `Event/Send` 要不要這支就做，還是先只做 HTTP header（client 現在用 matrix-rust-sdk 送，header 對它最省）？我建議這支兩個都做，
   pack 是目標、header 是過渡。

## 8. 驗收

- 單元：宣告驗證每條拒絕原因；`eventid_mxcs` 與事件同交易；聯集去重。
- e2e：E2EE 房間（`m.room.encryption` state）用 header 宣告送 `m.room.encrypted` → 計數 1 → redact → 計數 0 → bytes 刪、410；
  不宣告 → 計數 0 → 過保護期被掃、410（e2e 用 config 壓到幾秒；低於 7 天會被夾，所以測試要走一個測試專用的旁路或直接測 sweep 函式）；宣告別人的 mxc → 拒送；明文房間不宣告仍 +1；同一 mxc 兩則訊息 → 2 → 各撤一次才刪；
  非 wbf client 情境收到 bot 私訊且只收一次；既存哨兵媒體掃描不碰。

## 9. 落點

| 什麼 | 哪裡 |
|---|---|
| `eventid_mxcs` 表、驗證、聯集 | `src/service/media_refs/`、`src/database/maps.rs` |
| 送訊息入口（header、`Event/Send`） | `src/api/client/send.rs`（或 `Ruma` 抽 header）、`src/api/client/wbf/` |
| redact／purge／retention 改查表 | `timeline/redact.rs`、`timeline/purge.rs`、`retention/mod.rs` |
| 週期掃描 | `media_refs/collect.rs`（收集器旁邊，同一把每 mxc 鎖） |
| 警告 | `src/service/admin/`（bot 發話的既有路徑） |
| config：`media_gc_sweep_interval`、`media_unreferenced_grace_seconds`（≥ 7 天）、`attachments_max_per_event` | `src/core/config/mod.rs` |
