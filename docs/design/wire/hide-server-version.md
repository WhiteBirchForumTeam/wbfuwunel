# 提案：預設不說自己的版本，要說才設定

> **這份文件回答：server 現在把自己的實作名與版本說給誰聽、為什麼預設該閉嘴、哪些地方不能閉嘴，以及那個設定長什麼樣。**
> 狀態：📄 **提案，等維護者同意**（維護者 2026-10-01 提）。🚫 還沒實作。
> 相關：[/docs/design/wire/wire-format.md](../wire/wire-format.md) §6.3（`Control/Hello`）、[/docs/design/wire/api-bridge.md](../wire/api-bridge.md)。

## 0. 一句話

**`tuwunel_core::version::version()`（帶 git commit 的詳細版本）預設不出現在任何對外的回應裡**；
新設定 `publish_server_version`（預設 `false`）打開才給。🚫 實作名（`wbfuwunel`）與 `features` **不藏** —— 它們是功能協商要用的。

## 1. 現在說給誰聽（六處，全部查過原始碼）

| # | 位置 | 現在給什麼 | 要認證嗎 |
|---|---|---|---|
| 1 | `GET /_matrix/federation/v1/version`（`src/api/server/version.rs`）| `name`、`version`、`compiler` | 🚨 **不用** |
| 2 | `GET /_matrix/client/versions`（`src/api/client/versions.rs`，MSC4383 的 `server`）| 同上 | 🚨 **不用** |
| 3 | `GET /_tuwunel/server_version`（`src/api/client/tuwunel.rs`）| `name`、`version` | 🚨 **不用** |
| 4 | `Control/Hello` 的 `engine`／`engine_version`（`src/api/client/wbf/mod.rs`）| 實作名 ＋ 詳細版本 | ✅ 要（連線帶 token）|
| 5 | **對外請求的 User-Agent**（`src/service/client/mod.rs`、S3 provider）| `wbfuwunel/<semantic>` | — 我們是 client 的那一側 |
| 6 | `GET /_tuwunel/remote_version`（問自己時走 #3 同一份資料）| 同 #3 | ✅ 要 |

⭐ **前三個不用認證**，所以任何人都問得到。而 `version()` 帶的不只是 `1.9.0`：它會併進 **git commit**
（`git_semantic!`／`TUWUNEL_VERSION_EXTRA`）—— 那等於直接指出「這台跑的是哪一個 commit」，
對著公開 repo 就能查出它**還沒**修哪些洞。那是把漏洞清單送給對方。

## 2. 為什麼不是「全部藏起來」

🚫 **實作名（`wbfuwunel`）不藏，三個理由**：

1. **功能協商靠 `features`，不靠版本號** —— 那是 `Hello` 本來的設計（client 宣告、server 回報）。
   但 client 要**知道這台是不是這個 fork** 才會去讀 `features` 的那些 `org.wbftw.*`。
2. **聯邦對端需要它** 來處理已知的實作差異，而那是 Matrix 生態的慣例（Synapse、Dendrite 都報）。
3. 它不隨 commit 變動，所以**不構成「還沒修哪個洞」的資訊**。

🚫 **`features` 不藏**：藏了等於關掉功能協商。它說的是「我會什麼」，不是「我是哪一版」。

⚠️ **`compiler`（rustc 版本）跟著 `version` 一起藏** —— 它同樣指紋化建置環境，而且沒有任何協商用途。

## 3. 設定

```toml
# 預設 false：不在任何回應裡給詳細版本
publish_server_version = false
```

| 設定 | #1 #2 #3 #6 的 `version`／`compiler` | #4 的 `engine_version` | #5 User-Agent |
|---|---|---|---|
| `false`（預設）| **省略欄位**（不是空字串）| **省略欄位** | `wbfuwunel`（不帶版本）|
| `true` | 照現在 | 照現在 | 照現在 |

🚨 **省略欄位，不要給空字串或 `"hidden"`**（全域原則 A6）：Matrix 規格裡 `server.name`／`version`／`compiler`
**三個都是 optional**，所以省略是合法的、而且對讀的人語意清楚；`""` 既是「沒有」也是「空的」，而 `"hidden"`
會被當成版本號去比對。

📎 **`Hello` 的 `engine_version` 也是省略**：那是我們自己的 wire 欄位，⚠️ client 端要能接受它不存在 ——
這一條要寫進 `/docs/design/wire/wire-format.md` §6.3，並通知 `amaid/wbf-matrix-client`。

## 4. ⏳ 要維護者決定的

1. **設定的名字**：`publish_server_version`？還是 `allow_server_version_disclosure`、`show_server_version`？
   📎 我傾向 `publish_server_version` —— `publish_` 說得出「對外給」，而 `show_` 讀起來像 UI。
2. **User-Agent（#5）要不要一起藏？**
   - 藏的理由：對外連線也會洩漏，而且是主動送出去的。
   - 不藏的理由：對端用 UA 做相容處理與濫用排查，完全不帶版本可能被某些 server 視為異常。
   - ⏳ **我傾向跟著同一個設定**：預設只給 `wbfuwunel`（保留實作名、拿掉版本），對端仍認得我們。
3. **`/_tuwunel/server_version`（#3）要不要改成需要認證？**
   📎 那是上游自己的非標準端點，加認證不影響 Matrix 相容。但這是**另一件事**，我建議不混進來。
4. **`compiler` 跟著藏**（我的建議）還是獨立設定？📎 獨立設定是多一個沒人會調的旋鈕（A2）。

## 5. 達標條件

| 條件 | 怎麼驗 |
|---|---|
| 預設下 #1 #2 #3 的回應**沒有** `version` 與 `compiler` 欄位，但**有** `name` | e2e（三個端點各打一次，斷言欄位不存在）|
| 預設下 `Hello` 沒有 `engine_version`，但有 `engine` 與 `features` | e2e |
| `publish_server_version = true` 時四處都回到現在的樣子 | e2e（同一支腳本換 config 再跑）|
| 省略的是**欄位**，不是空字串 | 單元（序列化後的 JSON 逐字斷言）|
| ⭐ 預設下 `version()` 這個字串**完全沒有出現在任何回應裡** | e2e：把回應全文抓起來，斷言它不含 `version()` 的值 |

🚨 最後那條是真正的閘門：前面幾條逐欄位檢查，而它檢查的是「有沒有從別的縫漏出去」——
📎 這支提案一開始我以為只有四處，實際查出來是六處，所以逐欄位列舉**本身就是會漏的那種驗法**。
