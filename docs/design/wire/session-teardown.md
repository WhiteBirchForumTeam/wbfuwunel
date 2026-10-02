# 登出要把連線拆掉（外部審查 2026-09-29 #8 後半）

> ✅ **已實作：PR #105。** 維護者 2026-09-29 定：#8 切兩半，前半（控制框也 revalidate）走 **PR #103（已合併）**，
> 這一半另開提案。
>
> ✅ **維護者 2026-10-02 定了 §7 的四件**，結論寫在 §7，影響到的章節就地改掉了：
> **掛進 `remove_device`**（§4.2）、**拆訂閱＋關連線**（§4.2）、**鎖帳號不另走一條、靠重驗＋逾時**（§4.3 整節因此刪掉）、
> **取消訊號要做**（§5 的 B 案）。
>
> 📎 **實作時發現這份提案兩處估錯**（PR #105 寫明）：① §4.1 說把 `SlotTable` 的值換成連線的佇列 ——
> 實際上**位子與登記不是同一個數字**（位子在升級時就拿、連線要有佇列才叫得動），所以值是
> `DeviceConnections { reserved, live }` 兩個欄位，`count_for` 的語意才不會變。
> ② §3.3 把「session 可變」列為要新解的問題 —— **現有程式已經處理好了**（`ConnectionSlot::is_for`
> ＋ `Session::inherit_slot`），這支只是讓登記跟著位子走。
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

#### ⭐ 「怎麼知道要關哪一條」—— 這件事程式裡已經有答案了

維護者 2026-10-02 問的就是這個，而答案不是「要新建一個對照」：

```rust
// service/connections.rs
impl ConnectionSlot {
    pub fn is_for(&self, user: &UserId, device: &DeviceId) -> bool { … }
}
```

**`ConnectionSlot` 自己就記著它是哪個 `(user, device)` 的位子**，而那個 slot **放在連線的 `Session` 裡**
（`wbf/mod.rs` 的 `Session { user, device, token, slot }`）。所以：

- **身份換了，位子跟著換** —— `reserve_connection_slot` 在升級與**每次 `Login`** 都跑；同裝置重新登入由
  `Session::inherit_slot` 把舊位子接過去（`slot.is_for(&self.user, &self.device)` 就是那個判斷）。
  ⇒ ⚠️ §3.3 說的「session 可變」**已經被現有程式處理好了**，不是這支要新解的問題。
- **`remove_device(user, device)` 拿到的正是那把鍵。** 不需要反查、不需要猜：那張表就是以它為鍵。
- **「terminate device」就是同一條路** —— `DELETE /_matrix/client/v3/devices/{id}`、admin 刪裝置、MAS
  同步刪裝置，全部走 `remove_device`（§3.1 的 20 幾個呼叫點），所以它不是另一個情境，是同一個漏斗。
- **`/logout/all`** 走 `end_session(user, device, all_devices=true)`，它**逐一** `remove_device` ⇒ 每個裝置各自被關，
  不需要「關掉這個人全部連線」這種特殊動作。

⚠️ 代價：`SlotTable` 從「一個數字」變成「握著每條連線的送出端」。那讓 `connections` 這個模組同時回答
「這個裝置開了幾條」與「怎麼對其中一條說話」—— 兩個問題，但同一份生命週期，我認為值得（否則第二張表遲早漂）。

### 4.2 在 `remove_device` 裡掛一行（✅ 維護者 2026-10-02 定）

> 「`remove_device` 我建議直接掛進去，既然這是 remove session 語意，沒理由不關連線。」
> 「session 被 kill 我建議就關連線。」

```rust
// users/device.rs::remove_device，拿掉 token 之後
self.services.connections.end_device_sessions(user_id, device_id);
```

📎 `users::Service` 已經持有 `Arc<OnceServices>`，所以**不需要新的接線**。
**這條依賴方向是幹嘛用的**：結束 session 的程式在 `users`，socket 在 `connections` —— 總得有一邊去告訴另一邊。
反方向（`connections` 去監看 token 表）要輪詢或資料庫通知，都比一行呼叫差，而**不叫**就是現在這個狀況：
socket 只能靠自己下一次 `revalidate` 才知道，也就是那個「最壞一個 `wbf_ws_idle_timeout`」的路徑。

`end_device_sessions` 做兩件事（維護者定「拆訂閱 ＋ 關連線」都做）：

1. **拆訂閱** —— `streams` 的 `remove_connection(id)`（兩個登記簿各一次）。推送立刻停。
2. **關連線** —— `try_send(Outgoing::Close { 1008, "session ended" })` ＋ §5 的取消訊號。

⚠️ 兩件都不 `await`（拆訂閱是同步的、`try_send` 不等）⇒ 這個方法可以是同步的。

### 4.3 鎖帳號**不**走這條（✅ 維護者 2026-10-02 定）

> 「lock_account 即使放給他 timeout 應該也可以。這個場景在哪? 既然沒有天然走 remove session，這是不是嚴格定義不是破口。」

⭐ **對，嚴格講它不是破口** —— 因為 `revalidate` 本來就擋它：`check_token` 會呼叫 `locked_check`
（`wbf/mod.rs`），所以被鎖的帳號**在它的下一個 frame 就被關連線**，e2e7 `[3.1a]`／`[3.1b]`／`[4.8a]` 都在守這件事。
它跟登出的差別只有一個：**沒有事件可以掛勾**，所以靠的是「問」而不是「被通知」。

| | 誰接住它 | 最壞延遲 |
|---|---|---|
| 登出／刪裝置／撤 token | `remove_device` 主動拆（這支）| **立刻** |
| **鎖帳號** | `revalidate` 每個 frame 問一次 | **一個 `wbf_ws_idle_timeout`（預設 60 秒）** |
| token **過期** | 同上（🚨 **連事件都不可能有**，它是一個時間）| 同上 |

📎 **場景**：`set_locked` 的呼叫點全部是 **admin 路徑**（`/_synapse/admin/v2/users/{id}` 的 `locked:true`、
`api/client/admin/lock_user.rs`、MAS 的 provision／create_or_modify）—— 不是使用者自己能觸發的動作。
而要立刻的話，admin 本來就可以順手刪那個裝置（走 `remove_device`，立刻）。
⇒ ⭐ **所以這裡不加第二條路**：少一個漏斗、少一份會漂的實作。

## 5. 🚨 惡意 client 那一半：取消訊號（✅ 維護者 2026-10-02 定要做）

§3.4 的結論是「不給就只能靠 idle timeout」。兩個選項：

| | 做法 | 代價 |
|---|---|---|
| A | 只推 Close（上面 4.2 的第 2 步）。惡意 client 撐到 `wbf_ws_idle_timeout`（預設 60 秒）| 零新機制。⚠️ 但「立刻終止」對最該管的那個情境**不成立** |
| ✅ **B（定案）** | 每條連線多一個 `tokio::sync::Notify`（或 oneshot），接收迴圈的 `select!` 多一個分支 —— 跟現有的 `shutdown` 分支同形 | 多一個欄位、多一個分支。⭐ 形狀是現成的，`shutdown` 已經證明這個做法在這個迴圈裡可行 |

理由是 §1.1：這條防線存在的理由就是「裝置在別人手上」，而那個人不會合作。
⚠️ 而 B 要小心一件事：取消訊號**不能讓 task 跳過收攤**（`ConnectionSlot` 要還、`JoinSet` 的帳要收）——
它應該讓迴圈 `break`，走跟 `shutdown` 完全一樣的出口，🚫 不是 `abort()`。
📎 `abort()` 會繞過收攤，而 PR #101 剛好證明過這個模組的收攤路是安全性論證的一部分
（`State` 是 `*const Services`）。

## 6. 不做什麼

- 🚫 **不改 `revalidate` 的位置或頻率** —— 那是前半（PR #103），而且兩半互補（§2）。
- 🚫 **不給 server 端「清掉已經卡住的佇列」的逃生口** —— 那是 #88 明確決定過不給的事，與這支無關。
- 🚫 **不碰 HTTP**：HTTP 的每個 pack 都重新認證，沒有「留著的連線」這個概念。
- 🚫 **不處理聯邦**：對端的連線不是我們的。
- 🚫 **鎖帳號不另走一條**（§4.3，維護者 2026-10-02 定）。
- 🚫 **`Refresh` 換 token 不斷線**（下表 7.1）。

## 7. ✅ 維護者 2026-10-02 的決定

| | 問題 | 決定 |
|---|---|---|
| **7.1** | 哪些事件算？`remove_device` 的 20 幾個呼叫點一次全包，還是要挑？ | ✅ **全包，直接掛進 `remove_device`** ——「既然這是 remove session 語意，沒理由不關連線」。<br>⚠️ **`Refresh` 換 token 不算**（它不走 `remove_device`，走 `set_access_token`）：換 token 的是合法持有者，連線照舊活著（e2e7 `[4.3c]` 守著這個行為）|
| **7.2** | 只拆訂閱，還是也關連線？ | ✅ **兩個都做** ——「session 被 kill 我建議就關連線」。拆訂閱就不洩漏了，但關連線才是 client 期待的訊號（它才知道要重新登入，而不是以為自己還連著卻收不到東西）|
| **7.3** | 要不要 §5 的取消訊號？ | ✅ **要（B 案）** |
| **7.4** | `users` → `connections` 這條依賴方向可以嗎？「你怎麼知道要關哪個？」 | ✅ 可以。**而「怎麼知道」程式裡已經有答案**：`ConnectionSlot::is_for(user, device)`，slot 放在連線的 `Session` 裡、`Login` 時跟著換 —— 詳見 §4.1 那一小節。「terminate device」走的就是 `remove_device` 這同一個漏斗 |
| **7.5** | 鎖帳號？ | ✅ **不另走一條，靠 `revalidate` ＋ 逾時** —— 維護者判「嚴格定義不是破口」，§4.3 驗過成立（`locked_check` 在 `revalidate` 裡） |

## 8. 測試計畫

| 測什麼 | 怎麼測 |
|---|---|
| **登出之後推送真的停了** | e2e：bob 訂房間 → HTTP 登出 → 另一個人在那個房間發言 → bob 那條連線**收不到** Push（而登出前收得到）|
| **只斷那個裝置** | 🚨 同一個人兩個裝置都連著、都訂同一個房間 → 登出其中一個 → **另一個照樣收得到**（§3.2 的 `by_user` 陷阱就是這條在守）|
| **合作的 client 立刻斷** | e2e：登出後**不送任何 pack**，斷言幾秒內收到 Close 1008（對照 PR #103 的 `[3.2d]`，那條等的是 keep-alive 觸發）|
| **惡意 client 也立刻斷**（B 已定案）| ⚠️ PowerShell 的 `ClientWebSocket` 不給「收到 Close 卻不回」的控制 —— 這條可能只能用 server 端的 log／`count_tracked_tasks()` 間接驗，或寫成 Rust 整合測試。**實作時要先確認測得到，再宣稱它成立** |
| **收攤沒被繞過** | 取消之後 `ConnectionSlot` 還回去了：登出後立刻用同一個裝置開滿名額（e2e7 `[5.6]` 的同形）|
| **鎖帳號**（§4.3：**不**主動拆）| 🚨 要測的是**反向**：鎖之後那條連線**在它下一個 frame** 被關，而不是立刻 —— 等於釘住「這條靠 `revalidate` 不靠通知」這個決定。e2e7 `[3.1a]`／`[4.8a]` 已經守住拒絕那半 |
| **`Refresh` 不斷線**（§7.1）| e2e7 `[4.3c]` 已經在守，實作後要確認它還是綠的 —— ⭐ 那條現在是「換 token 不該觸發拆除」的反向證據 |

## 9. 風險

- ⚠️ **`SlotTable` 的鎖持有時間變長**：值從 `u32` 變成一張小表。🚫 鎖裡面不要 `await`（`connections.rs`
  的 `lock_tasks` docstring 已經寫過這條規則，同一個理由）。
- ⚠️ **`PackQueue` 的所有權**：表裡握著 sender 的複本，所以「所有 sender 都掉了」這個條件變了 ——
  送出 task 的結束條件靠的正是那個（`send_queued` 的 `while let Some(item) = outgoing.recv()`）。
  🚨 **連線死掉時表裡那份一定要跟著拿掉**，否則送出 task 永遠不結束。
  ⭐ `ConnectionSlot` 的 `Drop` 本來就在做「還位子」，這件事掛在同一個地方 —— 但這是這支最容易寫錯的一行。
- 📎 **惡意 client 的最壞情況取決於 §7.3**：選 A 就是 `wbf_ws_idle_timeout`，不是「立刻」。
