# 被邀請者算不算進房間版本號，看 `history_visibility`

✅ **維護者 2026-10-07 全部同意**（§2 的表、§4 的「不加接點」），尚未實作。定方向的原話：

> invite 要不要影響版號，應該是以 invite 有沒有權限讀訊息。
> 如果本房間只限 join，那就是目前的狀態，invite 不影響版號，不進裝置清單。
> 如果本房間是 invite 後可見，那不是洩漏而是設計 —— 當初當成洩漏，所以堵上只限 join；
> 如果是 invite 可見，則分發房間金鑰要包括這個 user 名下的裝置，房間版本號也要改。

權威是 [/docs/design/keys/room-device-version.md](room-device-version.md)（§4.1 的「誰算進去」、§6 的廣播）；
相關：[/docs/design/events/invites-on-the-wire.md](../events/invites-on-the-wire.md)（issue #111，另一支）。

## 1. 現狀，以及它為什麼是現在這樣

`is_membership_counted`（`src/service/device_versions/mod.rs`）是**一個純函數**：

```rust
/// `invite` and `knock` do not change who holds the room key; the `join`
/// that may follow does (…§4.1).
matches!(membership, Join | Leave | Ban)
```

⇒ `invite`／`knock` 不進雜湊，而裝置版本號只在 `Join` 那一格附上。
📎 維護者 2026-09-17 當時的理由是「版本號只為了分發金鑰，無權看的人不必管」。

⚠️ **而那句話的前提被 `history_visibility` 推翻了一半**：`m.room.history_visibility` 是
`invited`／`shared`／`world_readable` 時，被邀請者**加入之後看得到被邀請期間的訊息** ——
所以那些訊息的金鑰本來就該給他。只有 `joined` 才是「加入前的一概看不到」。

📎 **matrix-sdk 的行為一致**（issue #111 引的 `share_room_key`）：`history_visibility == Joined`
才只給 `JOIN`，否則用 `ACTIVE` ＝ join ∪ invite。⇒ 🚨 **現在我們的 `Members` 比 matrix-sdk 少給人**，
那在 `shared` 房間裡是「被邀請者加入後看到一段解不開的歷史」。

## 2. 規則

**`history_visibility == joined` ⇒ 只算 join（現狀不動）；否則 join ∪ invite。**

| `history_visibility` | 被邀請者算進版本號／裝置清單 | 為什麼 |
|---|---|---|
| `joined` | 🚫 不算 | 加入前的訊息他本來就看不到 ⇒ 金鑰不該給 |
| `invited` | ✅ 算 | 從被邀請那刻起有權看 |
| `shared` | ✅ 算 | 成員看得到全部歷史，而他即將是成員 |
| `world_readable` | ✅ 算 | 比 `shared` 更寬。⚠️ **而它在 E2EE 房間裡實際上退化成 `shared`**（維護者 2026-10-07 指出：「都加密了，全部可讀有差嗎？所以還是得發金鑰」）—— ⭐ 金鑰要有**收件人**，而「任何人」不是一份收件清單，所以 server 能做到的最寬就是 join ∪ invite。📎 matrix-sdk 的 `share_room_key` 也是這樣（非 `Joined` 一律用 `ACTIVE` ＝ join ∪ invite）。<br>📌 **這是推論不是規格條文**：規格沒說 `world_readable` 與 E2EE 不相容，但它的字面承諾（任何人可讀）在加密房間裡沒有實作得出來的意思 |

🚫 **`knock` 一律不算** —— 敲門的人還沒有任何人同意他進來，`history_visibility` 給的權限是給
**被邀請／已加入**的人，不是給敲門的（Matrix 的 `invited` 字面也只講 invite）。

## 3. 四個接點，而其中兩個會**靜默**失效

⭐ ①② 是「做這件事」，③ 是「不做就悄悄錯」——📌 而 ③ 才是這份文件存在的理由。

| | 在哪 | 要改什麼 |
|---|---|---|
| **① 雜湊算誰** | `device_versions/mod.rs` 的 `is_membership_counted` ＋ `compute_room_device_version` | 判準從「純看 membership」變成「看 membership ＋ 房間的 `history_visibility`」；被算進去的 `invite` 也要附裝置版本號（現在只有 `Join` 附）|
| **② `/members` 的裝置版本號** | `src/api/client/membership/members.rs` | `if content.membership == Join` 那個條件換成同一個判準 ⇒ ⭐ client 才送得出金鑰給他 |
| 🚨 **③ 被邀請者自己換裝置時，版本號要變** | `device_versions/mod.rs` 的 `announce_device_change` 走的是 **`rooms_joined(user_id)`** | 它**不含只被邀請的房** ⇒ 被邀請者加一台裝置，那間房的版本號不會被廣播。要改成 joined ∪（被邀請且可見的房）。<br>📎 **改起來不難（約 20 行）**：`state_cache::rooms_invited` 已經存在，跟 `rooms_joined` 逐字同形（同樣的 stream、同樣的前綴掃描）。<br>⭐ **而且同一份清單上面那段是 `room_versions` 的快取失效** ⇒ 擴充那份清單**一次同時修好失效與廣播**，不是兩個地方各改一次 |
| ✅ **④ `history_visibility` 自己變的時候** | 沒有任何接點 | 🚫 **不加** —— 維護者 2026-10-07 定，而§4 證明「以當下計算」本來就是 Matrix 的語意 |

**①②③ 都要讀房間的 `history_visibility`**，而 ⭐ **`members.rs` 已經在一次 `state_full` 裡拿整份房間狀態**
（`:67` 那個註解寫著「One read of the room state for both the list and the room version」）
⇒ 📌 可見性從**同一個快照**挑出來，那個「清單與版本號不會來自不同時刻」的保證免費延續。

🚫 **不要在 `is_membership_counted` 裡自己去查房間狀態** —— 它現在是純函數、有單元測試，
而且 `compute_room_device_version` 的契約就是「呼叫者給我一份狀態，我算一個數」。
可見性**當參數傳進去**，不是讓它自己去問（A4：接縫上只傳資料）。

## 4. ✅ `history_visibility` 變動不特別處理（維護者 2026-10-07 定）

> 我傾向 history_visibility 變動不管，畢竟版本號變動是當成員變動，成員變動時就會以當下
> history_visibility 去計算。

⭐ **而這不只是一個可接受的折衷，它就是 Matrix 的語意。** 證據在我們自己的
`user_can_see_event`（`state_accessor/user_can.rs`）：

```rust
let shortstatehash = self.services.state.pdu_shortstatehash(event_id).await  // ← 那一則當時的狀態
let history_visibility = self.state_get_content(shortstatehash, RoomHistoryVisibility, "")  // ← 從那個快照讀
HistoryVisibility::Invited => self.user_was_invited(shortstatehash, user_id)  // ← 成員也用同一個快照
```

⇒ 🚨 **每一則事件都用「那一則發生當時」的可見性 ＋ 當時的成員狀態來判** ——
不是「以 invite 當下為準」，也不是「以現在為準」，而是**以每一則訊息為準**。
⇒ 可見性改變**不會**回溯讓舊訊息變可見，也不會回溯收回；每一則訊息自己帶答案。

**所以兩邊都對，而且沒有「要不要補發」的問題：**

| | |
|---|---|
| 改成 `shared` **之後**送的訊息 | 被邀請者確實看得到，而那時算出的版本號確實含他（版本號是用**當前**狀態算的）|
| 改之前在 `joined` 下送的 | 他本來就看不到，金鑰也本來就沒給他 ⇒ 🚫 **不需要補發**（Matrix 本來也不往前補）|

⚠️ **唯一的時間差**：拿著舊 `/members` 不放的 client 在**下一次送訊息**吃
`1506 RoomDevicesChanged`、**什麼都沒送**，重抓 `/members` 就看到新集合。
⭐ 那道閘門把「金鑰範圍過期」變成「拒絕送出」—— 方向是對的那一邊。
📎 快取也是對的：`room_versions` 的鍵是 `(room_id) → (ShortStateHash, u64)`，而新的
`m.room.history_visibility` 事件會產生**新的 shortstatehash** ⇒ 舊值對不上、自動重算。

⇒ 🚫 **不為可見性變更加任何廣播接點。** 加了要付 §3 ③ 同一種「漏了就靜默錯」的風險，
而它買到的只有「早一次送訊息知道」。

## 5. 不做什麼

- 🚫 **不改金鑰分發本身** —— ⭐ **金鑰是 client 發的**，server 只提供「誰在裡面、各自有哪些裝置」
  （`Members` ＋ 版本號）。所以這一支改的是**那份清單的範圍**，不是任何加密流程。
- 🚫 **不補發已經發出去的金鑰**（Matrix 本來就不往前補）。
- 🚫 **不碰 `knock`**（§2）。
- 🚫 **不碰 issue #111 的推邀請那一支** —— 兩件事獨立：那支是**通知**，這支是**金鑰分發的範圍**。

## 6. 測試計畫

| 要釘住的 | 怎麼測 |
|---|---|
| 🔴 `joined` 房：被邀請者**不**算 | 版本號在邀請前後**一樣**；`/members` 那則 invite **沒有** `org.wbftw.device_version` ⇒ ⭐ 這條守的是「現狀不許被這支弄寬」 |
| 🔴 `shared`／`invited` 房：被邀請者**算** | 邀請讓版本號**變**，而且 `/members` 那則 invite **有**裝置版本號 |
| 🚨 ③ 被邀請者換裝置 | `shared` 房、bob 只被邀請 ⇒ bob 登入第二台裝置 ⇒ **那間房的版本號要變、而且要廣播到**（這條沒寫就是靜默失效的那一格）|
| `knock` 不算 | 敲門不改版本號，不論可見性 |
| 可見性改變 | `joined` → `shared` 之後版本號**變**（快取沒把舊值留著）；而 client 拿舊版本號送加密訊息得到 **1506** |
| 單元：判準本身 | 四種可見性 × 四種 membership 的表格式測試，🚫 不需要 `Services` |

## 7. ✅ 全部定案（維護者 2026-10-07）

§2 的表、§4 的「不加接點」都同意了。
📎 規模：程式 ~250、文件 ~150、e2e ~150 ⇒ 約 **550 行**，所以它是**獨立一支 PR**，不併進 issue #111。
