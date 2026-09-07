# 真伺服器端到端測試（Windows，PowerShell 5.1）

這裡的腳本啟動**真的** `tuwunel.exe`（e2e profile，`target\e2e\`），用 HTTP 與 WebSocket 打它，逐條檢查點印 `ok`／`FAIL`，最後一行是總結。
它們是這個 fork 自己功能的回歸網；單元測試證明不了 SQL 有效、檔案真的被刪、或不同路徑讀出同一個東西，這些才證明得了。

| 腳本 | 驗什麼 | 設計文件 |
|---|---|---|
| `e2e6.ps1` | 分塊上傳／下載（HTTP 一包一請求）：Create、有序塊、續傳、Seal、截斷、串流模式、按塊與明文位置讀、Abort、sweeper。**舊式**：每行印預期值，沒有 pass／fail 總結，看 `results.txt` | [chunked-upload-spec.md](../../docs/design/chunked-upload-spec.md) |
| `e2e7.ps1` | WebSocket 通道：Hello、Ping、一 message 一 pack、HTTP 與 WS 交錯續傳、idle 關線；情境 3：鎖定帳號兩個傳輸都拒、登出後下一個 pack 被拒並關線、連線開著時 `!admin server shutdown` 正常退出無 dangling；情境 4（Session kind）：匿名升級只能 Hello／Ping、3 秒沒登入被關、`Login`／`Refresh`／`Logout`、錯密碼、HTTP 與 channel 共用的登入限速、同一連線換帳號、logout-all 關掉另一條、鎖定帳號不能登入。舊式同上 | [wbf-wire-format.md](../../docs/design/wbf-wire-format.md) §6.1、§6.3、[review-followups-2026-09-06.md](../../docs/design/review-followups-2026-09-06.md) §2.3／2.4 |
| `e2e8.ps1` | 每房 `r_seq`、全域 `g_seq`、`Event/Recent`（`cg_seq`／`before`／byte 上限）、舊庫啟動的一次性編號 | [room-seq-and-recent.md](../../docs/design/room-seq-and-recent.md) |
| `e2e9.ps1` | 附件宣告（header 與 `Event/Send`）、四種拒送、共用附件、明文 fallback、bot 一次性警告、redact 保留備份後 purge、掃描不碰新上傳 | [media-attachments.md](../../docs/design/media-attachments.md) |
| `e2e10.ps1` | 媒體持有者集合：刪房／redact 保留備份／備份到期／頭像／purge 範圍／重複操作、`WBFUWUNEL_MEDIA_GRACE_SECONDS` 下的掃描；情境 4（要 `E2E_OLD_EXE`）：既存媒體被生成縮圖後仍不受管、不被掃 | [media-holders.md](../../docs/design/media-holders.md)、[review-followups-2026-09-06.md](../../docs/design/review-followups-2026-09-06.md) §2.1 |
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
- server 固定聽 `127.0.0.1:8015`；三支腳本要**依序**跑，不要並行。
- 環境變數：`E2E_EXE` 換 binary；`E2E_OLD_EXE` 給 e2e8 情境 2 與 e2e10 情境 4（要一個 PR #22 之前的 binary，沒有就跳過）；
  `WBFUWUNEL_MEDIA_GRACE_SECONDS` 由 e2e10 情境 3 自己設與清。
- 沒有機密：帳號密碼是固定的測試字串，只連 localhost。

## 寫新的

- 從 `e2e10.ps1` 抄骨架：dot-source helpers、`Check name ok detail`、每個情境一個乾淨的資料庫目錄、結束 `Stop-Server`。
- ⚠️ PowerShell 變數**不分大小寫**：`$m3` 會蓋掉 `$M3`。狀態碼用 `$st…` 之類的名字。
- ⚠️ 字串裡變數後面接 `?` 要寫 `${id}?width=…`：5.1 把 `$id?` 當成一個叫 `id?` 的變數（問號是合法名字元），URL 會少一段、server 回 400。
- ⚠️ 中文不要寫進 `.ps1` 字面值（5.1 讀無 BOM 檔案當 ANSI 會變亂碼）；訊息用英文。
- helper 只放真的共用的東西；腳本專用的函式留在腳本裡（`Room-Messages` 之類），不然沒定義時只在 console 印例外，`results.txt` 看不到。
