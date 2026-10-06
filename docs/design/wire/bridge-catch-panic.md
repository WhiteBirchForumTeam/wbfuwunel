# 橋底下的 handler panic 不該帶走整條連線

外部審查 2026-09-29 剩下的最後一條 🟡。維護者 2026-10-06：「那個另外處理，先小的完成再說吧」
—— ⇒ 🚫 **這一支只做橋**，WS handler 自己的 panic 是另一個形狀（§5）。

## 1. 現狀：橋的 router 身上一層 middleware 都沒有

| | |
|---|---|
| 服務用的 router | `src/router/layers.rs` 把十幾層套上去，**最外面是 `CatchPanicLayer::custom(catch_panic)`** |
| 橋的 router | `BridgeRouter(build(Router::new(), server).with_state(state))`（`src/api/router.rs:72`）—— ⭐ **一層都沒有** |

橋的路徑是 WS handler → `bridge::handle` → `router.0.clone().oneshot(request).await`
（`src/api/client/wbf/bridge.rs:333`）。⇒ 🚨 **那個 `.await` 底下的 handler 一 panic，panic 就沿著
WS 的 task 傳上去**，而那條 task 外面沒有 `catch_unwind`
（📎 `src/router/mod.rs` 的三個 `catch_unwind` 包的是 dylib 邊界的 `start`／`stop`／`run`，不是每個 task）。

⇒ **一個端點的 panic 換掉整條連線**，而且不走我們寫的收攤路（跟外部審查 #7／PR #101 同一個形狀）。
CLAUDE.md P：**每個失敗都要有去處，而且去處要說得出來。**

## 2. ⭐ 橋那一端不用改 —— 500 本來就會變成一則 `Error` pack

這是這支小的主要原因。`build_reply_pack`（`bridge.rs:642`）對非 2xx 走 `Error`：

```rust
let code = reject_code_for_status(status);   // 500 → RejectCode::Internal
… PackBuilder::new(Kind::Control, control::ERROR, flags, id, seq)
```

而 `catch_panic` 回的正是 `500` ＋ `{"errcode":"M_UNKNOWN", …}` 的 JSON。
⇒ ✅ **加上那一層之後，一個 panic 自然變成「`Error(Internal)`，message 取自 body 的 `error` 欄位」回給 client，連線活著。**
🚫 不用動橋、不用動 `Error` 詞彙表、不用動 client 契約、🚫 **不是 breaking**。

## 3. 真正要處理的是 crate 方向

| | 在哪 | crate |
|---|---|---|
| `catch_panic`（28 行：`requests_panic` metric ＋ M_UNKNOWN 的 500）| `src/router/layers.rs:298` | `tuwunel` |
| 橋的 router 建在 | `src/api/router.rs:72` | `tuwunel_api` |

`tuwunel` 依賴 `tuwunel_api`，🚫 不能反向 ⇒ **把 `catch_panic` 搬到 `tuwunel_api`**
（新檔 `src/api/router/panic.rs`），`src/router/layers.rs` 改成從那裡 import。
🚫 **不複製一份** —— 兩份 panic 回應遲早漂移，而漂移的那天不會有人通知你（A4）。
📎 四個依賴（`bytes`、`http-body-util`、`serde_json`、`tower-http` 的 `catch-panic`）
`tuwunel_api` 已經都有，🚫 不用動 `Cargo.toml`。

**而 layer 套在 `build_bridge_router` 裡面，不是由呼叫者套上去**：

```rust
pub fn build_bridge_router(state: State, server: &Server, services: &Arc<Services>) -> BridgeRouter
```

⭐ 理由是 A5：**讓橋自己保證它有這層**，而不是靠「每個呼叫者都記得套」。
📎 `BridgeRouter` 的欄位是 `pub(crate)`，所以外面本來也套不上 —— 多一個參數比把欄位開出去好。
呼叫點只有一個（`src/router/router.rs:13`），那裡手上就有 `services`。

## 4. 測試與它的界限

🚨 **正式路由裡沒有任何會 panic 的 handler**（那是好事），所以**端到端驗不到**。
⇒ 老實分成兩塊，🚫 不混在一起報：

| 驗得到 | 怎麼驗 |
|---|---|
| ✅ **這一層真的把 panic 變成 500** | 單元測試自己建 `Router::new().route("/boom", get(\|\| async { panic!("boom") }))`，套上同一個 layer，`oneshot` 斷言 `500` ＋ body 的 `errcode` 是 `M_UNKNOWN` |
| ✅ **500 ＋ 那個 body 會變成 `Error(Internal)`** | `build_reply_pack` 的既有單元測試已經走這條；需要的話補一條直接餵 `catch_panic` 的輸出 |
| ⚠️ **橋「套上了」它** | 🚫 只能讀碼。要端到端得在正式路由塞一個 debug-only 會 panic 的端點，侵入性不值得 |

📌 **這個界限寫進 PR 描述**，不寫成「已驗證」。

## 5. 🚫 這一支不做：WS handler 自己的 panic

橋以外的 handler（pack 解析、E2EE、媒體、`Event/Send` 那些）**同樣沒有去處** ——
它們不經過任何 HTTP router，所以 `CatchPanicLayer` 救不到。要包的是**task**，不是一層 middleware，
而且要決定「panic 之後這條連線是關掉還是回一則 `Error` 繼續」—— ⭐ 那是比這支大的題目。
維護者 2026-10-06 明講先做小的，⇒ 另開一支。
