# 真伺服器端到端測試（Windows，PowerShell 5.1）

這裡的腳本啟動**真的** `tuwunel.exe`（e2e profile，`target\e2e\`），用 HTTP 與 WebSocket 打它，逐條檢查點印 `ok`／`FAIL`，最後一行是總結。
它們是這個 fork 自己功能的回歸網；單元測試證明不了 SQL 有效、檔案真的被刪、或不同路徑讀出同一個東西，這些才證明得了。

| 腳本 | 驗什麼 | 設計文件 |
|---|---|---|
| `e2e6.ps1` | 分塊上傳／下載（HTTP 一包一請求）：Create、有序塊、續傳、Seal、截斷、串流模式、按塊與明文位置讀、Abort、sweeper；情境 6：本地 Seal 256 MiB（128 × 2 MiB）時**在 Seal 還在飛的時候取樣** server 的 working set（沒修漲 256 MiB、修了 ~10 MiB）；情境 7：重啟讓快取變冷後 `Status` 與 `Seal` 同時在飛，已封存媒體的列不被誤刪（PR #48）。**舊式**：每行印預期值，沒有 pass／fail 總結，看 `results.txt` | [chunked-upload-spec.md](../../docs/design/chunked-upload-spec.md) |
| `e2e7.ps1` | WebSocket 通道：Hello、Ping、一 message 一 pack、HTTP 與 WS 交錯續傳、idle 關線；情境 3：鎖定帳號兩個傳輸都拒、登出後下一個 pack 被拒並關線、連線開著時 `!admin server shutdown` 正常退出無 dangling；情境 4（Session kind）：匿名升級只能 Hello／Ping、3 秒沒登入被關、`Login`／`Refresh`／`Logout`、錯密碼、HTTP 與 channel 共用的登入限速、同一連線換帳號、logout-all 關掉另一條、鎖定帳號不能登入；情境 5（每 device 連線上限，設 2）：第三條 Bearer 升級 429、另一 device 不受影響、匿名 Login 滿了回 `TooManyConnections` 並被關且**不發 token**（原連線的 token 仍可用）、關一條後名額回來、同連線再登入不多佔、Logout 放回名額。超過單包上限的 frame 被 socket 層丟掉（送 3 MiB：上限是 `wbf_meta_max_bytes` ＋ `wbf_data_max_bytes` ＋ 外框，PR #50 之後約 2.1 MiB）。舊式同上 | [wbf-wire-format.md](../../docs/design/wbf-wire-format.md) §6.1、§6.3、[review-followups-2026-09-06.md](../../docs/design/review-followups-2026-09-06.md) §2.3／2.4 |
| `e2e8.ps1` | 每房 `r_seq`、全域 `g_seq`、`Event/Recent` 的 `Batch` 串流（一窗、`tc`／`bc`／`fs`／`ls`／`r`、`cg_seq`／`before` 翻窗、`batch`／`limit` 夾值、byte 上限切 Batch、**窗的 bytes 上限（`wbf_window_max_bytes`）先於 `limit` 截斷一窗、`more: true`、帶 `before` 翻到 `more: false` 每則恰好一次、被截斷的訂閱補窗第一個 `Push` 帶 `gap: true`**、HTTP 回 `Unsupported`、兩窗之間 Ping）、`rooms` 點名（PR #51：一個房 ＋ `before` 就是那個房的歷史、`r_seq` 連續、不在的房 `Forbidden`、`[]` 空窗、同一個房點兩次只算一次）、舊庫啟動的一次性編號；情境 4：30 個房間的一窗 first byte 量測（冷／暖，門檻只有「冷 < 2 秒」）；情境 5：**帳號抹除（MSC4025）之後才加入的人，在 `Recent` 與 `Subscribe{cg_seq}` 補窗裡拿到剪過的事件**（跟 `/messages` 一樣），當時就在房間的人仍拿到原文（PR #54） | [room-seq-and-recent.md](../../docs/design/room-seq-and-recent.md) |
| `e2e9.ps1` | 附件宣告（header 與 `Event/Send`）、四種拒送、共用附件、明文 fallback、bot 一次性警告、redact 保留備份後 purge、掃描不碰新上傳 | [media-attachments.md](../../docs/design/media-attachments.md) |
| `e2e10.ps1` | 媒體持有者集合：刪房／redact 保留備份／備份到期／頭像／purge 範圍／重複操作、`WBFUWUNEL_MEDIA_GRACE_SECONDS` 下的掃描；情境 4（要 `E2E_OLD_EXE`）：既存媒體被生成縮圖後仍不受管、不被掃 | [media-holders.md](../../docs/design/media-holders.md)、[review-followups-2026-09-06.md](../../docs/design/review-followups-2026-09-06.md) §2.1 |
| `e2e11.ps1` | WS 訂閱與推送（channel）：帳號層 `Subscribe` 只推給訂閱那條、自己送的也推、`cg_seq` 先補再推、新加入的房自動跟（點名訂閱不跟）、**點名訂閱的補窗只含它自己的房、重複點名只算一次**（PR #51）、離房／被踢停推、ignore 不推、`Unsubscribe` 冪等、HTTP 回 `Unsupported`；情境 2：訂閱者停讀時 100 則 60 KB 的訊息不被擋、恢復後有 `gap: true`、`Recent` 補齊；情境 3：連線健康計數器；情境 4：草稿（`0x02 Stream`，PR #45）—— 錨點是真事件、片的 `prev` 鏈、`id` 型別 byte、權限與可見性的外部審查七條 | [wbf-event-push.md](../../docs/design/wbf-event-push.md)、[streaming-messages.md](../../docs/design/streaming-messages.md) |
| `e2e12.ps1` | **to-device 走通道（`0x16 Device`）**：`device_id` 對不上回 `Forbidden`、**同一裝置後來的連線接手佇列，被接手的那條收到 `Superseded`(1505)（帶它自己的訂閱 id、`IS_LAST`）**、換成另一個帳號登入時舊裝置的佇列被放掉、推送帶每則的 count 與訂閱的 `id`、`Fetch` 舊→新且 `r=0`、沒被截斷的窗 `more: false`、`ItemsDestroy` → `Ack`（收到）→ `ItemsDestroyed`（結果）、已經不在的仍算銷毀、`tc` 與 data 不符就一個都不刪、非 holder 銷毀回 `Forbidden`、HTTP 回 `Unsupported` | [wbf-to-device.md](../../docs/design/wbf-to-device.md) |
| `e2e13.ps1` | **橋（flags bit4 `IS_BRIDGED`）**：情境 0 是這一層本身 —— 帶 bit4 只查橋的表、不帶只查原生的准入表，查不到都是 `UnknownKind`；走橋的回覆（含錯誤）帶 bit4，原生的不帶；WS 與 HTTP pack 行為一樣；bit5 仍是保留位元（`Corrupt`）；未登入的連線也是先查表。情境 1 是批 1 的 37 支：每支橋打一次、原本的 HTTP 端點打一次、結果一致（去掉 `age`、`last_seen_*` 這種時間性欄位後比）；錯誤路徑（404、403）也比；橋自己擋的（缺變數、多出表上沒有的變數、`id` 不是 0）不會呼叫端點；未登入走橋拿到端點自己的 401 `M_MISSING_TOKEN`；關卡的反向檢查 —— 被暫停的帳號走橋 `CreateRoom` 被拒（`M_USER_SUSPENDED`）、跟 HTTP 一模一樣；被鎖的帳號送走橋的 pack 在**橋之前**就被 WS 的 session 檢查拒絕（`Unauthorized`、訊息帶 `M_USER_LOCKED`、關連線）。情境 2 是批 2 的 8 支（另外起三台 server）：匿名連線查帳號名、登入方式、註冊碼（v1 路徑）；`Register` 的 UIAA 兩輪（`flows` 跟 HTTP 一樣）→ 帶 `inhibit_login` 建帳號、沒發 token → 同一條連線送原生 `Login` 變成那個帳號、超過裝置名額照樣 `TooManyConnections`；`system` 註冊不到；刪裝置／批次刪／改密碼／停用帳號的 UIAA（密碼錯是 `M_FORBIDDEN` 且 `session` 不變、`logout_devices` 保留這條連線、刪自己的裝置或停用後下一個 pack 在橋之前被拒）；註冊碼錯、禁用帳號名、關閉註冊，都跟 HTTP 拒得一模一樣。情境 3 是 E2EE (A) 的 7 支（金鑰上傳／查詢／claim／變動／交叉簽章／簽章，以及發 to-device 之後對方持有連線收到原生 `Push`、同一個 `txn_id` 不重送）。情境 4 是 E2EE (C) 的 14 支金鑰備份：建版本、改 `auth_data`、三種寫入（整份／單房／單 session）與對應的讀、刪，刪掉的讀回來跟 HTTP 拒得一樣，版本不存在兩條路都回空而不是拒絕，meta 少了 `version` 是橋自己的 `InvalidRequest` | [wbf-api-bridge.md](../../docs/design/wbf-api-bridge.md)、[bridge-specs/index.md](../../docs/bridge-specs/index.md)、[wbf-e2ee.md](../../docs/design/wbf-e2ee.md) |
| `e2e14.ps1` | **伺服器帳號 `@system`**：新資料庫上它叫 `@system`、profile 顯示名稱是 `[SYS] localhost`、沒有 `@conduit`；`system` 從第一刻起就註冊不到（`M_USER_IN_USE`）；管理員房間裡它自己的 join 事件帶顯示名稱；同一個資料庫重啟照常過閘門；情境 2（要 `E2E_OLD_EXE`，改名前的 binary）：舊版上有人註冊了 `system`，新版在那個資料庫上**拒絕啟動** | [server-user.md](../../docs/design/server-user.md) |
| `e2e15.ps1` | **E2EE 的 `Device/CryptoState`（`0x16 0x08`）**：`Subscribe` 之後一定跟一個 `CryptoState`，每欄都在（`unused_fallback_key_types: []` 真的出現）、沒有 `device_lists`；上傳 OTK 與 fallback key、別人 claim 一把，都推新的數量，沒加到任何一把的上傳不推；OTK 用完之後 claim 拿到 fallback key，推的狀態變成 `[]`；後來的連線接手之後只有它收，`Push` 與 `CryptoState` 共用一個 `seq`；Matrix 原本的 `/keys/changes` 沒被動到（`left` 照上游是空的） | [wbf-e2ee.md](../../docs/design/wbf-e2ee.md) §3 |
| `wbf-helpers.ps1` | 共用：pack 編解碼（CRC-32C）、HTTP／WS 傳輸、起停 server、寫設定檔。不是測試 | [wbf-wire-format.md](../../docs/design/wbf-wire-format.md) |
| `build-win.ps1` | 建 e2e profile 的 binary（MSVC 環境、Windows 的 feature 組） | [windows-build.md](../../docs/design/windows-build.md) |

更早的 e2e2–e2e5（媒體引用計數、哨兵、migrate-references）針對的是已被持有者集合取代的計數器，沒有進 repo；它們驗的行為
（redact 立刻釋放、備份持有、purge 只釋放一次、410 墓碑）現在由 e2e9 情境 2 與 e2e10 涵蓋。

## 跑

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File tests\e2e\build-win.ps1   # 第一次約 35 分，之後改一檔約 10 分
powershell -NoProfile -ExecutionPolicy Bypass -File tests\e2e\e2e10.ps1
```

- 所有產物（設定檔、資料庫、server log、`results.txt`）在 `target\e2e-runs\<腳本>-out\`，不會寫到腳本旁邊。
- server 固定聽 `127.0.0.1:8015`；每支腳本要**依序**跑，不要並行。
- 環境變數：`E2E_EXE` 換 binary；`E2E_OLD_EXE` 給 e2e8 情境 2 與 e2e10 情境 4（要一個 PR #22 之前的 binary，沒有就跳過）；
  `WBFUWUNEL_MEDIA_GRACE_SECONDS` 由 e2e10 情境 3 自己設與清。
- 沒有機密：帳號密碼是固定的測試字串，只連 localhost。

## 寫新的

- 從 `e2e10.ps1` 抄骨架：dot-source helpers、`Check name ok detail`、每個情境一個乾淨的資料庫目錄、結束 `Stop-Server`。
- ⚠️ PowerShell 變數**不分大小寫**：`$m3` 會蓋掉 `$M3`。狀態碼用 `$st…` 之類的名字。
- ⚠️ 字串裡變數後面接 `?` 要寫 `${id}?width=…`：5.1 把 `$id?` 當成一個叫 `id?` 的變數（問號是合法名字元），URL 會少一段、server 回 400。
- ⚠️ 中文不要寫進 `.ps1` 字面值（5.1 讀無 BOM 檔案當 ANSI 會變亂碼）；訊息用英文。
- helper 只放真的共用的東西；腳本專用的函式留在腳本裡（`Room-Messages` 之類），不然沒定義時只在 console 印例外，`results.txt` 看不到。
- ⚠️ 大小寫那條的實例（2026-09-07）：檢查式裡寫 `$b = $pg.batches[$_]` 就把 helpers 的 `$B`（base URL）蓋成 Hashtable，之後每個 HTTP 呼叫都失敗，
  server 重啟看起來像「server did not come up」。`Start-Server` 放棄前現在會印最後一個錯誤與一個新 HttpClient 的結果，把「server 沒起來」和「這個 process 的 HTTP 壞了」分開。
  helpers 另外把 ServicePoint 的 `DefaultConnectionLimit` 拉到 64（預設 2，留幾條 WS 就排隊），用完的 WS 還是要 `Dispose()`。
- ⚠️ 等 server 回應的接收要有時限（e2e8 的 `Ws-Recv-Bounded`；e2e7 的 `Recv-Frame`）：`ReceiveAsync().Result` 沒時限，server 不回就整支腳本卡住、沒有任何輸出。
- ⚠️ **超時的 `ReceiveAsync` 不能丟掉**：.NET 的 `ClientWebSocket` 讓它繼續掛著，下一個 frame 會被它吃掉，之後的接收就永遠等不到（e2e11 第一版就這樣卡死）。
  e2e11 的 `Recv-Or-Null` 把還沒完成的 task 按 socket 記著、下次先等它。推送類的腳本（有 server 主動送的 frame）一定會撞到這條。
- 🚨 **「跑不完」多半是跑完了，只是行程不退出**（2026-09-11 整晚都在這條上）。兩個原因，都修掉了：
  (1) **server 沒被收乾淨** —— 一台活著的 server 會佔住 port 與 DB，而且它繼承的 handle 讓呼叫端的重導向不 EOF，
  於是一個**已經寫完 RESULT 的 run** 看起來完全像 hang（實測：e2e11 寫完最後一行之後 server 還活了 11 分鐘）。
  `Stop-Server` 現在會**掃到沒有任何 `tuwunel` 為止**，不是發一次 kill 就算。
  (2) 腳本結尾補 `exit`，順便**讓 FAIL 反映在 exit code 上**（以前一律 0，只有文字看得出來）。
  📎 修完之後 e2e11 是 **56 秒**、e2e12 18 秒、e2e9 46 秒、e2e8 47 秒 —— 之前每一輪都以為要等好幾分鐘。
- 🚨 **build 之前也要先殺乾淨**：還在跑的 `tuwunel.exe` 鎖著 `target/e2e/tuwunel.exe`，linker 覆蓋不了就
  `存取被拒。(os error 5)`。⚠️ 更糟的是**接著跑的 e2e 會用舊的 binary**，看起來一切正常 —— 2026-09-10 的紅燈驗證就這樣白做了一輪。
  順序寫死：**殺乾淨 → build → 跑**。
- 🚨 **一次只跑一支，起跑前先確認沒有殘留的 `tuwunel` 進程**（`Get-Process tuwunel`）。每支腳本用**同一個 port**，
  所以前一支沒收乾淨的 server 會讓下一支看起來「卡住不動」或「大量 `Unauthorized`」——2026-09-10 兩種都踩到了，
  而且第二種**不會失敗、會假裝通過**。
  ✅ **2026-09-13 起 `Start-Server` 自己擋這件事**：port 已經有人回應就拒絕啟動，探測成功時自己啟動的行程已經退出也拒絕（那代表回應的不是它）。
  起因是一支腳本中途 `throw`、**沒跑到 `Stop-Server`**，下一輪整輪對著舊 server 與舊資料庫跑（破綻是新 server 卻報 `connection_id=6`）。
  ⚠️ 卡住時先看 `target/e2e-runs/<腳本>-out/results.txt`（它寫到哪一步），
  不要只看被 `Select-String` 過濾過的 tail —— 那次我就據此把停住的位置判斷錯了一步。
- ⚠️ **`gap: true` 是下一個「推得進去」的 `Push` 才帶的**：flood 之後要再送一則訊息才看得到它。推論也成立 —— 掉包之後那個房如果再無新事件，這個旗標**永遠不會到**，所以測試（與 client）都不能把「沒收到 `gap`」當成「沒漏過」。
- ⚠️ 函式回傳單元素陣列會被攤平成那個元素；呼叫端用 `@()` 包，函式裡**不要**再 `return ,$x`（兩邊都包會變成陣列裡包陣列，`.Count` 是 1、`[0]` 是整個陣列）。
  hashtable 的屬性存了單一物件時 `.Count` 是空的，判斷用 `@($x.events).Count`。
- 🚨 **被拒絕或已關閉的 WS 會讓接收迴圈空轉**（2026-09-13）：`ReceiveAsync` 立刻回「0 byte、`EndOfMessage` 為 false」，
  而且一直這樣回，所以依 `EndOfMessage` 的 `do…while` 永遠轉、每次 `Wait($ms)` 都立刻成功、逾時永遠不觸發 ——
  **腳本 100% CPU、server 閒著、沒有任何輸出**。e2e11 的 `Recv-Or-Null` 現在把這種讀取當成連線已關並把狀態印出來。
  ⭐ 判斷「在等」還是「在空轉」看 CPU：那次 PowerShell 燒了 1454 秒，server 只用了 0.5 秒。
- ⚠️ **關掉一條連線不代表名額立刻回來**：有推播還排著的連線會先把佇列排空（`DRAIN_TIMEOUT`）才結束、才還名額 ——
  實測兩次都是 **5.1 秒**。在 `wbf_ws_max_connections_per_device` 的邊上要換一條新連線，**不要用固定 sleep 猜**，
  用 e2e11 的 `Ws-Open-Usable`：開連線、`Hello` 來回證明可用、在期限內重試，並把等了多久記下來。
