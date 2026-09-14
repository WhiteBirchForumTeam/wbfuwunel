# 常用 Matrix API 走通道：一座通用的橋，而不是一支一支手搬

> **這份文件回答：怎麼把大部分常用的 Matrix client API 改成 WebSocket pack，搬的順序是什麼，每一支要花多少。**
> 狀態：📄 提案，等維護者同意。維護者 2026-09-14：「把大部分常用的 api 接口改成 web socket pack 的模式 —— account 註冊、登入、登出、session 相關、room 相關、device，看能做多少、多快；行數少就多做一點，難度高就少做一點，慢慢移植。」
> 上位文件：[wbf-pack-pipeline.md](wbf-pack-pipeline.md) §7（搬一個端點的七步）、[wbf-wire-format.md](wbf-wire-format.md) §3.3（kind 分配表）。

## 0. 一句話

**不一支一支手寫 handler。** 做一座通用的橋：把 pack 轉成一個**內部的 `http::Request`（不走網路）** —— **kind ＋ subtype 查出 method 與模板、meta 是模板的變數、data 就是 body** —— 直接丟進 axum 的 `Router` 跑一次 —— 認證、關卡、ruma 解析、route 函式全部是 HTTP 進來時的同一條路，回應的 JSON 原樣放進 `Ack`。之後搬一個端點＝**分配表加一列**（一個 subtype 號對一個 ruma 的 Request 型別），外加向量與 e2e。
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

### 2.0 整條路（維護者 2026-09-14 重申）

```
傳輸層：WebSocket  或  POST /_wbf/v1/pack（request body 就是一個 pack，response body 回一個 pack）
  → 解出 pack
  → 看 flags bit4 分流
       bit4 = 1（走橋）  → 查「橋的分配表」→ 轉成內部 HTTP request → Router → 轉回 pack（§2.1–§2.3）
       bit4 = 0（原生）  → 查「原生的准入表」→ wbf 自己的 handler（`Recent`、`Send`、`Upload/*`…）
       自己那張表查不到 → Error pack（UnknownKind；每條路只查自己的表，§2.3）
  → 原路返回：WebSocket 進來的從同一條 WebSocket 回；HTTP pack 進來的，回應 pack 放在同一個 HTTP response 的 body
```

- **兩張表，各自一份**：橋的分配表（kind／subtype → Matrix 端點）、原生的准入表（`wbf/mod.rs` 的 `admission`，已經存在）。📌 一個 (kind, subtype) 只會出現在其中一張（§2.3），這條要有一個單元測試守著：兩張表的鍵沒有交集。
- **原路返回不用新寫**：現在的 `Reply` 本來就分兩種出口（WebSocket 的佇列、HTTP 的單一回應），handler 只管 `reply.send(pack)`，不知道自己在哪條路上（pipeline §4.2）。橋照這個契約寫就自動原路返回。
- **HTTP pack 的認證在傳輸層**：`pack_route` 先用 HTTP 的 `Authorization: Bearer` 認出 session 才解 pack，所以 HTTP pack 上沒有匿名；WebSocket 則是連線的 session（可能還沒登入）。橋拿到的就是這一個 session 的 token，兩條路一樣。

### 2.1 一個 pack 的路：組成一個內部的 HTTP request，丟進 Router，不走網路

維護者 2026-09-14 定的做法：**pack 轉成一個 `http::Request`，直接交給 axum 的 `Router` 跑一次**，不開 socket、不繞回自己的 port。

```
pack（kind＝領域、subtype＝操作；meta＝變數；data＝body 的 bytes）
  → 分配表：subtype → ruma 的 Request 型別 → method（`Req::METHOD`）、URL 模板（`Req::PATH_BUILDER`）、header 模板
  → 組 http::Request：URL 模板的空格用 meta 的變數填（path 與 query），header 模板同理
                      body ＝ pack 的 data，**原樣的 bytes**
                      Authorization: Bearer <這條連線 session 的 access token>   ← server 填，client 蓋不掉
                      extensions: ConnectInfo(<這條連線的對端位址>)              ← 限速與 client IP 照常
  → Router::call(request)                      ← 就是 HTTP 進來時走的那一條，一條都不少
       axum 比對路徑 → 填 path 參數 → Args::from_request（token、鎖定、暫停、UIAA、ruma 解析）
       → route 函式（原封不動）
  → http::Response：2xx → 狀態碼與 header 進 Ack 的 meta、body 的 bytes 進 Ack 的 data
                    其他 → Error，照狀態碼與 Matrix 的 errcode 對應（§2.4）
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

### 2.2 pack 的三段對到 HTTP 的哪三段（維護者 2026-09-14 定）

[wbf-wire-format.md](wbf-wire-format.md) §2 已經定死一個 pack 長什麼樣：header、meta、data。橋要定的只是**這三段對到 HTTP 的哪裡**：

| pack | HTTP | 誰決定 |
|---|---|---|
| kind ＋ subtype | method ＋ URL 模板 ＋ header 模板 | **server 的分配表**（client 只送號碼） |
| meta | 模板裡的**變數**（填進 path、query、header） | client |
| data | **body，原樣的 bytes** | client |

```
kind=Room, subtype=SetTopic
  → (PUT, /_matrix/client/v3/rooms/{room}/state/m.room.topic/, { "Content-Type": "{format}" })
meta = { "room": "!abc:localhost", "format": "application/json" }
data = {"topic":"大家好"} 的 bytes
```

⭐ **為什麼 data 當 body，而不是把 body 塞進 meta**：

1. **E2EE 的密文原樣過去。** data 是 bytes，server 不解析、不重新編碼 —— 密文送進來什麼樣、進 route 就什麼樣。塞進 meta 的 JSON 會多一次 parse 與 re-serialize，而那是會改變 bytes 的（鍵的順序、數字的寫法）。
2. **本來就是這樣做的。** `Event/Send` 的 meta 是 `{room_id, type, txn_id, attachments}`、data 是事件內容的 JSON；`Upload/Chunk` 的 data 是塊的 bytes。這個對應不是新規則，是把既有的做法變成通則。
3. **撞名問題直接消失。** 提案原本要在 meta 裡分 `path`／`query`／`body` 三段，就是因為有些端點的 body 是**使用者的任意 JSON**（account data 的內容鍵叫什麼都可以，包括 `roomId`），攤平會跟變數撞名。body 不進 meta，就沒有命名空間要共用，也不需要「撞名時誰贏」那條會被寫錯的規則。
4. **回應對稱**：狀態碼與 header 進 `Ack` 的 meta、body 的 bytes 進 `Ack` 的 data。以後要搬**回二進位**的端點（舊的媒體下載、縮圖）不用再發明一套。
   ⚠️ 出去的 header 也照同一條規矩**由表決定**（例如只轉 `Content-Type`），🚫 不要把 response 的 header 整包倒給 client —— 那會把 server 自己的東西（中介層加的、將來某天加的）一起送出去。

#### 五條規則（維護者 2026-09-14 同意）

1. 🚨 **header 模板由 server 的表決定，client 不能自己塞 header。** meta 的變數只能填**表上宣告過的空格**。否則 client 塞一個 `X-Forwarded-For` 就能假裝自己是別的來源位址（繞過限速），或塞這個 fork 自己的 `X-Wbf-Attachments`。**`Authorization` 與來源位址一律由 server 填，client 給的永遠蓋不掉。**
2. **query 是 URL 的一部分**，一起寫在模板裡（`?limit={limit}&dir={dir}`）。⚠️ **變數沒給就整個省掉那個 query 參數**，🚫 不要填空字串 —— `""` 跟「沒有」是兩件事（全域原則 A6）。
3. **path 的變數要 percent-encode**：房間是 `!abc:localhost`、使用者是 `@alice:localhost`、別名有 `#`，不編碼就把路徑切壞了。**變數缺了、或不是字串 → 拒絕**，不要用空值硬拼。
4. **meta 出現表上沒有的變數 → 拒絕**，🚫 不要默默忽略。忽略的話，client 打錯欄位名會表現成「server 安靜地用了預設值」，那種 bug 最難找。
5. **`Content-Type` 預設寫死在表裡**（Matrix 這些端點就是 JSON）。像上面例子那樣讓它當變數，**只在表明確宣告的端點開放** —— 之後搬媒體上傳會需要，一般端點不需要。

#### 一個 subtype 對一個端點（維護者 2026-09-14 定）

上面的例子把 `m.room.topic` 寫死在路徑裡，等於一個 subtype 比一個端點更窄。**定案是 1:1**：`eventType`、`stateKey` 都是變數，一個 subtype 就是一個 Matrix 端點。

- 表比較短，而且 method 與 URL 模板**直接取自 ruma 的型別**（`Req::METHOD`、`Req::PATH_BUILDER`）—— 上游改路徑或改名字，建置就會壞。
- 更窄的別名（「設 topic」這種）以後真的常用再加，不要一開始就讓同一個端點有兩個入口。
- 📎 **變數的名字就用 ruma 模板裡的名字**（`room_id`、`event_type`、`state_key`），不另取一套 —— 另取一套就是第二份會漂移的對照表。每一列的變數名寫進 wire-format §3.2。

### 2.3 旗標 bit4 `IS_BRIDGED`：這個 pack 是不是走橋（維護者 2026-09-14 定）

旗標位組現在用到 bit3（`META_ENCRYPTED`、`WANT_ACK`、`IS_RESPONSE`、`IS_LAST`），**bit4 給橋**：設了就是「這個 pack 是一個轉成 HTTP 請求的 Matrix 端點呼叫」。

🚨 **兩條路各自「正面認得」，認不得就回錯誤，不猜**（全域原則 A5：不是正面認得，就落到安全值）。**每條路只查自己那張表**（維護者 2026-09-14 定）：

```
bit4 分流之後：

橋的路（bit4 = 1）
  防呆：bit4 不是 1 → Unsupported（1102）      ← 分流已經保證了，基本上進不來
  橋的分配表查得到 (kind, subtype)？ 查不到 → UnknownKind（1101）
  轉換：變數缺、多、不是字串（§2.2 規則 3、4） → InvalidRequest
  → 走橋（§2.1）

原生的路（bit4 = 0）
  防呆：bit4 不是 0 → Unsupported（1102）      ← 同上
  原生准入表查得到 (kind, subtype)？ 查不到 → UnknownKind（1101）
  → 准入表其餘的規則照舊（「HTTP 不可」→ Unsupported、未登入 → Unauthorized、id 型別）
  → 原生 handler（`Recent`、`Send`、`Upload/*` ……）
```

⭐ **為什麼不跨表查**：提案先前一版在查不到時會再去查**另一張表**，有就回 `Unsupported`（「你走錯路了」）。那讓兩條路互相知道對方的表 —— 為了一個比較貼心的錯誤訊息，在兩張表之間開了一道接縫（全域原則 A4）。每條路只認得自己的表，兩張表才能各自增減、互不牽動。

📎 **代價與為什麼可以接受**：client 帶錯 bit4（例如 `Recent` 帶了 bit4）收到的是 `UnknownKind` 而不是「走錯路」。那是 client 程式的 bug，開發時第一次打就會看到，而且在改程式之前重試本來就沒用 —— `UnknownKind` 的「別重試」並不誤導。

📎 **`Unsupported` 的意思因此保持單純**：它只在原生准入表的「這個 kind 不走這個傳輸」時出現（例：`Recent` 從 HTTP pack 送來），以及上面兩個防呆。橋本身不看傳輸層（§2.5），所以橋的路上不會因為傳輸回 `Unsupported`。

📎 **防呆為什麼留著**：分流那一行保證了 bit4 跟路一致，但那是**別處的承諾**（全域原則 A6：消費端自己再問一次）。以後有人改了分流，這兩行會把 bit4 不對的 pack 擋下來，而不是讓它被另一條路默默處理掉。

📌 **號碼空間只有一個**：一個 (kind, subtype) **不是分給原生 handler，就是分給橋，不會兩個都有**。bit4 是 client 的**聲明**，要跟分配對得上；它不是第二個號碼空間（不會出現「`0x14/0x05` 在 bit4=0 時是 A、bit4=1 時是 B」）。一個號碼一個意思，向量與 client 的程式碼才不會有歧義。

為什麼兩邊都要擋：這個旗標是 **client 說它在跟哪一個東西講話**。寬容地「猜它的意思」會讓同一個號碼在不同版本的 server 上跑到不同的地方，而那種錯誤不會報錯，只會做錯事。

⭐ **副作用是免費的向下相容保護**：`Flags::KNOWN` 從 `0b0000_1111` 擴成 `0b0001_1111`，而舊規則是「**保留位元非 0 就拒收**」（wire-format §2）—— 所以一個會說橋的 client 碰到**舊版 server** 時，拿到的是 `Corrupt`（1002）而不是一個被當成別的意思執行的請求。這正是 fail closed 要的形狀。

📌 **回應也帶 bit4**：橋的 `Ack` 跟一般的 `Ack` **形狀不同**（meta 是狀態碼與 header、data 是 body 的 bytes，§2.2），所以回應把 bit4 抄回去，client 不必靠「我記得我剛才發的是橋請求」來解讀。

⚠️ 要改的地方：`core/wbf/pack.rs` 的 `Flags`（新常數、`KNOWN`、`is_bridged()`）、wire-format §2 那張欄位表與「其餘保留」那句、§3.4 的 `Unsupported` 那列要講到這兩種情形，以及向量（一個橋請求、一個橋回應）。

### 2.4 錯誤：現在的對應表會把大部分 Matrix 錯誤變成 `Internal`

`Reject::from(Error)`（`wbf/mod.rs`）只認 404／410、400、413，**其餘一律 `Internal`** —— 包括 403（`M_FORBIDDEN`）、401（`M_UNKNOWN_TOKEN`、`M_USER_LOCKED`）、429（`M_LIMIT_EXCEEDED`）。之前的 handler 碰不到這些，橋會大量碰到。

要補的：
1. 狀態碼對應補齊：401 → `Unauthorized`、403 → `Forbidden`、429 → `RateLimited`（帶 `retry_after_ms`）。
2. ✅ `Error` 的 meta **多帶 Matrix 的 `errcode`**（維護者 2026-09-14 同意，放在 meta；例如 `M_ROOM_IN_USE`、`M_USER_SUSPENDED`）。wire 的 `code` 是這條通道的詞表、分類得很粗；client 要顯示「房間別名已被使用」這種訊息，需要 Matrix 的那一個。📎 這是 wire-format §3.4 的增補，要寫進那張表。
3. UIAA 的 401 帶 `flows`／`session`／`completed`：那是批 3 的事（§3）。

### 2.5 走 WS 還是走 HTTP pack：橋不管（維護者 2026-09-14 定）

pack 可以從兩條路進來：WebSocket，或 `POST /_wbf/v1/pack`。**兩條路傳的都是 pack，而 pack 走不走橋只看 bit4**（§2.3）—— 橋本身不看傳輸層，也不為任何一邊開例外。

📎 HTTP pack 包一個 Matrix 端點看起來多此一舉（直接打那個 HTTP 端點就好），實際上兩者差不多；它的價值是 debug 與 fallback（pipeline §0-12），不必為了它擋掉什麼。

⚠️ 那「HTTP 不可」的規則還在不在：**在，但它屬於原生 handler，不屬於橋**。准入表（pipeline §3）本來就依 kind 決定哪些 HTTP 不准 —— `Recent` 串流、`Stream` 草稿、會改變連線身份的 `Login`／`Refresh`／`Logout`，以及之後手寫的 `Register`（批 2）。這些都不在分配表裡，所以 bit4 帶不進去，照舊由准入表擋。

## 3. 分批

「行數少就多做一點、難度高就少做一點」—— 有了橋之後，**難度不看 route 多長，看它需不需要橋沒有的東西**。所以分批的依據是這個：

### 批 1：橋 ＋ 一般的已登入端點（難度低，量多）

不改變連線身份、不需要 UIAA、回應是普通 JSON。每支＝分配表一列 ＋ 向量 ＋ e2e。

| kind | 在 app 裡是什麼動作 | Matrix 端點 |
|---|---|---|
| `0x11 Account` | 查「我是誰」（登入後拿自己的 user id、device id） | `GET /account/whoami` |
| | 看別人／自己的**暱稱、頭像**；改自己的 | `GET`／`PUT /profile/{userId}/displayname`、`…/avatar_url` |
| | **app 自己的設定存在 server 上**（例如通知偏好、置頂清單；可以是整個帳號的，也可以是某個房間的） | `GET`／`PUT /user/{userId}/account_data/{type}`、`…/rooms/{roomId}/account_data/{type}` |
| | 房間的**標籤**（我的最愛、低優先） | `GET`／`PUT`／`DELETE /user/{userId}/rooms/{roomId}/tags…` |
| `0x13 Room` | **開房間** | `POST /createRoom` |
| | **加入**房間（用 id 或 `#別名`）、**離開**、**把離開的房從清單移除** | `POST /join/{roomIdOrAlias}`、`…/leave`、`…/forget` |
| | **邀請**、**踢人**、**封鎖**、**解除封鎖** | `POST /rooms/{roomId}/invite`、`kick`、`ban`、`unban` |
| | 我**加入了哪些房間** | `GET /joined_rooms` |
| | 房間**成員名單** | `GET /rooms/{roomId}/members` |
| | 房間**別名**（`#name:server`）的查、設、刪 | `GET`／`PUT`／`DELETE /directory/room/{roomAlias}` |
| `0x14 Event` | 用 event id **拿單一則訊息**（例如點開回覆引用的那則） | `GET /rooms/{roomId}/event/{eventId}` |
| | 讀／改**房間設定**：名稱、topic、頭像、權限……（Matrix 叫 state） | `GET`／`PUT /rooms/{roomId}/state/{eventType}/{stateKey}` |
| | **收回訊息**（redact） | `PUT /rooms/{roomId}/redact/{eventId}/{txnId}` |
| | **跳到某則訊息**，連同它前後幾則（搜尋結果、通知點進去） | `GET /rooms/{roomId}/context/{eventId}` |
| `0x15 Receipt` | 「**對方正在輸入…**」 | `PUT /rooms/{roomId}/typing/{userId}` |
| | **已讀到哪裡**（未讀數、已讀標記） | `POST /rooms/{roomId}/read_markers`、`POST /rooms/{roomId}/receipt/…` |
| `0x16 Device` | **登入過的裝置清單**、單一裝置的資訊、**改裝置名稱** | `GET /devices`、`GET`／`PUT /devices/{deviceId}` |

📎 送訊息、翻歷史、收新訊息、to-device 這些**已經有原生的 pack**（`Event/Send`、`Event/Recent`、`Subscribe`、`Device/*`），不在這張表。

約 35 支。估計：橋本身（Router、分配表、錯誤對應、回應轉換）加測試幾百行；之後每支幾行加一個向量、一個 e2e 檢查。

⚠️ **批 1 的 e2e 規矩**：每支都要**原本的 Matrix HTTP 端點打一次、橋打一次、結果一致**（pipeline §7 第 6 步）。另外**關卡要有反向檢查**，不能只驗正常路徑：被暫停的帳號在 WS 上 `createRoom` 被拒、被鎖的帳號被拒 —— 這幾條就是這座橋存在的理由，沒有測到等於沒有。

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
2. ~~**meta 三段分開還是攤平？**~~ ✅ **都不是**（維護者 2026-09-14）：body 根本不進 meta —— **data 就是 body**，meta 只放模板的變數（§2.2）。撞名問題因此消失；五條規則與「一個 subtype 對一個端點」也一併定了。
3. ~~**`Error` 多帶 Matrix `errcode`？**~~ ✅ **要，放在 meta**（維護者 2026-09-14，§2.4）。線上格式的增補，client 要跟。
4. ~~**橋上的操作准走 HTTP pack？**~~ ✅ **橋不看傳輸層**（維護者 2026-09-14，§2.5）：WS 與 HTTP 傳的都是 pack，走不走橋只看 bit4。「HTTP 不可」留在原生 handler 的准入表。
5. ~~**批 1 的清單要增要減？**~~ ✅ **這批就夠**（維護者 2026-09-14）。方向是**之後幾乎全部搬過去**，不必一個 commit 全上，一批一批來、常用的先上。
6. ~~**subtype 號什麼時候給？**~~ ✅ **開始寫程式時才補進文件**（維護者 2026-09-14）。照 Matrix 規格章節裡端點出現的順序排，寫進 wire-format §3.2；分配了就不改（§3.3）。粒度（一個 subtype 對一個端點）已定。

⚠️ **實作還沒有開始，也還沒有要開始**：維護者會給明確的開始訊號。這份文件在那之前只是定案的設計。

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
