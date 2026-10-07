# 📄 提案：邀請走訂閱線 —— 推一則、沒了也推一則，再加一支列出待處理邀請

⚠️ **這是提案，等維護者同意才動 `src/`。** issue #111（維護者要求優先；client 的
`amaid/wbf-matrix-client` 分支 `feat/room-actions` 正等它）。
相關：[/docs/design/events/event-push.md](event-push.md)（訂閱線的規則）、
[/docs/design/wire/wire-format.md](../wire/wire-format.md) §6.3（kind／subtype 分配）。

## 1. 問題：wbf 帳號收不到邀請，而且也查不到

| 路 | 為什麼不行 |
|---|---|
| 訂閱線的 `Event/Push` | 只推**已加入**的房（`event-push.md` §1：`state_cache::is_joined` 才進得了）|
| 橋的 `Room/JoinedRooms`（`0x28`）| 只列已加入的 |
| 自己去問房名（`GetState`／`GetStateEvent`）| **403** —— 還沒加入就讀不到房間狀態 |

⇒ 🚨 **client 沒有任何來源知道「誰邀我進哪間房、那間房叫什麼」。**

⭐ **而 server 其實已經存著那份資料**：`state_cache/update.rs` 的 `mark_as_invited` 把縮減狀態寫進
`userroomid_invitestate`，`state_cache/mod.rs` 的 `invite_state()` 讀得出來
（型別 `Vec<Raw<AnyStrippedStateEvent>>`）—— 📌 **只是沒有路送給 wbf client。**
📎 那正是標準 Matrix `/sync` 的 `rooms.invite.*.invite_state` 給的東西，所以這不是新資料，是新的送法。

## 2. 分配：兩個新的 `0x14 Event` subtype ＋ 一個新的 `0x13 Room` 原生

| | 號碼 | 方向 | 為什麼放這裡 |
|---|---|---|---|
| `Event/Invited` | `0x14` / **`0x08`** | 只有 server → client | ⭐ 它騎在**帳號層的 `Event/Subscribe`** 上，`id`／`seq`／`gap` 跟那條連線的 `Push` 共用 —— 跟 `0x07 DeviceChanged` 同一個形狀，所以同一個 kind |
| `Event/InviteGone` | `0x14` / **`0x09`** | 只有 server → client | 同上 |
| `Room/InvitedRooms` | `0x13` / **`0x01`** | 請求／回應（`id` 型別 `0x00`）| 跟 `Room/JoinedRooms`（`0x28`，走橋）對稱，而這支**沒有對應的 Matrix 端點**可以橋（那份資料只在 `/sync` 裡）⇒ 原生。📎 `0x13` 目前**沒有任何原生 subtype**，`0x01` 是乾淨的空位 |

📎 `0x14` 的原生用到 `0x07`（`DeviceChanged`），所以 `0x08`／`0x09` 沒撞號；原生只用 `0x01`–`0x1F`，走橋的從 `0x20` 起（`wire-format.md` §6.3）。

## 3. 線上的形狀

⚠️ **跟 issue #111 裡 client 畫的草圖有一處刻意不同，而我認為該照這裡**：

issue 的草圖把整包塞進一個 JSON（`{ "invite": { …, "state": [...] } }`）。
🚫 **不要那樣** —— ⭐ **pack 的慣例是「subtype 說它是什麼，meta 放欄位，一串事件放 data」**，
而 `Event/Batch` 已經定了「一串事件」的框法（每則 u32 大端長度 ＋ 事件 JSON）。
📌 縮減狀態是**一串事件**，所以它走 data、沿用同一個切法：

### 3.1 `Event/Invited`（`0x14` / `0x08`）

```jsonc
// meta
{ "room_id": "!r:localhost", "inviter": "@alice:localhost",
  "is_direct": true, "reason": "來吃飯",          // 兩個都可省（沒有就不帶）
  "sc": 7, "gap": false }                          // sc = data 裡幾則縮減狀態事件
```
**data**：`sc` 則，每則 `u32` 大端長度 ＋ 那則 stripped state event 的 JSON 原樣
（`m.room.create`、`m.room.name`、`m.room.avatar`、`m.room.join_rules`、`m.room.canonical_alias`、
`m.room.encryption`，以及邀請的那則 `m.room.member`）。

⚠️ **`invite_state` 的內容由 server 端既有的邏輯決定，這一支不改它** —— 原樣轉送。
🚨 **超過 `wbf_data_max_bytes` 時怎麼辦**：⭐ **照送 meta、data 截成 0 則（`sc: 0`）**，
而不是整則不推 —— 因為「有人邀你」這件事比「房名是什麼」重要得多，而 client 有 §3.3 可以補拿。
📌 這一條是刻意的 fail-open **方向選擇**，但它放行的是**通知**而不是權限，所以安全上是中性的。

### 3.2 `Event/InviteGone`（`0x14` / `0x09`）

```jsonc
{ "room_id": "!r:localhost", "membership": "leave", "gap": false }
```

`membership` 是**造成邀請消失的那個新狀態**：`leave`（對方撤回、自己拒絕）、`ban`、`join`（自己接受）。
📎 `join` 那個 client 也能從別處推知，但推它讓「邀請清單怎麼變空的」只有一個來源。

### 3.3 `Room/InvitedRooms`（`0x13` / `0x01`）

請求 meta `{}`。回應 meta：

```jsonc
{ "rc": 2 }            // rc = data 裡幾間房
```
**data**：`rc` 筆，每筆 `u32` 大端長度 ＋ 一個 JSON 物件
`{ "room_id", "inviter"?, "is_direct"?, "reason"?, "state": [ …stripped… ] }`。

📎 這裡把 `state` 放進那個 JSON 物件（而不是再切一層）是因為**一筆就是一間房**，
而框法要讓「一間房」是一個可獨立解析的單位 —— 🚫 不要讓 client 去算「這 7 則屬於前一間」。
⚠️ **只走 WS**（跟 `Event/Recent` 同一個理由：它是連線狀態的補拿），HTTP 回 `Error(Unsupported)`。

## 4. 推送的接點

| 事件 | 接點 | 推什麼 |
|---|---|---|
| 被邀請 | `state_cache/update.rs` 的 **`mark_as_invited`**（`:342`）| `Event/Invited` |
| 邀請沒了 | 同一個檔案裡 invite → leave／ban／join 的那條路（`mark_as_left`／`update_membership` 的分支）| `Event/InviteGone` |

⭐ **跟 `event-push.md` §3 接點 2 同一個做法**：掛在**寫入點**而不是在 handler 裡 ——
邀請可能來自 client API、來自聯邦、來自 admin，而寫入點只有一個。

**推給誰**：`subscribers.rs` 的 `by_user`（user → 連線集合）就是答案 ——
🚨 **不能用 `listeners(room)`**（那是 room-keyed，而被邀請的人**還沒加入**那個房，所以不在裡面）。
📎 那正是這一支跟 `Push`／`DeviceChanged` 唯一不同的地方。

⚠️ **只推給帳號層訂閱**（`Event/Subscribe` 沒點名 `rooms`）：點名了房間的訂閱是「我只要這幾間」，
而邀請是**還不在清單裡的房** —— 推給它等於違反它自己說的範圍。

## 5. 不做什麼

- 🚫 **不改 `invite_state` 的內容**（那是 server 既有的縮減邏輯，Matrix 規格定的那幾個 type）。
- 🚫 **不改房間版本號的語意** —— issue #111 末尾提到「`history_visibility` 是 `shared`／`invited` 時
  要不要把被邀請者算進版本號」，那是**另一件事**，client 那邊還在跟維護者討論，定了會另開 issue。
- 🚫 **不碰 `Event/Recent`** —— 讓它帶 invites 是 issue 給的另一個選項，🚫 不採：`Recent` 的語意是
  「已加入房間的訊息窗」，塞進一種沒有訊息的東西會讓它的 `tc`／`fs`／`ls` 失去意義（A3：一個模組回答一種問題）。

## 6. 測試計畫

| 要釘住的 | 怎麼測 |
|---|---|
| 🔴 **被邀請時推得到** | e2e：bob 帳號層訂閱 → alice 建房並邀 bob → bob 收到 `0x14/0x08`，meta 的 `room_id`／`inviter` 對，data 解出來含 `m.room.name` |
| 🔴 **只推給被邀的那個人** | 🚨 carol 也訂閱著、不在那間房、沒被邀 ⇒ **carol 收不到**（這條守的是「不要廣播邀請」）|
| 🔴 **點名房間的訂閱收不到** | 同一個 bob 另開一條連線、`Subscribe` 點名別的房 ⇒ 那條**收不到**（§4 最後那條規則）|
| 撤回／拒絕／封鎖／接受 | 四條各推一則 `InviteGone`，`membership` 各自對 |
| 🔴 **補拿對得上推送** | 斷線期間被邀 → 重連 → `InvitedRooms` 列得到那間（⭐ 這條才是「推送是提示不是保證」的實證）|
| 接受之後不再列 | `InvitedRooms` 不含已加入的房 |
| `state` 太大 | 🚨 單元測試：`sc: 0` ＋ meta 照送，而**不是**整則不推（§3.1 那個取捨）|

## 7. ⏳ 要維護者點頭的三件

1. **§3 那個形狀**（meta ＋ data 分開，而不是 issue 草圖的單一 JSON）—— 📌 它是**線上契約**，
   改一次要 client 跟著發版，所以值得先確認。
2. **§3.1 的 `sc: 0` 取捨**（state 太大時照推、data 清空）。
3. **`Room/InvitedRooms` 用 `0x13`/`0x01`**（原生）而不是塞進 `0x12 Sync` 或讓 `Recent` 帶。
