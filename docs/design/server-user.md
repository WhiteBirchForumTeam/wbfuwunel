# 伺服器帳號：`@system`，顯示名稱 `[SYS] <server_name>`

> **這份文件回答：伺服器自己的帳號叫什麼、顯示成什麼、為什麼改、改的時候怎麼防止它變成別人的帳號。**
> 狀態：維護者 2026-09-14 指定名字與顯示名稱格式；分支 `feat/system-user`。

## 0. 一句話

伺服器帳號從 `@conduit:<server_name>` 改名為 **`@system:<server_name>`**，並且一定有顯示名稱 **`[SYS] <server_name>`**（例：`[SYS] matrix.org`）。啟動時如果 `@system` 已經是一個**不是這台 server 建的**帳號，就拒絕啟動。

## 1. 為什麼改

- 使用者收到伺服器的訊息（管理員房間的回覆、伺服器公告、附件警告）時，client 顯示的是**顯示名稱**。這個帳號以前**從來沒有設定顯示名稱**，所以 client 拿 ID 的前半段頂著，顯示成一個叫「conduit」的人 —— 認不出是伺服器。
- `conduit` 是上游的歷史名字（Conduit → conduwuit → tuwunel），對這個 fork 的使用者沒有意義。
- 維護者 2026-09-14：「就叫 @system」；顯示名稱「`[SYS] domain`」。

## 2. 它是什麼身分（不因改名而變）

- server 內部把它當管理員（`admin::user_is_admin` 直接回 true）；管理員房間是它開的，權限 69420（維護者：不改）。
- 它發的每個事件照樣過 Matrix 的授權檢查（`timeline/create.rs` 的 `state_res::auth_check`）：不在房裡就發不出去。改名不改這一點。
- 沒有密碼，除非設了 `emergency_password`。

## 3. 顯示名稱

- 值：`[SYS] ` 加上 `server_name` 原樣（`globals::server_user_displayname`）。`server_name` 設定後不能改（資料庫有記號），所以這個值也不會漂移。
- **寫在兩個地方，缺一不可**：
  1. **profile**（`/profile` 查到的）：建立帳號時設；每次啟動時比對，不一樣就改。
  2. **它自己的 join 事件**：client 在房間裡顯示的是 `m.room.member` 事件裡的 `displayname`，不是 profile。所以它加入房間的三個地方（開管理員房間、開公告房間、開附件警告房間）都用同一個 `globals::server_user_join()` 產生內容，帶著顯示名稱。
- 啟動時如果改了 profile，照 `Propagation::All` 更新它已經加入的房間。

## 4. 🚨 防止「別人的帳號變成伺服器帳號」

`user_is_admin` 只比對 ID。如果 `@system` 在改名之前已經是一個真人的帳號（例如從 Conduit／conduwuit 匯入的資料庫裡有人叫 `system`），改名之後那個人**會被當成管理員**。這是權限提升，不是顯示問題。

**防線：一個記號，一個閘門。**

- 記號：server **自己建立**伺服器帳號時，在 `global` 表寫下 `server_user` ＝ 那個 user ID（`admin::create_server_user`，唯一建立它的地方）。
- 閘門：啟動時（`admin::ensure_server_user`，在資料庫遷移之後、任何 worker 之前）：

| `@system` 存在？ | 記號 | 結果 |
|---|---|---|
| 不存在 | — | 建立它（寫記號、設顯示名稱）。之後任何註冊路徑都因為「帳號已存在」拿不到這個名字 |
| 存在 | 等於 `@system:<server_name>` | 是自己建的；比對並補上顯示名稱 |
| 存在 | 沒有，或是別的值 | **拒絕啟動**，訊息說明這個名字已經被一個不是這台 server 建的帳號佔用 |

- ⭐ 為什麼啟動時就建立，而不是等第一次要用：`create_admin_room = false` 時帳號原本要到第一次發附件警告才建立，那之前 `@system` 這個名字是**空的，誰都能註冊**。啟動時就建立，名字從第一刻起就被佔住，六條註冊路徑（一般註冊、SSO、OIDC、MAS、管理 API…）不必各自再加一條保留規則。
- 📎 `database_migrations = false` 也照樣過這道閘門：它不是遷移，是身分檢查。
- 📎 唯讀模式（`read_only`）：只檢查、不寫；帳號不存在或顯示名稱不對就記一條警告。

## 5. 不做的

- **不遷移舊的 `@conduit`**：維護者 2026-09-14 確認這個 fork 從未上線，沒有要搬的資料。開發時留下的舊資料庫，管理員房間裡加入的是 `@conduit`，改名後 `get_admin_room` 找不到管理員房間（它只認伺服器帳號有加入的那一個）；重建資料庫即可。
- **不做成設定值**：名字能設定就等於允許運行中的 server 改名，又回到遷移問題。
- **69420 不改**（維護者 2026-09-14）。

## 6. 落點

| 做什麼 | 落點 |
|---|---|
| 名字、顯示名稱、join 事件內容 | `src/service/globals/mod.rs` |
| 建立時寫記號與顯示名稱；啟動閘門 | `src/service/admin/create.rs` 的 `create_server_user`、`ensure_server_user` |
| 呼叫閘門 | `src/service/services.rs` 的 `start`，在 `migrations` 之後 |
| 三個 join 事件 | `admin/create.rs`、`admin/attachments_notice.rs`、`api/client/admin/misc/send_server_notice.rs` |
| 文件裡的 `@conduit` | `docs/authentication/legacy.md`、`docs/troubleshooting.md`、`config/mod.rs` 的註解（`tuwunel-example.toml` 由它產生） |
| e2e | `tests/e2e/e2e9.ps1` 比對的 sender 改成 `@system:localhost`；加一條：伺服器發的邀請，邀請事件裡的 sender 顯示名稱是 `[SYS] localhost` |
