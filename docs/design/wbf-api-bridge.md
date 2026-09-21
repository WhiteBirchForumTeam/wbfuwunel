# 常用 Matrix API 走通道：一座通用的橋，而不是一支一支手搬

> **這份文件回答：怎麼把大部分常用的 Matrix client API 改成 WebSocket pack，搬的順序是什麼，每一支要花多少。**
> 狀態：✅ 維護者同意（PR #55）；橋的層與批 1 的 37 支 PR #56、批 2（註冊＋UIAA，8 支）PR #62＋#63、批 3（20 支＋新 kind `0x1D Report` ＋ `features`）PR #76＋#77，**都已合併**（總表 86 列）。**批 4（21 支＋四個新 kind）2026-09-21 提案中**（§3 批 4，一條待決定在 §5）。維護者 2026-09-14：「把大部分常用的 api 接口改成 web socket pack 的模式 —— account 註冊、登入、登出、session 相關、room 相關、device，看能做多少、多快；行數少就多做一點，難度高就少做一點，慢慢移植。」
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

📎 **層級沒有問題**：Matrix 的路由表是 **`api` crate 自己的** `tuwunel_api::router::build()` 組的（`router` crate 只是接上 fallback、掛 middleware、`with_state`）。所以橋可以在 `api` 裡組一份自己的 `Router<State>`，不必反過來依賴 `router` crate。**唯一的 seam**：`State` 裡那個 Services 指標是 `router` crate 建的，所以由它建橋的 Router（`tuwunel_api::router::build_bridge_router(state, server)`），**用 axum 的 `Extension` 掛在對外的 Router 上**；WebSocket 與 HTTP pack 兩個入口從請求裡取出來、放進 `PackContext`。方向還是 `api ← router`。
🚨 **為什麼不是「啟動時存進一個全域變數」**（提案原本這樣寫，實作時改掉）：橋的 Router 持有 `State`，而 `State` 是指向 `Services` 的裸指標。tuwunel 支援在**同一個 process 裡重載模組**（`src/main/mods.rs`），`Services` 會被重建；全域那一份會繼續指著已經丟掉的 `Services`。掛在對外的 Router 上，它就跟那個 Router 同生同死，永遠不會活得比它指著的 `Services` 久。

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

### 批 2：註冊 ＋ 要 UIAA 的（✅ 維護者 2026-09-15 同意，五條決定都照建議）

> 原本的規劃是「批 2 註冊手寫 `Session/Register`、批 3 UIAA 另外定格式」。批 1 做完再看，**兩件都不需要新的東西**，所以併成一批、全部走橋。下面是理由與要維護者決定的地方。

#### 2-A. 註冊不必手寫：`inhibit_login` ＋ 既有的 `Session/Login`

原本擔心的三件事（§3 舊版）：

| 擔心的 | 現在 |
|---|---|
| **匿名**：沒有 session 就能發 | 橋本來就不要求 session：沒登入的 WS 連線送橋的 pack，就是不帶 `Authorization` 的 HTTP 請求（批 1 e2e13 已驗：匿名的 `WhoAmI` 到得了端點、拿到端點自己的 401） |
| **會改變連線身份** | Matrix 的 `/register` 有 **`inhibit_login: true`**：只建帳號，不建裝置、不發 token。建完再送一個既有的 **`Session/Login`**，連線身份的變更、`wbf_ws_max_connections_per_device` 名額閘門、登入限速，全部走已經有的那條路 |
| **註冊的 UIAA**（registration token、`m.login.dummy`、同意條款、email） | 跟 2-B 同一條規則：第一次回 401 帶 `flows`，client 帶 `auth` 再送 |

所以流程是：

```
client                                          server
  │ Session 0x21 UsernameAvailable {username}  →   （可選）
  │ Session 0x20 Register  data {username, password, inhibit_login: true}
  │                                           ←   Error 401, data {flows, session}      ← UIAA 第一輪
  │ Session 0x20 Register  data {…, auth: {type, session, …}}
  │                                           ←   Ack, data {user_id}                    ← 帳號建好，沒有 token
  │ Session 0x01 Login     meta {password 登入}  →   （原生，不帶 bit4）
  │                                           ←   Ack {user_id, device_id, access_token} ← 這條連線變成那個帳號
```

- ⭐ **關卡一條不少**：`allow_registration`、registration token、appservice 命名空間、`forbidden_usernames`、`@system` 被佔住（#57），全是 HTTP 那一道本身。**省掉的是 493 行 route 的抽取**，而原本要抽的正是這些關卡最容易漏抄的地方。
- 代價：多一個來回；密碼在同一條連線上送兩次。
- ⚠️ **沒帶 `inhibit_login`** 的註冊：端點照 HTTP 的行為建裝置、發 token，token 在回覆的 data 裡，但**這條連線不會變成那個帳號**（橋不改連線身份、也不看 body）。不危險（跟 HTTP 呼叫一模一樣），只是白發一個 token。→ **決定 1**。
- 訪客註冊（`kind=guest`）不能 `inhibit_login`，同上：拿得到 token，連線不變。→ 併入決定 1。
- 📎 HTTP pack 在傳輸層就要 token，所以匿名註冊**只能走 WS**。
- ⚠️ **沒登入的 WS 連線只活 `wbf_ws_unauthenticated_timeout`（預設 30 秒，從升級算起，ping 不延長）**。要 email 驗證、或使用者看到 `flows` 才去找註冊碼的註冊，一定超過。註冊的 UIAA `session` 存在 server（鍵是伺服器帳號，跟連線無關），所以 client **重連一條匿名連線、帶同一個 `session` 接著送**就行。→ **決定 4**。

#### 2-B. UIAA 不必另定格式：批 1 已經把整個 Matrix 錯誤 body 放進 data

批 1 定的「失敗回覆的 data ＝ Matrix 錯誤 body 原樣」（bridge-specs §4 第 1 點），就是為了這一批。UIAA 在 WS 上長這樣，**沒有新欄位**：

```
第一輪  → Account 0x2D Deactivate   data {}
       ← Error  01 01 03 14
           meta {"code":"Unauthorized","code_id":1301,"message":"…","status":401}
           data {"flows":[{"stages":["m.login.password"]}],"session":"xYz","params":{}}
第二輪  → Account 0x2D Deactivate   data {"auth":{"type":"m.login.password","session":"xYz","identifier":{…},"password":"…"}}
       ← Ack    data {"id_server_unbind_result":"no-support"}
密碼錯  ← Error  meta {…,"errcode":"M_FORBIDDEN","status":401}   data {"flows":…,"session":"xYz","completed":[],"errcode":"M_FORBIDDEN",…}
```

- client 怎麼分辨「要 UIAA」和「沒登入」：**走橋的回覆（bit4）、status 401、data 有 `flows`** ＝ UIAA 挑戰；沒登入的 401 有 `errcode` `M_MISSING_TOKEN`／`M_UNKNOWN_TOKEN`、沒有 `flows`。→ **決定 2**（要不要在 meta 另加一個旗標，省得 client 解 data）。
- `session` 由 server 存。已登入的這幾支以「這個帳號、這個裝置」為鍵，所以同一個裝置的另一條連線也接得上（實作時 e2e 驗）。
- **對連線的影響**：
  - 停用帳號、刪掉**自己這個**裝置之後，這條連線的 token 就失效。回覆照樣先送到，**下一個 pack** 被 `revalidate` 擋下並關連線（批 1 §1.2 那一道）。
  - 改密碼的 `logout_devices` 保留發請求的那個裝置（HTTP 本來的行為），所以這條連線不受影響。
- 密碼猜測的限速：跟 HTTP 的 UIAA 一樣（同一個 `auth_uiaa`），橋不另加也不減。

#### 2-C. 這一批的端點

| kind | subtype | 名稱 | 端點 | 變數 | data |
|---|---|---|---|---|---|
| `0x10 Session` | `0x20` | Register | `POST /register` | query `kind` | JSON：`/register` 的 body（建議 `inhibit_login: true`） |
| | `0x21` | UsernameAvailable | `GET /register/available` | query `username` | — |
| | `0x22` | RegistrationTokenValidity | `GET /_matrix/client/v1/register/m.login.registration_token/validity` | query `token` | — |
| | `0x23` | LoginTypes | `GET /login` | — | — |
| `0x11 Account` | `0x2C` | ChangePassword | `POST /account/password` | — | JSON：`new_password`、`logout_devices`、`auth` |
| | `0x2D` | Deactivate | `POST /account/deactivate` | — | JSON：`erase`、`auth` |
| `0x16 Device` | `0x23` | DeleteDevice | `DELETE /devices/{device_id}` | `device_id` | JSON：`auth` |
| | `0x24` | DeleteDevices | `POST /delete_devices` | — | JSON：`devices`、`auth` |

共 8 支。`0x10` 的 `0x01`–`0x03`（Login／Refresh／Logout）是原生的，橋的號碼照慣例從 `0x20` 起。`LoginTypes` 不是註冊，但 client 在登入畫面前要知道 server 支援哪些登入方式，順手放進來。

- 🔧 **橋要改的一處**：`RegistrationTokenValidity` 只有 v1 路徑（MSC3231 進規格時就是 v1），而 `shape_of` 現在只收 v3。改成「**有 v3 取 v3，沒有就取 ruma 列的最新穩定路徑**」，批 1 的 37 支全是 v3，不受影響；總表的端點欄照寫完整路徑。→ **決定 3**。
- 🔧 **補一條測試**（批 1 留下的待查，已查清楚）：`ip_source` 與信任網段是外層 Router 用 layer 放進請求的 extension（`router/layers.rs`），橋的 Router 沒有這些 layer、橋組的請求也只帶 `ConnectInfo`。所以走橋的呼叫在端點一定走「沒設 `ip_source`」的路：掃轉發 header（橋一個都不帶）→ `ConnectInfo`，而那個位址是**傳輸層已經照 `ip_source` 解析好的 client IP**。行為正確、client 偽造不了；測試把它釘住。

#### 2-D. e2e（沿用批 1 的規矩）

每支橋打一次、HTTP 打一次、結果一致；另外：
- 註冊的反向檢查：`allow_registration = false` 被拒、registration token 錯被拒、`system` 註冊不到（`M_USER_IN_USE`）、`forbidden_usernames` 被拒 —— 跟 HTTP 一模一樣。
- 完整流程：匿名 WS 連線 → `Register`（UIAA 兩輪）→ `Login` → `WhoAmI` 是新帳號；超過裝置名額的 `Login` 照樣 `TooManyConnections`。
- UIAA：第一輪拿到 `flows`＋`session`、密碼錯拿到 `M_FORBIDDEN` 且 `session` 不變、第二輪成功；刪掉自己的裝置後下一個 pack 被拒並關連線。

### 批 3：房間其餘、關聯與討論串、在線狀態／filter／capabilities、檢舉（20 支，✅ PR #77，2026-09-21 合併）

維護者 2026-09-20 從候選清單裡挑的範圍：**C（房間其餘）＋ D（關聯與討論串）＋ E（在線狀態／filter／capabilities）**，共 20 支，並指定順便補齊 `Hello` 的 `features`（3-C）。
17 支落在**分配表（[wbf-wire-format.md](wbf-wire-format.md) §3.3）已經給過號的 kind** 裡，**唯一的新 kind 是 `0x1D Report`**（維護者 2026-09-20 決定：三支檢舉放一起，決定 1）—— 除此之外不需要新的 wire 格式，每一支就是總表一列、`BRIDGED_ENDPOINTS` 一列、範例檔一段、e2e 跟 HTTP 比一次。
⚠️ **每一支的 kind 照 §3.3 的分配表走，不是挑一個看起來像的**：presence 在 `0x15 Receipt`、filter 在 `0x12 Sync`、capabilities 在 `0x1C Misc`（這三支提案初稿曾經整堆塞進 `0x11 Account`，跟分配表不合，已改）。`0x12` 與 `0x1C` 這一批是**第一次被用到**，號碼本來就在表上。
🔲 **不在這一批**：推播與通知 12 支（要開新 kind `0x18`）、目錄與搜尋 4 支（要先決定算哪個 kind）—— 留在下面的候選清單等維護者再挑。

#### 3-A. 這一批的端點

`0x13 Room` 從 `0x2C DeleteAlias` 之後續號：

| subtype | 名稱 | 端點 | path 變數 | query 變數 |
|---|---|---|---|---|
| `0x2D` | Upgrade | `POST /rooms/{room_id}/upgrade` | `room_id` | — |
| `0x2E` | Knock | `POST /knock/{room_id_or_alias}` | `room_id_or_alias` | `via`、`server_name` |
| `0x2F` | JoinedMembers | `GET /rooms/{room_id}/joined_members` | `room_id` | — |
| `0x30` | GetVisibility | `GET /directory/list/room/{room_id}` | `room_id` | — |
| `0x31` | SetVisibility | `PUT /directory/list/room/{room_id}` | `room_id` | — |
| `0x32` | Summary | `GET /_matrix/client/v1/room_summary/{room_id_or_alias}` | `room_id_or_alias` | `via` |
| `0x33` | Hierarchy | `GET /_matrix/client/v1/rooms/{room_id}/hierarchy` | `room_id` | `from`、`limit`、`max_depth`、`suggested_only` |
| `0x34` | MutualRooms | `GET /_matrix/client/v1/mutual_rooms` | — | `user_id`、`from` |

`0x14 Event` 從 `0x25 Context` 之後續號：

| subtype | 名稱 | 端點 | path 變數 | query 變數 |
|---|---|---|---|---|
| `0x26` | Relations | `GET /_matrix/client/v1/rooms/{room_id}/relations/{event_id}` | `room_id`、`event_id` | `from`、`to`、`dir`、`limit`、`recurse` |
| `0x27` | RelationsByRelType | `GET /_matrix/client/v1/rooms/{room_id}/relations/{event_id}/{rel_type}` | 再加 `rel_type` | 同上 |
| `0x28` | RelationsByRelTypeAndEventType | `GET /_matrix/client/v1/rooms/{room_id}/relations/{event_id}/{rel_type}/{event_type}` | 再加 `event_type` | 同上 |
| `0x29` | Threads | `GET /_matrix/client/v1/rooms/{room_id}/threads` | `room_id` | `from`、`include`、`limit` |

`0x15 Receipt` 從 `0x22 Receipt` 之後續號（分配表 §3.3：presence 屬這一類）：

| subtype | 名稱 | 端點 | path 變數 | query 變數 |
|---|---|---|---|---|
| `0x23` | GetPresence | `GET /presence/{user_id}/status` | `user_id` | — |
| `0x24` | SetPresence | `PUT /presence/{user_id}/status` | `user_id` | — |

**`0x12 Sync`（這一批第一次用到）** —— 分配表 §3.3 把 filter 歸在這裡（跟 `/sync` 同一章）；`/sync` 本身不搬（見「不搬」那張表），但 filter 是獨立的端點：

| subtype | 名稱 | 端點 | path 變數 | query 變數 |
|---|---|---|---|---|
| `0x20` | GetFilter | `GET /user/{user_id}/filter/{filter_id}` | `user_id`、`filter_id` | — |
| `0x21` | CreateFilter | `POST /user/{user_id}/filter` | `user_id` | — |

**`0x1C Misc`（這一批第一次用到）**：

| subtype | 名稱 | 端點 | path 變數 | query 變數 |
|---|---|---|---|---|
| `0x20` | Capabilities | `GET /capabilities` | — | — |

**`0x1D Report`（新 kind）** —— 沒有原生的 subtype，`0x20` 起就是橋的：

| subtype | 名稱 | 端點 | path 變數 | query 變數 |
|---|---|---|---|---|
| `0x20` | ReportEvent | `POST /rooms/{room_id}/report/{event_id}` | `room_id`、`event_id` | — |
| `0x21` | ReportRoom | `POST /rooms/{room_id}/report` | `room_id` | — |
| `0x22` | ReportUser | `POST /users/{user_id}/report` | `user_id` | — |

📌 **號碼取 `0x1D` 的理由**：分配表（[wbf-wire-format.md](wbf-wire-format.md) §3.3）本來把 `report` 塞在 `0x1C Misc`，那是**只有一支**（檢舉事件）的年代；`0x1D`–`0x1F` 保留給「Matrix 尚未引進的章節」，而檢舉正好就是後來才長成獨立主題的 —— 檢舉房間 MSC4151 進規格 1.13、檢舉使用者 MSC4260 進 1.14。所以這一批要同時：
- 在 §3.3 加一列 `0x1D Report`，並把 `report` 從 `0x1C Misc` 那列拿掉（**同一件事只留一份**，兩份表遲早漂移）。這兩改已經在這份提案裡做了。
- `src/core/wbf/pack.rs` 的 `Kind` 枚舉加 `Report = 0x1D`，並把 `Misc` 那一行注解裡的 `reports` 拿掉。
- 新增三個範例檔：[../bridge-specs/0x1D-report.md](../bridge-specs/0x1D-report.md)、`0x12-sync.md`、`0x1C-misc.md`（後兩個 kind 這一批第一次被用到），總表 `index.md` 的目錄也要指過去。
📎 subtype 照規格裡出現的順序排：事件（1.0 就有）、房間（1.13）、使用者（1.14）。

#### 3-B. 三件先查清楚的事（都已對著 pin 住的 ruma 查過）

1. **每一支都有穩定路徑**，所以 `shape_of` 都拿得到（批 2 已經把「有 v3 取 v3，沒有取最新穩定路徑」做進 `pick_path`）。
   其中 **8 支不是 v3**：Summary（v1，1.15）、Hierarchy（v1，1.2）、MutualRooms（v1，1.19）、Relations 三支與 Threads（v1，1.3／1.4）。總表的端點欄照批 2 的規矩寫完整路徑。
2. **同一個端點的舊 URL 不另給號**（✅ 維護者 2026-09-20 同意）。⚠️ 這條講的是 **HTTP 路徑**，不是訊息內容 —— 橋從頭到尾不看 body（§2.2），`data` 進去是什麼 bytes 出來就是什麼 bytes，訊息裡的任何擴充欄位橋都不碰。
   有兩支在 ruma 上掛著兩條路徑，是 MSC 還沒定案時的舊 URL：房間摘要的 `/_matrix/client/unstable/im.nheko.summary/summary/{room_id_or_alias}`（`get_room_summary_legacy`）與 MutualRooms 的 `/_matrix/client/unstable/uk.half-shot.msc2666/user/mutual_rooms`。**同一個 route 函式、同一個回應，只是 URL 不同**，所以橋的表只放穩定的那一條（`pick_path` 本來就這樣挑），舊 URL 在 HTTP 上照舊在，沒有人被斷掉。
3. **認證等級照端點自己的**：Summary 是 `AccessTokenOptional`、GetVisibility 是 `NoAccessToken`，其餘都是 `AccessToken`。橋本來就不看這件事（它只在有 session 時帶上 token），所以匿名連線送 GetVisibility 會跟匿名 HTTP 拿到一樣的東西 —— 這是既有行為，批 1 的 e2e13 [1.13] 已經驗過同一條路。
   📎 限速也一樣是 HTTP 那一份：Knock、ReportRoom、ReportUser、Hierarchy、Threads、SetPresence、Capabilities、MutualRooms 在端點上標了 `rate_limited`，走橋不會繞過它。

#### 3-C. 順手補 `Hello` 的 `features`（維護者 2026-09-20 指定）

現在是 `["upload","download","recent","batch","seq","attachments","login","push","org.wbftw.device_versions"]` ——
**草稿（`0x02 Stream`，#45）、to-device（`0x16 Device`，#43）、還有整座橋**都沒有出現過。靠 `features` 偵測能力的 client 會以為這台 server 不支援它們（roadmap §2.9 記的缺口）。

補三個字串：**`"stream"`、`"device"`、`"bridge"`**（✅ 維護者 2026-09-20 決定：一個能力一個字串）。

- 🚫 **不是一批一個字串**（`bridge-batch-3` 之類）：能力表是給 client 問「有沒有這條路」，不是給它問「你做到第幾批」。某一支在不在橋上，client 送出去就知道 —— 不在表裡是 `UnknownKind`／`Unsupported`，那一條本來就要處理。
- 這是**線上契約的增補**（只加字串，舊 client 不受影響），但名字要跟 client repo 對一次再寫死。

#### 3-D. e2e（沿用批 1 的規矩）

每支橋打一次、HTTP 打一次、結果一致（e2e13 新增情境 5）；另外：

- **要有內容才比得出東西**：Relations 三支與 Threads 得先在房間裡送出一串有 `m.relates_to` 的事件（回覆、編輯、`m.thread`），不能只驗空清單。
- **反向檢查**（批 1 的規矩：關卡要驗，不能只驗正常路徑）：被暫停的帳號 `Knock`、`Upgrade`、`SetVisibility` 被拒；不在房間裡的帳號 `JoinedMembers`、`Hierarchy` 被拒 —— 跟 HTTP 一模一樣。
- **新 kind `0x1D` 的防呆**：`0x1D` 不帶 bit4（它沒有原生的 subtype）要是 `UnknownKind`、帶 bit4 但 subtype 不在表上也是 `UnknownKind` —— 跟其他 kind 同一條路，不是新規則。
- **`features`**：`Hello` 的回覆含這三個新字串（一條單元測試釘住字串本身，避免之後有人改字沒改文件）。

### 批 4：推播、目錄與搜尋，加上補洞（21 支，2026-09-21 提案）

維護者 2026-09-21：**🔴 與 🟡 都做**。起因是清點「常用的基礎還缺什麼」—— 拿 `src/api/router.rs` 註冊的 **177 個 client 端點**逐一對橋的 86 列，剩下的裡面只有兩塊是使用者會直接感覺到的（推播設定、找人找房找訊息），其餘是既有領域裡的小洞與明確不搬的那些。

⭐ **這一批沒有要維護者決定的 kind 歸屬**：每一支的 kind 都直接來自分配表（[wbf-wire-format.md](wbf-wire-format.md) §3.3），號碼早就留好了。批 3 的教訓是「挑一個看起來像的」會挑錯，所以這裡一律照表。
開四個新 kind：`0x18 Push`、`0x19 Media`、`0x1A Search`、`0x1B Voip`。四個都**沒有原生 subtype**，所以不帶 bit4 一律 `UnknownKind`，跟 `0x1D Report` 同一條路，不是新規則。

#### 4-A. 推播與通知：新 kind `0x18 Push`（12 支）

分配表 §3.3：`0x18 Push` ＝ pushers、push rules、notifications（`push/`）。

| subtype | 名稱 | 端點 | path 變數 | query 變數 |
|---|---|---|---|---|
| `0x20` | GetPushRules | `GET /pushrules/` | — | — |
| `0x21` | GetGlobalPushRules | `GET /pushrules/global/` | — | — |
| `0x22` | GetPushRule | `GET /pushrules/global/{kind}/{rule_id}` | `kind`、`rule_id` | — |
| `0x23` | SetPushRule | `PUT /pushrules/global/{kind}/{rule_id}` | `kind`、`rule_id` | `before`、`after` |
| `0x24` | DeletePushRule | `DELETE /pushrules/global/{kind}/{rule_id}` | `kind`、`rule_id` | — |
| `0x25` | GetPushRuleEnabled | `GET /pushrules/global/{kind}/{rule_id}/enabled` | 同上 | — |
| `0x26` | SetPushRuleEnabled | `PUT /pushrules/global/{kind}/{rule_id}/enabled` | 同上 | — |
| `0x27` | GetPushRuleActions | `GET /pushrules/global/{kind}/{rule_id}/actions` | 同上 | — |
| `0x28` | SetPushRuleActions | `PUT /pushrules/global/{kind}/{rule_id}/actions` | 同上 | — |
| `0x29` | GetPushers | `GET /pushers` | — | — |
| `0x2A` | SetPusher | `POST /pushers/set` | — | — |
| `0x2B` | Notifications | `GET /notifications` | — | `from`、`limit`、`only` |

📎 **`scope` 不是變數**：規格的路徑是 `/pushrules/{scope}/…`，但 ruma 這版把 scope 釘死成 `global`（只有 `global` 有意義），所以模板裡只有 `kind` 與 `rule_id`。
📎 **前兩支的路徑結尾有斜線**（`/pushrules/`、`/pushrules/global/`），跟 `state_key` 為空那條一樣是**真的路徑**，不是筆誤。

#### 4-B. 目錄與搜尋：`0x13 Room` 續號 ＋ 新 kind `0x1A Search`（4 支）

分配表 §3.3 已經回答了批 3 留下的那個問題：**Room 那一列本來就含 `directory`**，而 **`0x1A Search` ＝ search、user directory**。所以不必另開 `0x19 Directory`（`0x19` 是 Media，早就分配掉了）。

| kind | subtype | 名稱 | 端點 | 變數 |
|---|---|---|---|---|
| `0x13` | `0x35` | PublicRooms | `GET /publicRooms` | query `limit`、`since`、`server` |
| `0x13` | `0x36` | PublicRoomsFiltered | `POST /publicRooms` | query `server`；data 是過濾條件 |
| `0x1A` | `0x20` | SearchEvents | `POST /search` | query `next_batch`；data 是查詢 |
| `0x1A` | `0x21` | SearchUsers | `POST /user_directory/search` | data：`search_term`、`limit` |

#### 4-C. 補洞：既有領域裡漏掉的幾支（5 支）

這幾支不成一類，但都落在**已經搬過的領域**裡，缺了就是 client 得為了一支掉回 HTTP。

| kind | subtype | 名稱 | 端點 | 變數 |
|---|---|---|---|---|
| `0x13` | `0x37` | RoomAliases | `GET /rooms/{room_id}/aliases` | `room_id` |
| `0x14` | `0x2A` | TimestampToEvent | `GET /_matrix/client/v1/rooms/{room_id}/timestamp_to_event` | `room_id` ／ query `ts`、`dir` |
| `0x19` | `0x20` | MediaConfig | `GET /_matrix/client/v1/media/config` | — |
| `0x19` | `0x21` | MediaPreview | `GET /_matrix/client/v1/media/preview_url` | query `url` |
| `0x1B` | `0x20` | TurnServer | `GET /voip/turnServer` | — |

⚠️ **`0x19 Media` 這個 kind 是「相容路徑」，不是 fork 的媒體通道**：分塊上傳／下載是原生的 `0x03 Upload`／`0x04 Download`，這兩支只是「這台 server 的媒體上限是多少」與「給我這個連結的預覽」。
📎 這兩支走的是**認證媒體**的路徑（`/_matrix/client/v1/media/…`，MSC3916 已進規格 1.11），不是已經棄用的 `/_matrix/media/v3/…` —— tuwunel 的 handler 用的就是前者。

#### 4-D. 🚫 帳號資料的「刪」搬不了，要維護者決定

`0x11 Account` 有 `GetAccountData`／`SetAccountData` 與房間版兩支，但**刪**的那兩支（`DELETE …/account_data/{event_type}`、房間版）在這版 ruma 裡**只有 unstable 路徑**（`org.matrix.msc3391`，MSC 還沒進規格）。`pick_path` 只收 v3 與較新的穩定版本，所以 `shape_of` 對它們回 `None` —— **現在的橋搬不了**。

所以「能讀能寫不能刪」不是我們漏搬，是 Matrix 還沒把它定案。→ **決定 1**（§5）。

#### 4-E. e2e（沿用批 1 的規矩）

每支橋打一次、HTTP 打一次、結果一致（e2e13 新增情境 6）；另外：

- **推播規則要真的改到東西**：設一條規則 → HTTP 讀得到它 → 改 `enabled` 與 `actions` → 刪掉之後 HTTP 也讀不到。只驗「回 200」沒有意義。
- **`before`／`after` 要驗順序**：插一條規則在另一條之前，讀回來的順序要跟 HTTP 讀到的一致 —— 這是這兩個 query 變數唯一看得出來的效果。
- **搜尋要有東西可搜**：先送幾則訊息、建一個公開房間，再比對；不是驗空結果。
- **四個新 kind 的防呆**：`0x18`／`0x19`／`0x1A`／`0x1B` 不帶 bit4 都要是 `UnknownKind`。
- **形狀要釘**（批 3 的教訓，[5.7b] 那種）：範例檔宣稱的回應形狀直接斷言，不能只靠「兩條路一致」—— 兩條路回的是同一個形狀，文件寫錯照樣全綠。

### 不搬（至少這一輪）

| 端點 | 為什麼 |
|---|---|
| `/sync` | long-poll 在 WS 上的形狀是 server 推送，已經是 `Subscribe`／`Push`；另一件事 |
| `/messages`（翻歷史） | `Event/Recent` 帶 `rooms` ➕ `before` 就是單房歷史（PR #51），同一個問題不要兩個入口 |
| SSO、JWT、LDAP、appservice 登入 | 要瀏覽器或是給服務用的 |
| 舊的整檔媒體、縮圖 | 已經有 `Upload`／`Download` |
| 3pid（綁 email／手機）、OpenID、dehydrated device、QR 登入（rendezvous）、第三方橋接協定 | client 日常用不到；要的時候再排，不用先占號 |
| admin 端點（鎖定、暫停、admin 註冊） | 管理者用，不是 client 的日常路徑；`0x20 Admin` 還沒開 |
| `/rooms/{id}/initialSync`、`/events` | 規格已棄用，`Recent`／`Subscribe` 已經取代 |

📎 keys、backup、cross-signing（`0x17`）本來也在這一列，2026-09-17 起已經搬完（[wbf-e2ee.md](wbf-e2ee.md)）。

## 4. 會不會跟正在進行的 PR 撞

`wbf/recent-erasure`（#54）改的是 `wbf/recent.rs` 的 `collect_window`、兩份設計文件、e2e8、CHANGELOG。這個提案要動的是 `wbf/mod.rs` 的派發、新檔 `wbf/bridge.rs`、`router/router.rs` 一行、wire-format §3.2／§3.3／§3.4、pipeline §7 —— **沒有同一個檔案的同一段**。CHANGELOG 照慣例合併後才寫，不會同時改。實作分支等這份文件同意之後才開，從當時的 main 開。

## 5. 要維護者決定的

1. ~~**橋，還是手搬？**~~ ✅ **定了**（維護者 2026-09-14）：用橋，而且是**把 pack 轉成內部的 HTTP request 丟進 Router**這一版（§2.1）。上游檔案一個都不用動。
2. ~~**meta 三段分開還是攤平？**~~ ✅ **都不是**（維護者 2026-09-14）：body 根本不進 meta —— **data 就是 body**，meta 只放模板的變數（§2.2）。撞名問題因此消失；五條規則與「一個 subtype 對一個端點」也一併定了。
3. ~~**`Error` 多帶 Matrix `errcode`？**~~ ✅ **要，放在 meta**（維護者 2026-09-14，§2.4）。線上格式的增補，client 要跟。
4. ~~**橋上的操作准走 HTTP pack？**~~ ✅ **橋不看傳輸層**（維護者 2026-09-14，§2.5）：WS 與 HTTP 傳的都是 pack，走不走橋只看 bit4。「HTTP 不可」留在原生 handler 的准入表。
5. ~~**批 1 的清單要增要減？**~~ ✅ **這批就夠**（維護者 2026-09-14）。方向是**之後幾乎全部搬過去**，不必一個 commit 全上，一批一批來、常用的先上。
6. ~~**subtype 號什麼時候給？**~~ ✅ **開始寫程式時才補進文件**（維護者 2026-09-14）。照 Matrix 規格章節裡端點出現的順序排；分配了就不改（§3.3）。粒度（一個 subtype 對一個端點）已定。
   📌 維護者 2026-09-14 開工時指定落點：**號碼寫在 [../bridge-specs/index.md](../bridge-specs/index.md) 的總表**（不是 wire-format §3.2），每一批的詳細範例寫在同目錄的 kind 檔。wire-format §3.3 指過去。

✅ **維護者 2026-09-14 給了開工訊號**：分支 `wbf/api-bridge`。順序是先寫 [../bridge-specs/index.md](../bridge-specs/index.md) 的總表 → 寫橋的那一層 → 才真的搬。

### 批 2 要維護者決定的（2026-09-15 提案）

✅ **維護者 2026-09-15「提案同意」：五條都照建議**—1 (a) 照 HTTP 放行、2 (a) 不加旗標、3 照做、4 (a) 不改時限、5 這 8 支。

1. **沒帶 `inhibit_login` 的註冊怎麼辦？**（§3 批 2-A）
   - (a) 照 HTTP 放行：帳號建好、token 在 data，連線不變。橋照舊不看 body。
   - (b) 橋對 `Register` 這一列特別檢查 body，沒帶 `inhibit_login: true` 就 `InvalidRequest`。
   - 建議 **(a)**：跟 HTTP 行為一致、不危險；橋一旦開始為某一列看 body，就是第二套規則。範例檔寫清楚「要帶 `inhibit_login: true`，再送 `Login`」。
2. **UIAA 挑戰要不要在 meta 另加旗標？**（§3 批 2-B）
   - (a) 不加：client 看「bit4 ＋ status 401 ＋ data 有 `flows`」。
   - (b) meta 多一個 `uiaa: true`。
   - 建議 **(a)**：失敗回覆的 data 本來就是 Matrix body，client 要拿 `session` 一定得解；多一個欄位是同一件事的第二份。
3. **只有 v1 路徑的端點**：`shape_of` 改成「有 v3 取 v3，沒有取最新穩定路徑」（§3 批 2-C）。建議照做。
4. **註冊超過 30 秒的匿名時限**（§3 批 2-A）：
   - (a) 不改時限，client 重連、帶同一個 UIAA `session` 接著送。
   - (b) 沒登入的連線每送一個走橋的 pack 就延長時限。
   - 建議 **(a)**：時限存在是為了不讓匿名連線佔著資源；讓匿名連線自己延長，就等於沒有時限。
5. **批 2 的號碼與清單**（§3 批 2-C 那 8 支）要增要減？

### 批 3 要維護者決定的（2026-09-20 提案，✅ 當天三條都定了）

範圍由維護者挑：C＋D＋E 共 20 支、`features` 一起補。另外三條：

1. **`ReportUser` 放哪個 kind？**（§3 批 3-A）
   - (a) `0x11 Account`（回報的對象是一個使用者，帳號那一類最接近）。
   - (b) 新開一個「檢舉」kind，三支回報放一起。
   - ✅ **維護者選 (b)**（提案原本建議 (a)）。→ 新 kind **`0x1D Report`**，三支 `0x20`–`0x22`；分配表 §3.3 加一列，`0x1C Misc` 那列的 `report` 拿掉。
2. **`features` 補哪幾個字串？**（§3 批 3-C）
   - (a) `"stream"`、`"device"`、`"bridge"` —— 一個能力一個字串。
   - (b) 再加一個「批號」字串（例 `"bridge3"`），client 可以問到搬了幾批。
   - ✅ **維護者選 (a)**：某一支在不在橋上，送出去就知道（`UnknownKind`／`Unsupported` 本來就要處理）；批號會在每一批之後變成要維護的第二份清單。
   ⚠️ 字串的名字要先跟 client repo 對一次 —— 這是線上契約。
3. **同一個端點的舊 URL 要不要也給一個 subtype 號？**（§3 批 3-B 第 2 點）
   ✅ **維護者 2026-09-20：不做**。橋的表只放穩定路徑那一條，`im.nheko.summary` 與 `uk.half-shot.msc2666` 兩條舊 URL 在 HTTP 上照舊，橋上沒有號碼。
   📎 維護者同時重申了橋的界線：**「如果只是個 msg extension，你就維持原樣就好，bridge 不管內容是什麼，只管怎麼原樣 forward。」** 這跟 §2.2 已經定的一致 —— 橋不看 body，這一條講的是 HTTP 路徑，不是訊息內容。

### 批 4 要維護者決定的（2026-09-21 提案）

範圍維護者 2026-09-21 已經定了（“🔴🟡都做”），kind 歸屬全部照 §3.3 的分配表，**沒有另外要挑的**。只剩一條：

1. **帳號資料的「刪」只有 unstable 路徑，要不要為它放寬 `pick_path`？**（§3 批 4-D）
   - (a) **不做**：這一批不搬，等 MSC3391 進規格、ruma 給出穩定路徑再說。client 要刪帳號資料就掉回 HTTP（或者照 Matrix 的做法寫空值 `{}`）。
   - (b) **放寬**：`pick_path` 再加一層後備 —— 沒有 v3、也沒有任何穩定版本時，取 unstable 那條。兩支因此上得了橋，MSC 一進規格就會**自動**改走穩定路徑（表指的是型別，不是字串）。
   - 建議 **(a)**：“一個 subtype 對一個端點”的前提是那個端點的形狀定了。MSC 還在動的時候把它放上線，等於拿一個永久的號碼押一個還會改的東西；而「不能刪」的代價是一支接口掉回 HTTP，不是功能不見了。
   ⚠️ 選 (b) 的話要記得：放寬之後，**任何只有 unstable 路徑的端點都會變成“橋搬得了”**，這條規則不只管這兩支。

## 6. 同意之後的落點

| 做什麼 | 落點 |
|---|---|
| 橋自己的 Router（`tuwunel_api::router::build` 組一份，不掛 middleware）、分配表、meta ↔ request／ response 轉換 | `src/api/client/wbf/bridge.rs`（新） |
| 建橋的 Router 並用 `Extension` 掛在對外的 Router 上（`State` 是那邊建的） | `src/api/router.rs` 的 `BridgeRouter`、`build_bridge_router`；`src/router/router.rs` 掛上；`wbf/mod.rs` 與 `ws.rs` 取出來放進 `PackContext` |
| 上游的 `router/args.rs`、`auth.rs` | **不動** |
| 錯誤對應補齊、`errcode` | `src/api/client/wbf/mod.rs` 的 `Reject::from(Error)`；wire-format §3.4 |
| 派發 | `wbf/mod.rs` 的 `dispatch`：看 bit4 分兩條路（§2.3），不看 kind |
| 契約 | [bridge-specs/index.md](../bridge-specs/index.md) 每支一列（wire-format §3.2 只列原生的），每個 kind 一份 `KIND.md` 寫範例；pipeline §7 標明一般端點走橋 |
| 向量、e2e | `core/wbf/vectors.rs`；新腳本 `tests/e2e/e2e13.ps1` |
