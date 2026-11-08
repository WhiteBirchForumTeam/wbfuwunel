# 會話 token 的兩個洞：refresh token 可以當 access token 用、重新登入不收舊 token

來源是外部審查的 S1 與 S4。2026-11-08 讀程式 ＋ **對真的 server 實跑**確認兩條都成立 ——
下面每一節都附實跑的輸出，而不是審查的敘述（轉述一條審查意見不等於確認它成立；同一批
審查裡的 S2 讀完就不成立）。

| | 狀態 |
|---|---|
| §2 S1：refresh token 直接當 access token 用 | ✅ **成立**（實跑 200）⇒ ✅ **已修**（維護者 2026-11-09 核可，§2.3 的兩張表） |
| §3 S4：同一個 `device_id` 重新登入，舊的 access ／ refresh token 都還活著 | ✅ **成立**（實跑 200），而且是 **upstream 刻意的設計** ⇒ ✅ **照選項 A 修**（維護者 2026-11-09） |

⭐ 兩條的共同根因只有一句：**「這個字串是不是一把 access token」是用「它在不在
`token_userdeviceid` 這張表裡」回答的，而 refresh token 也寫在那張表裡。**

## 1. 一把 token 今天怎麼被認得

```
Authorization: Bearer <token>
  → api/router/auth.rs 的 find_token
      → users.find_from_token(token)
          → 讀 token_userdeviceid：token → (user, device, expires_at)
```

而 `users/device.rs` 的 `set_refresh_token` 裡有這一行：

```rust
txn.raw_put(&self.db.token_userdeviceid, refresh_token, value);
```

⇒ refresh token 跟 access token **是同一個命名空間**。`classify_refresh_token` 自己的註解
就寫明了它靠這點工作：「The current refresh token resolves and matches the device's active
pointer (an access token resolves but will not match)」—— 它用「解析得到 ＋ 指標對得上」
來分辨兩者，所以**「解析得到」對兩種憑證都成立**，而認證那一側只問了前半。

### 1.1 兩種憑證的定義（這一節是後面兩個洞的判準）

| | access token | refresh token |
|---|---|---|
| 用在哪 | **每一個** API 請求的 `Authorization: Bearer` | **只有** `/refresh`（與 OAuth 的 token 端點） |
| 權限 | 這個裝置的全部 | 只能做一件事：換一組新的 token |
| 每次使用後 | 不變 | **輪替**（舊的退成 `spent`，留一代） |
| 該不該長命 | ❌ 不該：`access_token_ttl` 預設 7 天 | ⭐ **可以，而且預設就是永不過期**（`refresh_token_ttl` 預設 0）|
| 偷到之後 | 7 天內的全權，過期自己失效 | 可以一直換新的 —— 但**每次換都輪替** ⇒ 合法裝置下一次 refresh 會拿到「已經被用過」的答案，`refresh_token_reuse_revoke` 收掉整個裝置 ⇒ **看得見** |

⭐ **所以「refresh token 不過期」本身是合理的設計，不是洞**（維護者 2026-11-08 問到）：
一台裝置要能一直登著而不必保存密碼，就需要一把長命的憑證；它敢長命，是因為它
**單一用途 ＋ 每次輪替 ＋ 重放看得出來**。真正不該長命的是 access token，而它確實只有 7 天。

🚨 **§2 的洞不是「它太長命」，是「它同時是一把 access token」**：長命 × 全權 × 不輪替
＝ 一把永久的全權金鑰，而且那套重放偵測永遠不會動（§2.2 第 2 點）。
⇒ 📎 §2.3 的修法**完全不動它的壽命**：refresh token 照樣永不過期，只是不能當 Bearer 用。

### 1.2 這兩個洞是 upstream 的，不是這個 fork 弄出來的

2026-11-08 逐行對過 `upstream/main`（`b89011524`）：

| 行為 | upstream | 本 fork | |
|---|---|---|---|
| `set_refresh_token` 把 refresh token 寫進 `token_userdeviceid` | ✅ 有（`device.rs` 的 `txn.raw_put(&self.db.token_userdeviceid, refresh_token, value)`） | 同 | **繼承** |
| 認證只讀那張表、沒有任何前綴檢查 | ✅ 同（`api/router/auth.rs` 的 `find_token`） | 同 | **繼承** |
| 一個裝置可以有多把 access token | ✅ **刻意**（commit `6726324c7`，2026-05-29：「A device may hold several access tokens」） | 同 | **繼承** |
| 既有 `device_id` 重新登入只 `set_access_token`、不刪舊的 | ✅ 同（`api/client/session/mod.rs`） | 同（包在 `issue_session` 裡） | **繼承** |
| 差別 | 有 device lock（`lock_device` ／ `issue_tokens`），refresh 輪替的結構不同 | `issue_session` ＋ `admit` 閘（wbf 通道共用） | 只是重構，**沒有改這兩個行為** |

⇒ **修這兩條都是這個 fork 主動偏離 upstream**，而且動到的是 upstream 還在改的檔
（`users/device.rs`）⇒ 以後 merge upstream 會在這裡撞。這是決定的一部分，不是細節。
📎 另一條路是把修法也送回 upstream；要不要送由維護者決定。

## 2. S1：refresh token 可以直接當 access token 用

### 2.1 實跑（2026-11-08，`target/e2e/tuwunel.exe`，全新資料庫）

```
login: expires_in_ms=604800000 has_refresh=True
[S1] whoami with the ACCESS token  = 200  {"user_id":"@alice:localhost","device_id":"DEVICEONE"}
[S1] whoami with the REFRESH token = 200  {"user_id":"@alice:localhost","device_id":"DEVICEONE"}
[S1] account_data PUT with the REFRESH token = 200  {}
[S1] /devices with the REFRESH token = 200
```

不是只有讀：**寫入也過**（`PUT /user/{id}/account_data/...`）。refresh token 在認證這一側
就是一把完整的 access token。

### 2.2 為什麼這比「多一把 token」嚴重

1. 🔴 **它把一把「刻意長命的單一用途憑證」變成一把「永不過期的全權金鑰」。**
   `access_token_ttl` 預設 7 天，`refresh_token_ttl` 預設 0（永不過期）—— ⭐ 而後者
   **是對的**（§1.1）：單一用途 ＋ 每次輪替 ＋ 重放看得見，所以它敢長命。
   🚨 洞在於**那三個前提在「當 access token 用」這條路上一個都不成立**：不是單一用途
   （全部端點）、不會輪替（當 Bearer 用不會碰 `/refresh`）、看不見（下一點）。
   ⇒ 於是 client 乖乖輪替 7 天期的 access token，而同一個 session 手上還有一把
   **永久的**全權憑證。
2. 🚨 **重用偵測整套被繞過。** 這個 fork 為 refresh token 做了相當完整的防護 ——
   spent token 保留一代、`refresh_token_reuse_grace`（預設 15 秒）分辨善意的重送、
   `refresh_token_reuse_revoke` 在真的重放時收掉裝置（`users/device.rs` 的
   `classify_refresh_token`）。⭐ **但那一整套只在有人呼叫 `/refresh` 時才會動。**
   偷到 refresh token 的人**不需要**呼叫它：直接當 access token 用就好，於是
   token 永遠不輪替、`spent` 永遠不產生、合法的 client 照常自己輪替、
   **沒有任何一個訊號會亮**。偵測機制在的時候比不在更危險 —— 它讓人以為有在看。
3. 📎 **wbf 通道同一個洞**：`api/client/wbf/mod.rs` 的 `check_token` 也是
   `find_from_token`，所以一把 refresh token 也能開一條通道。

### 2.3 提案的修法：兩種憑證，兩張表

⚠️ **不能只在 `find_from_token` 裡擋 `refresh_` 前綴** —— 有兩個地方**依賴**它解析
refresh token：

| 誰 | 為什麼需要解析 refresh token |
|---|---|
| `classify_refresh_token` | 它就是 `/refresh` 的入口 |
| `api/oidc/revoke.rs` | RFC7009 的撤銷端點**本來就該**同時接受 access 與 refresh token（而且 `token_type_hint` 明列兩者） |

⇒ 要分的不是字串，是**表**：

```
token_userdeviceid        ← 只有 access token。認證只讀這張
refresh_userdeviceid（新） ← 只有 refresh token。/refresh 與撤銷讀這張
spentrefresh_userdeviceid ← 不動（上一代的 refresh token，重放偵測用）
```

- **認證那一側**（`find_from_token`，HTTP auth ／ wbf `check_token` ／ OIDC `userinfo`）
  只讀 `token_userdeviceid`。
- **`/refresh`**（`classify_refresh_token`）讀新表 ＋ spent 表。
- **撤銷**（RFC7009）兩張都查 —— 它的語意本來就是「這個字串是任一種 token」。
- `find_refresh_token_expires_at` 從新表的值讀期限（今天它去 `token_userdeviceid` 撈）。
- `remove_refresh_token` 改刪新表（它今天刪 `token_userdeviceid`，所以**不要漏掉**，
  否則撤銷會留下活著的列）。
- 🚨 **第二道（fail closed，CLAUDE.md A5）**：認證那一側**同時**拒絕
  `starts_with("refresh_")` 的字串。表分開之後它理論上不會命中，而那正是該留著的理由 ——
  它壞掉的方向是拒絕，不是放行；而且哪天有人又把 refresh token 寫進 access 表，
  是這一道會擋住。

📎 **不需要遷移**：這個 server 到 2026-11-08 從未真實上線過，沒有既有的 refresh token
要搬。⚠️ 這個理由**有期限**，第一次正式上線之後就不成立。

### 2.4 測試

1. **洞本身**：登入拿 refresh token → 當 Bearer 打 `whoami`、`/devices`、一個寫入端點
   → 三個都要是 401 `M_UNKNOWN_TOKEN`。
2. **沒有把 `/refresh` 弄壞**：同一把 refresh token 打 `/refresh` → 200，而且輪替後
   舊的那把再打一次 → 照既有規則（grace 內 200、grace 外 401）。
3. **沒有把撤銷弄壞**：RFC7009 用 access token 撤銷 → 200；用 **refresh token** 撤銷
   → 200，而且之後兩把都失效。
4. **wbf 通道**：拿 refresh token 開通道 → 被拒（跟 HTTP 同一個理由）。
5. 🔴 **第二道自己也要有測試**：直接把一把 `refresh_` 開頭的字串寫進 access 表，
   然後認證 → 要被拒。不然「第二道」只是一句註解。

### 2.5 ⏳ 跟著冒出來的一個問題：被撤銷的 refresh token 該回 401 還是 403

`/refresh` 拿到一把**不認識**的 refresh token 時回 **403 `M_FORBIDDEN`**
（`RefreshToken::Unknown` 那條，upstream 也是這樣），而「已經被用過」那條回
**401 `M_UNKNOWN_TOKEN`**。

🚨 **這支改完之後，「被撤銷」會落進前者**：§3 的重新登入、以及 RFC7009 的撤銷，都讓那把
refresh token 變成「不認識」⇒ client 收到 403。而 Matrix 規格的 `/refresh` 寫的是
**401 `M_UNKNOWN_TOKEN`**（「The provided token was unknown, or has already been used」），
而 401 ＋ `soft_logout: false` 正是在告訴 client「這個 session 沒了，重新登入」——
403 沒有這個意思，client 多半只會當成一個錯誤。

⇒ 📄 **建議把 `Unknown` 改成 401 `M_UNKNOWN_TOKEN`（`soft_logout: false`）**，理由跟 §3 選 A
一樣是「朝規格那一邊」，而且它是這支造成的後果 —— 但它同樣是 client 可見的改變，
所以**等維護者**。⏳ 在那之前 `tests/e2e/e2e18.ps1` 的 `[1.5c]`、`[2.2]` 只釘「被拒絕」
並把代碼印出來，答案定了就收成一個。

📎 **真的要改的話，跟著要動的還有一處**：`tests/e2e/e2e7.ps1` 的 `[4.8c]` 寫的預期就是
「Refresh with an unknown token → 403 `M_FORBIDDEN`」（2026-11-09 實跑還是那樣）。

## 3. S4：同一個 `device_id` 重新登入，舊 token 都還活著

### 3.1 實跑（同一次，接在 §2.1 之後）

```
login2: device=DEVICEONE expires_in_ms= has_refresh=False
[S4] whoami with the token from login 1 = 200  {"user_id":"@alice:localhost","device_id":"DEVICEONE"}
[S4] whoami with the token from login 2 = 200
[S4] whoami with login 1's REFRESH token after the re-login = 200
[S4] POST /refresh with login 1's refresh token after the re-login = 200
     {"access_token":"…","refresh_token":"refresh_…","expires_in_ms":604800000}
```

🚨 **最後那一行是這一節真正的重點**：重新登入之後，**上一個 session 的 refresh token
還能換到一組全新的 access ＋ refresh token** —— 它可以無限期地自己續命下去，而使用者
以為「我重新登入過了」。

📎 兩個順帶看到的事實：
- `login2` 沒有要求 refresh token ⇒ `expires_in_ms` 是**空的** ⇒ 那把新 token
  **永不過期**（`generate_access_token(false)`）。所以「重新登入」在預設 client 行為下
  產生的是一把長命 token，而舊的那些也還在。
- ✅ **`/logout` 是乾淨的**：三把（login 1 的 access、login 1 的 refresh、login 2 的
  access）實跑都變 401 —— 因為它走 `remove_device` → `remove_tokens`。壞的是**重新登入**
  這條路，不是登出。

### 3.2 為什麼：這是 upstream 刻意的設計

`users/login.rs` 的 `issue_session` 對**既有**裝置只呼叫 `set_access_token`，而那個函式
**加一把**（寫進 `token_userdeviceid` 與 `userdeviceidtoken_index`），**不刪任何舊的**。
而那個索引的來歷是 upstream 的一個 commit：

```
6726324c7  2026-05-29  Jason Volk
service/users: Allow multiple access tokens per device.

A device may hold several access tokens; a new (user, device, token)
index lets device removal enumerate and revoke every one.
```

（`git merge-base --is-ancestor` 確認它在 `upstream/main` 裡 ⇒ 我們繼承的，不是本 fork 的
決定。）⇒ 「一個裝置多把 access token」是**有意的**，索引就是為它加的。所以 S4
**不是一個意外**，是一個**設計衝突**。

### 3.3 規格怎麼說：它自己不一致

- **裝置那一節**（較強）：client 指定既有 `device_id` 時，server **will invalidate** 先前
  發給那個裝置的 access 與 refresh token。
- **`/login` 那一節**（較弱）：server **may** invalidate。
- **舊版 r0.6.1** 講得最死：「there is therefore at most one active access token assigned to
  each device at any one time」。
- 📎 **Synapse 有過同一個缺口**（matrix-org/synapse#6616：同一個 device_id 兩列 token 都
  有效），所以這不是只有我們這樣。

⇒ 照規格的**字面**，我們沒有違反 `/login` 那節的 `may`；照裝置那節，我們違反了。

### 3.4 要維護者決定的是什麼

問題不是「要不要修」，而是**「重新登入該不該結束上一個 session」**。四個選法：

| | 做法 | 代價 | 會不會把裝置鎖在門外 |
|---|---|---|---|
| **A** | 照規格的強版本：登入既有 `device_id` 時收掉該裝置先前的 access ＋ refresh token | 關掉 upstream 刻意留的「一裝置多把 token」；**同一個 `device_id` 真的有兩個 client 在跑**時，後登入的會把前一個踢掉 | ❌ **不會。** 剛登入的那個 client 當場拿到新的一組；被收掉的是同一個 `device_id` 的**上一個持有者**，它要重新登入（密碼）—— 而它本來就有密碼才登得進來 |
| **B** | 只收 **refresh token**：舊 access token 留著自己過期（7 天），但不能再自我續命 | 改得最小，擋掉 §3.1 最後一行那個無限續命；舊 access token 仍有最多 7 天的窗口。而沒要求 refresh 的登入給的是**永不過期**的 token，所以這條只對有用 refresh 的 client 有意義 | ❌ 不會，風險比 A 更小 |
| **C** | 設定開關（預設 A，可關回 upstream 行為） | 多一個設定、多一組測試；而「安全預設 ＋ 可退回」是這個 repo 已經有的模式（`refresh_token_reuse_revoke` 就是） | 同 A，而且有退路 |
| **D** | **不修**，接受 upstream 的行為，只修 §2 的 S1 | 「我重新登入了」不會讓舊 session 失效；要收掉只能靠 `/logout`（它是乾淨的） | 完全不會 |

#### 鎖在門外的風險，實際長什麼樣（維護者 2026-11-08 問到）

⭐ **A 不會讓任何裝置失去「回得來」的能力** —— 收掉 token 不等於收掉帳號：重新登入的那個
client 手上有密碼（它剛用密碼登入），而被收掉的那一個要回來也是用密碼。這跟
`/logout` 的處境完全一樣，而 `/logout` 本來就會把那把 token 收掉。

🚨 真正會痛的只有一種 client：**登入成功之後沒有改用新 token、繼續拿舊的那把用。**
那種 client 在 A 之下會突然 401。本專案的 client 不是這樣（`wbf-matrix-client`
每次登入都用回來的 `access_token` 開通道，而且它連 refresh token 都沒要求：
`backend/matrix_sdk.rs` 的 `refresh_token: None`）。

📎 反過來說，**不修（D）的代價是補救的路只剩 `/logout`**：使用者懷疑自己被盜時會做的事
是「改密碼、重新登入」，而那兩件在今天都不會讓偷走的 token 失效。
（改密碼帶 `logout_devices` 時會 —— 那條走 `remove_device`。）

⇒ **我的建議仍然是 A，而且不加開關。** upstream 那個索引不會白費：它正是 A 要用的東西
（枚舉一個裝置的每一把 token 然後全撤）。而「同一個 `device_id` 兩個 client」在這個 fork
裡站得住腳的機會很低 —— `device_id` 是 client 自己產生的，兩個 client 本來就該有兩個。

⚠️ 若選 A：這是**對 Matrix client 可見的行為改變**（雖然是朝規格那一邊），而且是
**主動偏離 upstream**（§1.2）⇒ 依 `matrix-compat-best-effort` 的約定要先讓維護者同意，
標 `[BREAKING]`。這一節就是在問。

### 3.5 測試（實際做的）

**整合測試**（`src/main/tests/session_token_separation.rs`，一支裡兩節，真 server ＋ 真資料庫）：

1. 同一個 `device_id` 登入兩次 → 第一次的 access token、以及**那次 refresh 換到的那把**，
   兩把都不再認證。
2. 🚨 **第一個 session 的 refresh token 不能再換一組**（§3.1 最後一行的反面）。
3. **另一個裝置的 token 仍然有效** —— 這條防的是「修過頭，把整個帳號的 token 都收了」。
4. 裝置還在：`device_exists` 仍然是 true（登入不是登出）。

**e2e**（`tests/e2e/e2e18.ps1`，情境 2 ＋ 情境 3）：同樣四件事走**公開 HTTP**，外加
通道那一半 —— refresh token 開不了連線；既有連線在重新登入後的**下一個 frame** 被拒。

📎 **通道那一半的語意寫在這裡**：這條路不走 `remove_device`，所以
`/docs/design/wire/session-teardown.md` §3.1 的「一個出口」在這裡**不適用**，而它不需要
適用 —— `revalidate` 每個 frame 都重問一次 token，所以連線在下一個 frame 自己結束。
⚠️ 代價跟鎖定帳號那條一樣：**不是立刻**，上限是 `wbf_ws_idle_timeout`（同 §4.3）。

### 3.6 變異驗證（2026-11-09，三個各自驗）

🚨 分開驗，因為同一支測試在第一個失敗就停 —— 一起弄壞只證明得了第一道：

| 弄壞什麼 | 紅在哪 |
|---|---|
| `find_from_token` 的前綴第二道改成 `false && …` | 「a refresh-prefixed token sitting in the access table authenticates」 |
| `issue_session` 裡的 `remove_tokens` 拿掉 | 「the access token from the first login still authenticates after a re-login on its device」 |
| `set_refresh_token` 寫回 `token_userdeviceid` | 「the refresh token is not in the refresh table, so /refresh cannot work」 |

⭐ 第三個特別值得記：它**不會**讓「refresh token 認證得了」那條紅，因為前綴那道會擋住 ——
那正是第二道存在的意義，而**分表本身**是由 `/refresh` 那條斷言釘住的。

## 4. 狀態

| | |
|---|---|
| 兩條都成立 | ✅ 讀程式 ＋ 實跑確認（§2.1、§3.1） |
| S1 的修法（§2.3） | ✅ **已實作**：兩張表 ＋ 認證側的前綴第二道 |
| S4 的修法（§3.4） | ✅ **已實作，選項 A**（維護者 2026-11-09）：登入既有 `device_id` 先收掉該裝置的 access ＋ refresh |
| 幾支 PR | ✅ **一支**：同一塊程式、同一組測試。client 可見的行為改變 ⇒ 標 `[BREAKING]` |
| 遷移 | ✅ **不需要**（server 從未真實上線，沒有既有的 refresh token 要搬）。⚠️ 理由有期限 |
| 要不要送回 upstream | ⏳ **等維護者**（§1.2：兩個洞都是繼承的，修法是主動偏離） |
