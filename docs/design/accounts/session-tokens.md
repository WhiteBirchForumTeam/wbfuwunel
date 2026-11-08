# 會話 token 的兩個洞：refresh token 可以當 access token 用、重新登入不收舊 token

來源是外部審查的 S1 與 S4。2026-11-08 讀程式 ＋ **對真的 server 實跑**確認兩條都成立 ——
下面每一節都附實跑的輸出，而不是審查的敘述（轉述一條審查意見不等於確認它成立；同一批
審查裡的 S2 讀完就不成立）。

| | 狀態 |
|---|---|
| §2 S1：refresh token 直接當 access token 用 | ✅ **成立**（實跑 200）。§2.4 是提案 |
| §3 S4：同一個 `device_id` 重新登入，舊的 access ／ refresh token 都還活著 | ✅ **成立**（實跑 200），⚠️ 但它是 **upstream 刻意的設計** ⇒ §3.4 要維護者決定 |

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

1. 🔴 **期限剛好反過來。** `access_token_ttl` 預設 **604800**（7 天）；
   `refresh_token_ttl` 預設 **0**，而 config 自己寫著「The default of `0` disables
   refresh-token expiry entirely」⇒ 那把能當 access token 用的憑證**永不過期**。
   client 乖乖輪替 access token，而那把長命的憑證同時是一把長命的 access token，
   整個 refresh 機制想達成的事（access token 短命）被它自己抵銷掉。
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

問題不是「要不要修」，而是**「重新登入該不該結束上一個 session」**。三個選法：

| | 做法 | 代價 |
|---|---|---|
| **A** | 照規格的強版本：登入既有 `device_id` 時收掉該裝置先前的 access ＋ refresh token | 關掉 upstream 刻意留的「一裝置多把 token」。而且**同一個裝置真的有兩個 client 在跑**（例如桌面與 CLI 共用一個 device_id）時，後登入的會把前一個踢掉 |
| **B** | 只收 **refresh token**：舊的 access token 留著自己過期（7 天），但**不能再自我續命** | 改得最小，擋掉 §3.1 最後一行那個「無限續命」；舊 access token 仍有最多 7 天的窗口。而沒要求 refresh 的登入給的是**永不過期**的 token，所以這一條只在 client 有用 refresh 時才有意義 |
| **C** | 設定開關（預設照 A，可關回 upstream 行為） | 多一個設定、多一組測試；而「安全預設 ＋ 可退回」是這個 repo 已經有的模式（`refresh_token_reuse_revoke` 就是） |

⭐ **我的建議是 A，而且不加開關**：「我重新登入了」在使用者心裡**就是**「舊的那個不算了」，
而這條路是補救被偷 token 的人唯一會自己做的事。upstream 那個索引**不會白費** ——
它正是 A 要用的東西（枚舉一個裝置的每一把 token 然後全撤）。
📎 而「同一裝置兩個 client」那個代價，在這個 fork 裡站得住腳的機會很低：device_id 是
client 自己產生的，兩個 client 本來就該有兩個。

⚠️ 若選 A：這是**對 Matrix client 可見的行為改變**（雖然是朝規格那一邊），依
`matrix-compat-best-effort` 的約定要先讓維護者同意 —— 這一節就是在問。

### 3.5 測試（A 版）

1. 同一個 `device_id` 登入兩次 → **第一次的 access token 401**、第二次的 200。
2. 第一次的 **refresh token** 也 401，而且 `POST /refresh` 用它 → 401（§3.1 那一行的反面）。
3. **不同 `device_id`** 登入兩次 → 兩把都還有效（這條防的是「修過頭，把別的裝置也收了」）。
4. 裝置還在、沒有被刪：`/devices` 仍然列得到 `DEVICEONE`（登入不是登出）。
5. wbf 通道：用舊 token 開的連線在重新登入後要被結束 —— ⚠️ 走的是
   `remove_device` 以外的路，所以要確認 `/docs/design/wire/session-teardown.md` §3.1
   的「一個出口」在這條路上也成立，或者明寫它不成立、為什麼。

## 4. 狀態與還沒定的

| | |
|---|---|
| S1 兩條都成立 | ✅ 讀程式 ＋ 實跑確認（§2.1、§3.1） |
| S1 的修法（§2.3） | 📄 **提案**：兩張表 ＋ 認證側的前綴第二道 |
| S4 要不要修、怎麼修（§3.4） | ⏳ **等維護者**：A（照規格收掉，我的建議）／B（只收 refresh）／C（開關） |
| 兩個洞幾支 PR | ⏳ 看 S4 的答案：S1 自己一支；S4 若選 A 可以同一支（同一塊程式、同一組測試），但它是 client 可見的行為改變，標 `[BREAKING]` |
| 遷移 | ✅ **不需要**（server 從未上線）。⚠️ 理由有期限 |
