# 橋底下的 handler panic 不該帶走整條連線

外部審查 2026-09-29 剩下的最後一條 🟡。維護者 2026-10-06：「那個另外處理，先小的完成再說吧」
—— ⇒ 🚫 **這一支只做橋**，WS handler 自己的 panic 是另一個形狀（§5）。

## 1. 現狀：橋的 router 身上一層 middleware 都沒有

| | |
|---|---|
| 服務用的 router | `src/router/layers.rs` 把十幾層套上去，而 `CatchPanicLayer::custom(catch_panic)` 是**最後加的那一個** ⇒ ⭐ **最內層，直接包住 router** |
| 橋的 router（本支之前）| `BridgeRouter(build(Router::new(), server).with_state(state))` —— ⭐ **一層都沒有** |

⚠️ **「最後加的＝最內層」這一條我原本寫反了**（cirno 在 PR #109 指出）。從 tower 的原始碼追：

```rust
// tower-0.5.3 builder/mod.rs:132
pub fn layer<T>(self, layer: T) -> ServiceBuilder<Stack<T, L>> { Stack::new(layer, self.layer) }
// tower-layer-0.3.3 stack.rs:25
fn layer(&self, service: S) -> Self::Service { let inner = self.inner.layer(service); self.outer.layer(inner) }
```

`builder.layer(A).layer(B)` 展開成 `A(B(svc))` ⇒ **先加的在外面**。
🚨 **後果（既有，不是本支引入的）**：壓在 catch-panic **外面**的那十幾層 —— 含 `HandleLayer`
（也就是 `request::handle`，我們自己的程式）、Trace、三層逾時、CORS、壓縮 —— **它們自己的 panic
仍然沒有去處**。那屬於 §5 那題，🚫 不在這支。
📎 本支的結論不受影響：橋走的是 `Router::layer`，而 axum 的 `Router::layer` 同時套到
`path_router` 與 `fallback_router` ⇒ **橋的每一個端點都在覆蓋內**。

橋的路徑是 WS handler → `bridge::handle` → `router.0.clone().oneshot(request).await`
（`src/api/client/wbf/bridge.rs` 的 `handle`）。⇒ 🚨 **那個 `.await` 底下的 handler 一 panic，panic 就沿著
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
| `catch_panic`（28 行：`requests_panic` metric ＋ M_UNKNOWN 的 500）| **原本**在 `src/router/layers.rs` | `tuwunel` |
| 橋的 router 建在 | `src/api/router.rs` 的 `build_bridge_router` | `tuwunel_api` |

📌 這張表講的是**改之前**的位置；現在 `catch_panic` 在 `src/api/router/panic.rs`。
📎 本文一律指**符號**而不是行號 —— 行號會漂（cirno 在 PR #109 指出原本幾處已經指錯）。

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
| ✅ **這一層真的把 panic 變成 500 ＋ 那個 body** | 單元測試自己建一個 `panic!("boom")` 的 route，套上同一個 layer，`oneshot` 斷言 `500` ⭐ **並且把 body 讀出來比 `errcode`／`details`** —— 📌 穿過 layer 驗到的就是橋要讀的那幾個欄位 |
| ✅ **`to_panic_response` 自己的形狀** | 另一條直接餵它，驗 `application/json` 跟三個欄位齊全 |
| ⚠️ **`requests_panic` 那一行 metric** | 🚫 **沒有測試跑到它**（salvia 在 PR #109 指出）—— 它要一個完整的 `Services`，而那要一整個資料庫（`Services::build` 只在被排除的 crate 的整合測試裡跑得起來）。📎 那 5 行是**搬家未改**、而且跟服務用的 router 共用同一份 ⇒ 為一行 `fetch_add` 開一個 DB 不划算 |
| ⚠️ **橋「套上了」它** | 🚫 只能讀碼。要端到端得在正式路由塞一個 debug-only 會 panic 的端點，侵入性不值得 |

📌 **這個界限寫進 PR 描述**，不寫成「已驗證」。

## 5. 🚫 這一支不做：WS handler 自己的 panic

橋以外的 handler（pack 解析、E2EE、媒體、`Event/Send` 那些）**同樣沒有去處** ——
它們不經過任何 HTTP router，所以 `CatchPanicLayer` 救不到。要包的是**task**，不是一層 middleware，
而且要決定「panic 之後這條連線是關掉還是回一則 `Error` 繼續」—— ⭐ 那是比這支大的題目。
維護者 2026-10-06 明講先做小的，⇒ 另開一支。
