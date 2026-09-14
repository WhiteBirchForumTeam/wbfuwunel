# 常用 Matrix API 走通道：一座通用的橋，而不是一支一支手搬

> **這份文件回答：怎麼把大部分常用的 Matrix client API 改成 WebSocket pack，搬的順序是什麼，每一支要花多少。**
> 狀態：📄 提案，等維護者同意。維護者 2026-09-14：「把大部分常用的 api 接口改成 web socket pack 的模式 —— account 註冊、登入、登出、session 相關、room 相關、device，看能做多少、多快；行數少就多做一點，難度高就少做一點，慢慢移植。」
> 上位文件：[wbf-pack-pipeline.md](wbf-pack-pipeline.md) §7（搬一個端點的七步）、[wbf-wire-format.md](wbf-wire-format.md) §3.3（kind 分配表）。

## 0. 一句話

**不一支一支手寫 handler。** 做一座通用的橋：把 pack 轉成一個**內部的 `http::Request`（不走網路）**，直接丟進 axum 的 `Router` 跑一次 —— 認證、關卡、ruma 解析、route 函式全部是 HTTP 進來時的同一條路，回應的 JSON 原樣放進 `Ack`。之後搬一個端點＝**分配表加一列**（一個 subtype 號對一個 ruma 的 Request 型別），外加向量與 e2e。
📌 這個形狀是維護者 2026-09-14 定的（「內部做一個 ws pack 轉換成內部 http request 的方法，直接塞進去 route，不走網路丟一次」）；比提案原本寫的「自己呼叫 route 函式」少動一個上游檔案，理由在 §2.1。

## 1. 為什麼不照 `Login` 那樣手搬

pipeline §7 的第 3 步是「把業務邏輯抽到 `service/`，HTTP 與 pack handler 都呼叫它」，`Login` 就是這樣做的（`issue_session`）。這一輪盤點下來，**大部分常用端點不需要這一步**，而且照這樣手搬反而會出事：

### 1.1 route 函式本來就很薄

邏輯早就在 service 層，route 只是拆 request、呼叫、組 response：

| 端點 | route 行數 | 內容 |
|---|---|---|
| `POST /rooms/{roomId}/leave` | 33 | 拿房間鎖 → `services.membership.leave(...)` |
| `GET /devices` | 22 | `services.users.all_devices_metadata(...)` |
| kick／ban／unban | 各 32 | 同上形狀 |
| `POST /createRoom` | 1147 | 大，但**整支就是它自己的邏輯**；搬過去不需要改它 |

所以「搬」要做的其實只有：pack ↔ ruma request／response 的轉換，加上認證。**這兩件事每一支都一樣。**

### 1.2 🚨 真正的風險在關卡，而關卡不在 route 函式裡

HTTP 請求進 route 之前，`router/auth.rs` 會依 **route 的型別**（`TypeId`）套用政策：

| 關卡 | 內容 |
|---|---|
| token | 找不到、過期、appservice token |
| MSC3939 鎖定 | 帳號被鎖 → `M_USER_LOCKED`；**logout 兩支例外** |
| MSC3823 暫停 | 被暫停的帳號**不能** join／invite／knock／kick／ban／unban／createRoom／upgrade／改 profile |
| 可選認證 | `require_auth_for_profile_requests`、公開房間目錄 |
| UIAA | 註冊、停用帳號、改密碼、刪裝置要走互動式認證 |

手搬一支，就要記得把這幾條**再寫一次**；搬三十幾支就是三十幾份。漏掉的那一支不會 fail closed —— 例如被暫停的帳號在 WS 上照樣能 `createRoom`。這正是 [room-seq-and-recent.md](room-seq-and-recent.md) §2 剛修掉的那種洞：`Event/Recent` 只抄了 `/messages` 三道關卡裡的兩道（PR #54）。

⭐ 橋的做法是**不抄關卡，走同一條路**：請求經過的就是 HTTP 進來時的那個 Router 與 `Args::from_request`，上游之後在 `auth.rs` 加的政策，WS 自動有。

## 2. 橋怎麼接

### 2.1 一個 pack 的路：組成一個內部的 HTTP request，丟進 Router，不走網路

維護者 2026-09-14 定的做法：**pack 轉成一個 `http::Request`，直接交給 axum 的 `Router` 跑一次**，不開 socket、不繞回自己的 port。

```
pack（kind＝領域、subtype＝操作，meta＝{ path?, query?, body? }）
  → 分配表：subtype → ruma 的 Request 型別（只用它的 METADATA 拿 method 與 path 模板）
  → 組 http::Request：path 模板填上 meta.path、query 串上 meta.query、body ＝ meta.body
                      Authorization: Bearer <這條連線 session 的 access token>
                      extensions: ConnectInfo(<這條連線的對端位址>)   ← 限速與 client IP 照常
  → Router::call(request)                      ← 就是 HTTP 進來時走的那一條，一條都不少
       axum 比對路徑 → 填 path 參數 → Args::from_request（token、鎖定、暫停、UIAA、ruma 解析）
       → route 函式（原封不動）
  → http::Response：2xx → body 的 JSON 原樣當 Ack 的 meta
                    其他 → Error，照狀態碼與 Matrix 的 errcode 對應（§2.3）
```

⭐ **這一版比「自己呼叫 route 函式」好在哪**：

| | 自己呼叫 route 函式 | 丟進 Router |
|---|---|---|
| path 參數 | ⚠️ axum 的 `Path` extractor 只替**經過 Router 比對過**的 request 填參數，所以要另寫一個 `from_pack`，把 `Args::from_request` 的中段抽成共用 | ✅ Router 自己填，`Args::from_request` **一個字都不用改** |
| 關卡（token、鎖定、暫停、UIAA） | 走同一段程式，但接縫是我新開的 | ✅ 走的就是 HTTP 那條路本身 |
| 要動的上游檔案 | `router/args.rs` | ✅ **零**（下面那個 seam 是 fork 自己的檔案） |
| 每支端點的成本 | 分配表一列（型別 ＋ route 函式） | 分配表一列（只要型別） |

📎 **層級沒有問題**：Matrix 的路由表是 **`api` crate 自己的** `tuwunel_api::router::build()` 組的（`router` crate 只是接上 fallback、掛 middleware、`with_state`）。所以橋可以在 `api` 裡組一份自己的 `Router<State>`，不必反過來依賴 `router` crate。**唯一的 seam**：`State` 裡那個 Services 指標是 `router` crate 建的，所以由它在啟動時呼叫一次 `wbf::install_bridge_router(state)`，把組好的 Router 交給橋。一行，方向還是 `api ← router`。

📎 **橋用的是自己組的那份 Router，不是對外服務的那份** —— 對外那份掛著 CORS、壓縮、逾時這些 HTTP 的 middleware，對一個內部呼叫沒有意義（壓縮還要再解一次）。

🚨 **分配表是白名單，pack 不帶自由的 path**。client 送的是 kind ＋ subtype，method 與 path 由 server 這邊的表決定。反過來做（pack 裡直接寫 method 與 path）等於把**每一個** HTTP 端點都開到通道上 —— 包括 `/sync`（會佔住這條連線的 handler 幾十秒）、舊的媒體上傳、以及規格裡刻意標成「HTTP 不可」的那幾個（`Recent`、`Stream`）。認不得的 subtype 一律拒絕（fail closed）。

⚠️ **代價：路徑對不對從編譯期變成執行期**。所以分配表存的是 **ruma 的 Request 型別**，method 與 path 模板從它的 `METADATA` 取 —— 上游改路徑或改型別名字，**建置就會壞**，不是上線才壞。另外加一條啟動檢查／測試：分配表每一列都對橋的 Router 打一次不帶 token 的請求，**回 401 才算這條路存在**（回 404 就是表寫錯了）。

### 2.2 meta 的形狀

```json
{ "path": { "roomId": "!abc:example.org" }, "query": { "limit": 10 }, "body": { "reason": "spam" } }
```

- **三段分開**，不攤平：有些端點的 body 是**任意 JSON**（account data 的 body 就是使用者自己的內容，鍵叫什麼都可以，包括 `roomId`）。攤平就要一條「撞名時誰贏」的規則，而那條規則會在某個使用者剛好用了那個鍵的時候，悄悄把使用者自己的資料當成 path 參數吃掉。
- 三段都可省略；沒有 path 參數的端點就不帶 `path`。
- `path` 的鍵用 **Matrix 規格上的名字**（`roomId`、`userId`、`eventType`、`stateKey`），client 對著規格寫就對，不必知道 server 內部。
- 回應 meta ＝ **Matrix 回應的 JSON 原樣**。不另造欄位（pipeline §7 第 2 步）。

### 2.3 錯誤：現在的對應表會把大部分 Matrix 錯誤變成 `Internal`

`Reject::from(Error)`（`wbf/mod.rs`）只認 404／410、400、413，**其餘一律 `Internal`** —— 包括 403（`M_FORBIDDEN`）、401（`M_UNKNOWN_TOKEN`、`M_USER_LOCKED`）、429（`M_LIMIT_EXCEEDED`）。之前的 handler 碰不到這些，橋會大量碰到。

要補的：
1. 狀態碼對應補齊：401 → `Unauthorized`、403 → `Forbidden`、429 → `RateLimited`（帶 `retry_after_ms`）。
2. `Error` 的 meta **多帶 Matrix 的 `errcode`**（例如 `M_ROOM_IN_USE`、`M_USER_SUSPENDED`）。wire 的 `code` 是這條通道的詞表、分類得很粗；client 要顯示「房間別名已被使用」這種訊息，需要 Matrix 的那一個。📎 這是 wire-format §3.4 的增補，要寫進那張表。
3. UIAA 的 401 帶 `flows`／`session`／`completed`：那是批 3 的事（§3）。

### 2.4 HTTP 准不准

pack 也能走 `POST /_wbf/v1/pack`。橋上的操作**准走 HTTP**：它們本來就是 HTTP 端點，擋掉沒有好處，而且 debug 時方便（pipeline §0-12：HTTP pack 是 debug／fallback）。
**例外**：會改變「這條連線是誰」的操作（批 2 的 `Register`）跟 `Login` 一樣只走 WS —— HTTP 上沒有連線可以改。

## 3. 分批

「行數少就多做一點、難度高就少做一點」—— 有了橋之後，**難度不看 route 多長，看它需不需要橋沒有的東西**。所以分批的依據是這個：

### 批 1：橋 ＋ 一般的已登入端點（難度低，量多）

不改變連線身份、不需要 UIAA、回應是普通 JSON。每支＝分配表一列 ＋ 向量 ＋ e2e。

| kind | 端點 |
|---|---|
| `0x11 Account` | `whoami`；profile 取／設（displayname、avatar_url）；account data 取／設（global、room）；tags 取／設／刪 |
| `0x13 Room` | `createRoom`；join（id 或 alias）；leave；forget；invite；kick；ban；unban；`joined_rooms`；members；別名取／設／刪 |
| `0x14 Event` | 取單一事件；state 取／設；redact；context |
| `0x15 Receipt` | typing；read markers；receipt |
| `0x16 Device` | 列出裝置；取單一裝置；改裝置名稱 |

約 35 支。估計：橋本身（`from_pack`、分配表、錯誤對應、回應轉換）加測試幾百行；之後每支幾行加一個向量、一個 e2e 檢查。

⚠️ **批 1 的 e2e 規矩**：每支都要 **HTTP 打一次、WS 打一次、結果一致**（pipeline §7 第 6 步）。另外**關卡要有反向檢查**，不能只驗正常路徑：被暫停的帳號在 WS 上 `createRoom` 被拒、被鎖的帳號被拒 —— 這幾條就是這座橋存在的理由，沒有測到等於沒有。

### 批 2：註冊（難度中）

`POST /register` 的 route 有 493 行，而且有三件橋沒有的事：
- **匿名**：沒有 session 就能發。
- **會改變連線身份**：註冊成功就發 token，這條連線要變成那個帳號 —— 跟 `Login` 一樣要過 `wbf_ws_max_connections_per_device` 的名額閘門（pipeline §2.1）。
- **註冊的 UIAA**：registration token、`m.login.dummy`。

所以它是一支手寫的 `Session/Register`（`0x10` 已經有 Login／Refresh／Logout），比照 `Login` 把「發 session」那段抽出來共用。`register/available`、`token_validity` 這兩支是匿名的唯讀查詢，走橋就好。

### 批 3：要 UIAA 的（難度中高）

停用帳號、改密碼、刪裝置（單一與批次）。橋能跑它們，但 UIAA 是多來回的：第一次回 401 帶 `flows` 與 `session`，client 帶 `auth` 再送一次。要定的是 WS 上那個 401 長什麼樣（`Error` 帶哪些欄位），以及 client 怎麼接回同一個 `session`。這批等批 1 的錯誤格式定下來再做。

### 不搬（至少這一輪）

| 端點 | 為什麼 |
|---|---|
| `/sync` | long-poll 在 WS 上的形狀是 server 推送，已經是 `Subscribe`／`Push`；另一件事 |
| SSO、JWT、LDAP、appservice 登入 | 要瀏覽器或是給服務用的 |
| 舊的整檔媒體、縮圖 | 已經有 `Upload`／`Download` |
| keys、backup、cross-signing（`0x17`）、push rules（`0x18`）、search | 不在維護者這次點名的範圍；橋做好之後各是分配表的幾列，之後再排 |

## 4. 會不會跟正在進行的 PR 撞

`wbf/recent-erasure`（#54）改的是 `wbf/recent.rs` 的 `collect_window`、兩份設計文件、e2e8、CHANGELOG。這個提案要動的是 `wbf/mod.rs` 的派發、新檔 `wbf/bridge.rs`、`router/router.rs` 一行、wire-format §3.2／§3.3／§3.4、pipeline §7 —— **沒有同一個檔案的同一段**。CHANGELOG 照慣例合併後才寫，不會同時改。實作分支等這份文件同意之後才開，從當時的 main 開。

## 5. 要維護者決定的

1. ~~**橋，還是手搬？**~~ ✅ **定了**（維護者 2026-09-14）：用橋，而且是**把 pack 轉成內部的 HTTP request 丟進 Router**這一版（§2.1）。上游檔案一個都不用動。
2. **meta 三段分開**（§2.2）還是攤平？我建議分開。
3. **`Error` 多帶 Matrix `errcode`**（§2.3）。這是線上格式的增補，client 要跟。
4. **橋上的操作准走 HTTP pack**（§2.4）？我建議准，`Register` 除外。
5. **批 1 的清單**（§3）要增要減？
6. **subtype 號**：我照 Matrix 規格章節裡端點出現的順序排，寫進 wire-format §3.2；分配了就不改（§3.3 的規矩）。要不要先給號、還是批 1 寫程式時一起給？

## 6. 同意之後的落點

| 做什麼 | 落點 |
|---|---|
| 橋自己的 Router（`tuwunel_api::router::build` 組一份，不掛 middleware）、分配表、meta ↔ request／ response 轉換 | `src/api/client/wbf/bridge.rs`（新） |
| 把組好的 Router 交給橋（啟動時一行，因為 `State` 是那邊建的） | `src/router/router.rs` 呼叫 `wbf::install_bridge_router(state)` |
| 上游的 `router/args.rs`、`auth.rs` | **不動** |
| 錯誤對應補齊、`errcode` | `src/api/client/wbf/mod.rs` 的 `Reject::from(Error)`；wire-format §3.4 |
| 派發 | `wbf/mod.rs` 的 `dispatch`：`0x11`／`0x13`／`0x15` 與 `0x14`、`0x16` 裡新的 subtype 進橋 |
| 契約 | wire-format §3.2 每支一列；pipeline §7 改寫成「搬一個端點＝分配表一列」 |
| 向量、e2e | `core/wbf/vectors.rs`；新腳本 `tests/e2e/e2e13.ps1` |
