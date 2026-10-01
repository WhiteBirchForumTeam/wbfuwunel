# 登出要把連線拆掉（外部審查 2026-09-29 #8 後半）

> 📄 **提案，還沒實作。** 維護者 2026-09-29 定：#8 切兩半，前半（控制框也 revalidate）走 PR #103，
> 這一半另開提案。⏳ §7 有四件要維護者決定的事；決定之前 🚫 不動 `src/`。
>
> 前半是 [/docs/design/wire/pack-pipeline.md](pack-pipeline.md) §3.1。

## 1. 現在的樣子

**訂閱掛在「連線」上，而登出只是把 token 從資料庫刪掉 —— 沒有任何人去告訴那條連線。**

訂閱登記簿（`src/service/streams/subscribers.rs`）是記憶體裡的表，鍵是 `ConnectionId`：

```rust
topics:      HashMap<Topic, HashSet<ConnectionId>>,
subscribers: HashMap<ConnectionId, Subscriber>,
by_user:     HashMap<OwnedUserId, HashSet<ConnectionId>>,
```

`POST /_matrix/client/v3/logout` 走 HTTP，動的是 `users` 服務的 token 表 —— **這三張表它一個字都沒碰**。

而推送端**會再查一次房間成員資格**（`streams/rooms.rs` 的 `push_to_room`：「a kick that lands during the
caller's `await` does not get one more event through」），但**沒有任何地方再問一次 token**。
登出不改變你還在那個房間，所以：

> ⭐ **事件繼續推、to-device 繼續推，推給一條 token 已經不存在的連線。** 而 to-device 推的是 **E2EE 的金鑰通道**。

### 1.1 威脅模型很窄，但正好是最該管的那個

洩漏要求攻擊者**已經握著那個 socket**（連線當初是合法認證過的）。所以真實情境只有一個：

🚨 **裝置被偷／token 被偷，使用者去「遠端登出那個裝置」—— 而那個動作的全部意義就是讓它立刻停止收東西。**
帳號被 admin 鎖（處理被盜帳號）是同一件事。

### 1.2 不會毀資料，這點要說清楚

to-device 的項目**只有 client ack 了才前進**（`streams/devices.rs` 的 `PushedItem`：「the position the
client acks and later destroys by」），而一條失效的連線**一送 ack pack 就被 `revalidate` 擋下並關線** ——
項目還在佇列裡，合法的新 session 照樣收得到。
它還佔著那個裝置的連線名額與 device topic 的持有權，但 `subscribe_device` 的規則是**新來的把舊的擠掉**
（送一個 superseded pack），所以也不是永久阻擋。

⇒ **後果是「讀取洩漏 ＋ 一段時間的佔用」，不是「資料遺失」。**

## 2. 前半買到多少，以及為什麼兩半互補

PR #103 之後 session 在**每一個 frame**（含 Ping／Pong）之前都重驗一次。但 **Ping 是 client 驅動的**：

| | 洩漏窗口 |
|---|---|
| #103 之前 | **無限期**（只要持續 ping）|
| **#103 之後** | 到它送出任何一個 frame 為止，上限 ＝ `wbf_ws_idle_timeout`（維護者 2026-10-01 把預設從 300 降到 **60 秒**，就是為了這個）|
| **這一半做完** | 立刻 |

⭐ **而這一半取代不了前半**：token **過期**是一個時間（`wbf/mod.rs` 的 `check_token` 比對 `expires_at`），
**沒有任何事件可以掛勾** —— 只有「每個 frame 問一次」接得住它。
📎 soft logout（`refresh_token_hard_logout = false`）也是這一類：裝置還在、只有 refresh token 被拿掉，
沒有任何「拆掉它」的事件。

## 3. 現有零件盤點：缺的比想像少

### 3.1 🚨 掛鉤點只有一個 —— `remove_device`

這是這份提案最重要的發現。**每一條「硬性結束 session」的路都走 `users::Service::remove_device(user, device)`**
（`src/service/users/device.rs:82`），它自己再呼叫 `remove_tokens`：

| 來源 | 進入點 |
|---|---|
| `/logout`、`/logout/all` | `users/login.rs` 的 `end_session` → `remove_device` |
| 刪裝置（`/devices/{id}`、`/delete_devices`） | `api/client/device/*` |
| 改密碼帶 `logout_devices` | `api/client/account/change_password.rs` |
| OIDC revoke、session end、token 路徑 | `api/oidc/*` |
| refresh token 重用後撤銷（`refresh_token_reuse_revoke`） | `users/login.rs` |
| admin、MAS、dehydrated device | `api/client/admin/*`、`service/users/dehydrated_device.rs` |

⭐ **20 幾個呼叫點，一個漏斗。** 所以「哪些事件算」這件事**不需要逐一掛鉤** —— 掛在 `remove_device` 裡。
📎 這正是 [/docs/design/overview/why-not-matrix-and-core-design.md](../overview/why-not-matrix-and-core-design.md)
講的「接縫要少而明確」：一個閘門，不是三處各做一次。

⚠️ **唯一的例外是鎖帳號**：`set_locked(user)` 是另一個漏斗（`users/mod.rs:333`，4 個呼叫點），
而且它是**使用者**粒度、不碰裝置 —— 它要自己一條路。

### 3.2 ⚠️ 對照表是三張半成品，沒有一張夠用

| 哪張表 | 鍵 | 有什麼 | 少什麼 |
|---|---|---|---|
| `connections.rs` 的 `SlotTable` | **`(user, device)`** ✅ | 一個 `u32` **計數** | 🚫 沒有 id、沒有 handle |
| `subscribers.rs` 的 `by_user` | `user` | ConnectionId 的集合 | 🚫 **沒有 device**（登出一個裝置會砍掉同一人其他裝置）、只含**訂閱過**東西的連線 |
| device 串流的 topic | **`(user, device)`** ✅ | `device_holder()` 直接回 ConnectionId | 🚫 只含**訂了自己 to-device 佇列**的那一條 |

### 3.3 ⚠️ session 是可變的

同一條連線可以**先登入成 alice、再登入成 carol**（通道上的 `Login`，e2e7 `[4.5a]` 守著這個行為）。
所以任何索引**不能只在升級時寫一次**，`Login`／`Refresh`／`Logout` 每一次都要更新它。

### 3.4 🚨 真正的機械缺口：socket 要死，得由接收迴圈放手

送出端（`ws.rs` 的 `send_queued`）只握著 sink：

```rust
| Outgoing::Close { code, reason } => {
    let _closing = sink.send(Message::Close(Some(frame))).await;
    break;      // 然後 sink.close()
}
```

而接收迴圈卡在 `stream.next()` 上等著。於是：

| 對方 | 結果 |
|---|---|
| **合作的 client** | 收到 Close 就回 Close → `stream.next()` 結束 → **立刻真的斷** ✅ |
| 🚨 **惡意的 client**（無視 Close、不關 TCP）| 讀的那半還開著 → 接收迴圈**撐到 `wbf_ws_idle_timeout`** |

⇒ ⭐ **「推一個 Close 進佇列」對偷裝置的人不夠。** 要對它也立刻斷，接收迴圈需要一個**從外面叫得動的取消訊號**
（跟現在的 `shutdown` 一樣，多一個 `select!` 分支）。

## 4. 提案

### 4.1 讓 `SlotTable` 存有用的東西

```rust
// 現在
type SlotTable = Arc<Mutex<HashMap<(OwnedUserId, OwnedDeviceId), u32>>>;

// 提案
type SlotTable = Arc<Mutex<HashMap<(OwnedUserId, OwnedDeviceId), HashMap<ConnectionId, PackQueue>>>>;
//                                                                ↑ 數量 = len()，上限照舊
```

⭐ **為什麼放在 `connections.rs` 而不是新開第四張表**：它**已經有那個鍵**、**已經有完整的生命週期**
（升級時 `take_slot`，連線死掉時 `ConnectionSlot` 的 `Drop` 還回去）、**也已經處理過「同一裝置重新登入不該多佔一個位子」**
（e2e7 `[5.5a]`）。計數本來就是 `len()`，所以上限邏輯不變。

⚠️ 代價：`SlotTable` 從「一個數字」變成「握著每條連線的送出端」。那讓 `connections` 這個模組同時回答
「這個裝置開了幾條」與「怎麼對其中一條說話」—— 兩個問題，但同一份生命週期，我認為值得（否則第二張表遲早漂）。

### 4.2 在 `remove_device` 裡掛一行

```rust
// users/device.rs::remove_device，拿掉 token 之後
self.services.connections.end_device_sessions(user_id, device_id).await;
```

📎 `users::Service` 已經持有 `Arc<OnceServices>`，所以**不需要新的接線** —— 但這是一條新的
**`users` → `connections` 依賴方向**，要維護者認可（§7.4）。

`end_device_sessions` 做兩件事：

1. **拆訂閱** —— `streams` 的 `remove_connection(id)`（兩個登記簿各一次）。推送立刻停。
2. **叫它關** —— 對每條連線 `try_send(Outgoing::Close { 1008, "session ended" })`。

### 4.3 鎖帳號走自己那條

`set_locked(user)` 之後對**那個使用者的每一個裝置**做同一件事。
⚠️ 它是同步函式（不 `await`），而 `end_device_sessions` 要 `await` 嗎？拆訂閱與 `try_send` 都不需要 ——
所以這條可以是同步的，不改 `set_locked` 的簽名。

## 5. 🚨 惡意 client 那一半：要不要給取消訊號

§3.4 的結論是「不給就只能靠 idle timeout」。兩個選項：

| | 做法 | 代價 |
|---|---|---|
| **A** | 只推 Close（上面 4.2 的第 2 步）。惡意 client 撐到 `wbf_ws_idle_timeout`（預設 60 秒）| 零新機制。⚠️ 但「立刻終止」對最該管的那個情境**不成立** |
| **B** | 每條連線多一個 `tokio::sync::Notify`（或 oneshot），接收迴圈的 `select!` 多一個分支 —— 跟現有的 `shutdown` 分支同形 | 多一個欄位、多一個分支。⭐ 形狀是現成的，`shutdown` 已經證明這個做法在這個迴圈裡可行 |

📌 **我建議 B**，理由是 §1.1：這條防線存在的理由就是「裝置在別人手上」，而那個人不會合作。
⚠️ 而 B 要小心一件事：取消訊號**不能讓 task 跳過收攤**（`ConnectionSlot` 要還、`JoinSet` 的帳要收）——
它應該讓迴圈 `break`，走跟 `shutdown` 完全一樣的出口，🚫 不是 `abort()`。
📎 `abort()` 會繞過收攤，而 PR #101 剛好證明過這個模組的收攤路是安全性論證的一部分
（`State` 是 `*const Services`）。

## 6. 不做什麼

- 🚫 **不改 `revalidate` 的位置或頻率** —— 那是前半（PR #103），而且兩半互補（§2）。
- 🚫 **不給 server 端「清掉已經卡住的佇列」的逃生口** —— 那是 #88 明確決定過不給的事，與這支無關。
- 🚫 **不碰 HTTP**：HTTP 的每個 pack 都重新認證，沒有「留著的連線」這個概念。
- 🚫 **不處理聯邦**：對端的連線不是我們的。

## 7. ⏳ 等維護者決定的四件

| | 問題 | 我的建議 |
|---|---|---|
| **7.1** | **哪些事件算？** `remove_device` 的 20 幾個呼叫點一次全包（§3.1），還是要挑？ | **全包** —— 它們全都是「這個裝置的 session 結束了」，挑反而會留下不一致。⚠️ 但 `Refresh` 換 token **不算**（它不走 `remove_device`），而現行行為是連線照舊活著（e2e7 `[4.3c]` 守著）—— 我建議**維持不斷**：換 token 的是合法持有者 |
| **7.2** | **只拆訂閱，還是也關連線？** | **兩個都做**。拆訂閱就不洩漏了，但關連線才是 client 期待的訊號（它才知道要重新登入，而不是以為自己還連著卻收不到東西）|
| **7.3** | **要不要 §5 的取消訊號（B）？** | **要**，理由見 §5。若維護者選 A，那就要接受「最壞 60 秒」並把它寫進文件，🚫 不要宣稱「立刻」|
| **7.4** | **`users` → `connections` 這條依賴方向可以嗎？** | 可以，而且我想不出更好的 —— 另一個方向（`connections` 去監看 token 表）要靠輪詢或資料庫通知，兩個都比一行呼叫差。📎 接線已經有（`users::Service` 持有 `Arc<OnceServices>`）|

## 8. 測試計畫

| 測什麼 | 怎麼測 |
|---|---|
| **登出之後推送真的停了** | e2e：bob 訂房間 → HTTP 登出 → 另一個人在那個房間發言 → bob 那條連線**收不到** Push（而登出前收得到）|
| **只斷那個裝置** | 🚨 同一個人兩個裝置都連著、都訂同一個房間 → 登出其中一個 → **另一個照樣收得到**（§3.2 的 `by_user` 陷阱就是這條在守）|
| **合作的 client 立刻斷** | e2e：登出後**不送任何 pack**，斷言幾秒內收到 Close 1008（對照 PR #103 的 `[3.2d]`，那條等的是 keep-alive 觸發）|
| **惡意 client 也立刻斷**（若選 B）| ⚠️ PowerShell 的 `ClientWebSocket` 不給「收到 Close 卻不回」的控制 —— 這條可能只能用 server 端的 log／`count_tracked_tasks()` 間接驗，或寫成 Rust 整合測試。**實作時要先確認測得到，再宣稱它成立** |
| **收攤沒被繞過** | 取消之後 `ConnectionSlot` 還回去了：登出後立刻用同一個裝置開滿名額（e2e7 `[5.6]` 的同形）|
| **鎖帳號** | 鎖 → 該使用者**每個**裝置的連線都斷 |

## 9. 風險

- ⚠️ **`SlotTable` 的鎖持有時間變長**：值從 `u32` 變成一張小表。🚫 鎖裡面不要 `await`（`connections.rs`
  的 `lock_tasks` docstring 已經寫過這條規則，同一個理由）。
- ⚠️ **`PackQueue` 的所有權**：表裡握著 sender 的複本，所以「所有 sender 都掉了」這個條件變了 ——
  送出 task 的結束條件靠的正是那個（`send_queued` 的 `while let Some(item) = outgoing.recv()`）。
  🚨 **連線死掉時表裡那份一定要跟著拿掉**，否則送出 task 永遠不結束。
  ⭐ `ConnectionSlot` 的 `Drop` 本來就在做「還位子」，這件事掛在同一個地方 —— 但這是這支最容易寫錯的一行。
- 📎 **惡意 client 的最壞情況取決於 §7.3**：選 A 就是 `wbf_ws_idle_timeout`，不是「立刻」。
