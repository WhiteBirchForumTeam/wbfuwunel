# 邀請走自己的訂閱線（`0x05 Invite`）

issue #111。⚠️ **這份是 2026-10-09 重寫過的** —— 第一版從「線上格式」開始寫，而它的前提
（「邀請沒有水位」）是錯的，於是後面每一格都在補洞。📕 重寫的順序刻意反過來：
**先問這份資料是誰給的、什麼東西上限它、有沒有序號**，形狀是從那些界推出來的。

## 0. 一句話

邀請是**一則成員事件**，它有自己的 count；所以它跟 to-device 同一類
（/docs/design/keys/to-device.md）—— **不能漏、要補到齊** —— 於是給它自己的 kind、
自己的訂閱、自己的 `id`／`seq`／`gap`，🚫 不寄生在訊息那條線上。

## 1. 問題

wbf 帳號**收不到邀請**，而且也查不到：

| 路 | 為什麼不行 |
|---|---|
| `Event/Push`（`0x14`/`0x06`）| 只推**已加入**的房（進得了 `state_cache::is_joined` 才算）|
| 橋的 `Room/JoinedRooms`（`0x28`）| `/joined_rooms` 本身**只列已加入的** |
| 自己去問房名 | **403** —— 還沒加入就讀不到房間狀態 |

⭐ **而 server 本來就存著那份資料**（`mark_as_invited` → `userroomid_invitestate`）——
📌 只是沒有路送給 wbf client。

⚠️ **Matrix 也沒有「列出我被邀請的房」這種端點** —— 邀請**只從 `/sync` 來**。
所以這條線是 fork 自己要造的，不是把某個 Matrix 端點橋過來（這也是為什麼它不走 `0x20+`）。

## 2. 邀請是什麼，「消失」又是什麼

🚨 **事件是不可變的、只往後長；成員狀態是 state，而 state 只會被取代。**

- 邀請 ＝ 一則 `m.room.member`，`state_key` 是被邀請的人，`membership: invite`。
- 「收回邀請」**不是**把那則事件拿掉 —— 是**新增**一則 `membership: leave`
  （對被邀請者下 kick，`src/service/membership/kick.rs` 明確允許當前狀態是 `Invite`）。
  原本那則邀請的 PDU **永遠留在房間歷史裡**。
- ⇒ 所以會「消失」的只有**我們自己的快取列** `userroomid_invitestate`，它快取的是
  「這個人在這個房的當前狀態是 invite」這個推導結果。

**五種「邀請結束」：**

| 原因 | 機制 | 有事件／count？ |
|---|---|---|
| 對方收回（kick）／自己拒絕 | 新的成員事件 `leave` | ✅ |
| 被 ban | `ban` | ✅ |
| 自己接受 | `join` | ✅ |
| knock | `knock` —— ⚠️ 授權規則正常擋著 invite → knock，但**寫入路徑照樣清掉那一列** | ✅ |
| **管理員清掉整個房** | `state_cache::delete_room_join_counts` **直接刪列** | 🚫 **沒有事件、沒有 count** |

🚨 **最後一列跟前四列性質不同**：沒有事件就沒有序號 ⇒ 它**搭不上水位那條路**。
📌 §3 的完整快照仍然蓋住它（被清掉的房就是「不在快照裡」），所以它不會造成**漏**；
剩下的只是「連線中的 client 要不要**即時**知道」，那是另一張 issue。

## 3. 水位：為什麼補得齊

### 3.1 邀請跟房間事件在**同一個號碼空間**

`mark_as_invited` 收到的 `count` 就是那則成員事件的 `PduCount`，存在
`roomuserid_invitecount`（`state_cache::get_invite_count`）。而 `Event/Push` 的 `g_seq`
也是 `PduCount` ⇒ **兩個數字可以直接比大小**。
📎 跟 to-device 同一個情況（/docs/design/keys/to-device.md §2：`next_count()` 同一支）。

### 3.2 上游怎麼做的

`/sync` 組 `rooms.invite` 的那段（`src/api/client/sync/v3.rs`，`collect_invited_rooms`）：

```rust
.rooms_invited_state(sender_user)                       // 每次 sync 都全掃
let invite_count = get_invite_count(&room_id, sender_user).await.ok();
if Some(since) >= invite_count || Some(next_batch) < invite_count {
    return invited_rooms;                               // 不在水位區間 ⇒ 不送
}
```

⇒ 上游的答案是：**序號 ＋ 一個跨越一切的水位（`since` token）＋ 每次全掃再過濾**。
🚫 沒有 gap 旗標、🚫 沒有補拿端點 —— 水位把那些全吃掉了。
⭐ **注意那個上界** `Some(next_batch) < invite_count`：一輪的範圍在請求當下就固定住。

### 3.3 🚨 水位是**那張表本身**，不是 client 存的號碼

**維護者 2026-10-09 定：`Fetch` 不帶跨連線的水位，每次連線走一輪完整快照。**

理由跟 to-device 把 `cd_seq` 拿掉是**同一條**（/docs/design/keys/to-device.md §3.1.2，
issue #87）：**client 存的號碼是一個濾網，而濾網會造洞。**
邀請這邊的洞更具體 ——

> `ci_seq` 只補得到「新增」，**補不到「消失」**：那些列已經被刪了，怎麼掃都掃不到。

⇒ 帶著舊水位回來的 client 會**永遠留著一個已經被收回的邀請**。而「還存在的列」本身就是
「待處理邀請」的完整集合（刪除是真的 `del_raw`，🚫 沒有墓碑要跳過），所以：

| | |
|---|---|
| ✅ **一輪走完 ＝ 一份完整快照** | 離線期間被收回的邀請**自然不在裡面**，不需要重播 `Gone` |
| ✅ **一輪一定會結束** | 上界固定在請求當下（§3.2 那個 `next_batch`），期間新來的由 `Push` 負責 |
| ✅ **client 知道自己補完了沒有** | `tc`（這輪總共幾筆）、`r`（這批之後還剩幾筆）—— **`r == 0` 才算補完** |

📎 `ci_seq` 仍然存在，但它只是**同一輪之內的續傳游標**（補到一半斷線，從 `ns` 接著要），
🚫 不是跨連線保存的水位。

📎 **量真的大起來再說的事**：若有人被灌到幾千則邀請，每次重連重傳就會有感。那時的做法是
上游那套（跨連線保留水位 ＋ 另外掃 `userroomid_leftstate` 把「消失」補出來），⚠️ 而那是
**上游每次 sync 都在付**的成本。現在不做。

## 4. 🚫 為什麼不併進 `Event`（訊息）那條線

維護者 2026-10-09 否掉了「讓補窗同時帶邀請」，理由四條，每一條都夠硬：

| | |
|---|---|
| 🚨 **兩種水位語意不同** | 訊息是「補一段就停，除非使用者往上滾」—— 它**本來就不保證補齊**（`more` 可以一直是 true）。邀請**必須補到齊** |
| 🚨 **往上滾會拖來一堆不相干的東西** | 混在一起之後，為了補更舊的邀請要連訊息（甚至媒體）一起拉，純屬多餘 |
| ⭐ **A3：一個模組回答一種問題** | `Recent` 的 `tc`／`fs`／`ls` 是「已加入房間的訊息窗」，塞進一種沒有訊息的東西會讓它們失去意義 |
| 📎 **汙染** | 訊息處理那條路不該為了邀請多一個分支 |

⚠️ **重疊是已知且接受的取捨**：那則邀請的成員事件，之後 client 點進房間時也會在時間線裡看到。
⭐ 兩者用途不同 —— **訊息是給點進房間的人看的，邀請推播是讓 client 知道「有這個房存在」。** 不衝突。

## 5. 線上形狀：`0x05 Invite`

**新的 kind。** `0x05`–`0x0F` 在 /docs/design/wire/wire-format.md §3.3 的分配表裡寫著
「保留給 fork 自己的新功能」。⭐ 一個 kind ＝ 一條流 ＝ 一個訂閱 ＝ **自己的 `id`／`seq`／`gap`**。

📎 **形狀同構於 `0x16 Device`**（那是這個 repo 裡「不能漏、要補到齊」的既有先例），
而 subtype 編號跟 `Event`／`Device` **對齊**：

| subtype | 誰發 | meta | data |
|---|---|---|---|
| `0x01 Fetch` | client | `{ "ci_seq"?: u64, "batch"?: n }`；**`id` 由 client 選**；回應是一串 `0x02 Batch`，🚫 不是 `Ack`；**只走 WS** | 無 |
| `0x02 Batch` | **server → client** | `{ "tc", "bc", "os", "ns", "counts": [u64…], "r" }` | `bc` 筆，每筆 u32 大端長度 ＋ 一間房的 JSON |
| `0x04 Subscribe` | client | `{}`；**`id` 由 client 選**（之後每個 `Push`／`Gone` 抄它）| 無 |
| `0x05 Unsubscribe` | client | `{}`；回應 `{}`；沒訂也是 no-op | 無 |
| `0x06 Push` | **server → client** | `{ "room_id", "inviter"?, "is_direct", "reason"?, "is": <count>, "sc": n, "gap": bool }` | `sc` 則縮減狀態事件 |
| `0x07 Gone` | **server → client** | `{ "room_id", "membership", "is": <count>, "gap": bool }` | 無 |

📎 **`0x03` 故意留空**：`Event` 的 `0x03` 是 `Batch`、`Device` 的 `0x03` 是 `ItemsDestroy`，
兩邊本來就不一致。⭐ 讓 `0x04`／`0x05`／`0x06` 跟**兩條既有的流對齊**比填滿號碼值錢。

⚠️ /docs/design/wire/wire-format.md §2.2 那張 **`id` 型別表**要加一列：
`Invite/Fetch`、`Invite/Subscribe`／`Unsubscribe` 的 `id` **由 client 挑**。
🚨 那張表自己寫著「先在這張表加一列，才能發」。

### 5.1 一輪補窗怎麼走

**舊 → 新。** 📎 跟 `Device/Batch` 的 `ot`／`nt` 同一個方向，而理由具體：

> ⭐ **client 每收一批就能把續傳游標推到 `ns`**，所以補窗**中途斷線不會留洞**。
> 反過來排（新 → 舊）就只能等整輪結束才敢推進。

| 欄位 | 意思 |
|---|---|
| `tc` | 這一輪總共幾筆（請求當下、上界之內的待處理邀請數）|
| `bc` | 這批幾筆 |
| `os`／`ns` | 這批**最舊／最新**的 count |
| `counts` | 逐筆的 count，順序跟 data 一致 |
| `r` | 這批之後還剩幾筆 —— 🚨 **`r == 0` 才是補完** |

一筆的 JSON：`{ "room_id", "inviter"?, "is_direct", "reason"?, "state": [ …stripped… ] }`。
📌 **一筆就是一間房**（state 包在裡面），所以 client 🚫 不用自己算「這 7 則屬於前一間」。

⚠️ **`r` 沒有搭配 `more`**（`Event/Batch`／`Device/Batch` 都有）：那兩個的 `more` 是
「這**窗**停在上限、後面可能還有」，而邀請這一輪**沒有窗上限、跑到完為止** ⇒ 多一個永遠是
`false` 的欄位只會讓人以為它有意義。

### 5.2 三個欄位從**存起來的那份 state** 取

`inviter`／`is_direct`／`reason` 在 `0x06 Push` 與 `0x02 Batch` 都是從
`userroomid_invitestate` 裡**那則被邀請者自己的成員事件**讀出來的，🚫 不是從呼叫者手上那份。

🚨 **因為呼叫者手上那份有一條路是空的**：聯邦的 `PUT /_matrix/federation/v2/invite`
（`src/api/server/invite.rs`）傳給 `update_membership` 的是
`RoomMemberEventContent::new(MembershipState::Invite)` —— **一個全新的空內容**，
`is_direct` 永遠是 `false`、`reason` 掉了。而**真的那則 PDU 就在它存的 state 裡**
（那段 `.chain([pdu.to_format()])`）。

⭐ 兩條路都保證那則成員事件在 state 裡（本地是 `rooms::state::summary_stripped` 結尾的
`.chain(once(event.to_format()))`），所以這個取法沒有退化情形。

⚠️ **那份 state 裡有兩則 `m.room.member`** —— 邀請者的（`summary_stripped` 的清單裡有一格
是 `event.sender()`）跟被邀請者的。🚨 **「找第一則 `m.room.member`」會挑錯人**，要比對
`state_key`。測試釘這件事。

### 5.3 降級：裝不下的時候

| 情況 | 做法 |
|---|---|
| `0x06 Push` 的 data 超過 `wbf_data_max_bytes` | **照送 meta、`sc: 0`、data 空** ——「有人邀你」比房名重要，而 §5 的補窗拿得到 |
| meta 裝不下 `wbf_meta_max_bytes`（`reason` 是邀請者打的字，最大可到一則事件的上限）| **整個拿掉 `reason`**，🚫 不截斷 —— 截一半的理由當成理由顯示是在說謊，而完整的字還在 state 裡 |
| `0x02 Batch` 的一筆自己就超過上限 | 那筆照送、`state: []`；`tc`／`r` 照算，🚫 不說謊 |

⭐ **而 §6 的閘讓 `sc: 0` 變成一個無歧義的訊號**：修剪後的 state **一定含**被邀請者自己的
成員事件 ⇒ 正常的邀請 `sc` 永遠 ≥ 1，所以 **`sc == 0` 只可能是降級**。
📌 不需要再加一個 `truncated` 欄位（外部審查 2026-10-07 問過這件事）。

## 6. 🚨 進門的閘：修剪 `invite_room_state`

⚠️ 聯邦邀請的縮減狀態是**對方 server 給的，而進門時完全沒有上限** ——
`enforce_stripped_state`／`validate_stripped_create` 只驗 `m.room.create`（MSC4311），
不看數量也不看大小。上限只有 HTTP body 的 `max_request_size`（**預設 24 MiB**，而
`config/check.rs` 不准設低於 10 MB）。

⇒ 一個惡意或寫壞的遠端，**每一則邀請可以塞 24 MiB 進我們的資料庫**。後果不只在 wbf：
`userroomid_invitestate` 的**儲存放大**（邀請轟炸是現成手法），以及 `/sync` 的
`rooms.invite` 也會把那包送給原生 Matrix client。

**所以在存之前修剪**（`src/api/server/invite.rs`），三條規則：

| | 規則 | 為什麼 |
|---|---|---|
| ① | 只留**建議的那幾格**（`create`／`join_rules`／`canonical_alias`／`name`／`avatar`／`encryption`／`topic` ＋ 成員事件）| 那就是 spec 建議放的東西，也是 `summary_stripped` 自己送出去的清單；其餘對被邀請者沒有用途 |
| ② | 丟掉**單則大於 65536 bytes** 的 | ⭐ 那是 Matrix 單則事件的硬上限 —— **比合法事件還大的東西本來就不是合法的狀態事件** |
| ③ | 留下的格數上限 **16** | ①＋② 之後總量 ≤ 1 MiB ⇒ **位元組上限是推導出來的**，🚫 不用再開一個設定 |

🚫 **不是整則邀請拒收** —— 邀請太胖是邀請者那邊的問題，拒收是處罰被邀請的人。⭐ 修剪，不退件。

🔴 **順序不能顛倒**：`validate_stripped_create`（MSC4311 那道）要**先跑在原始的
`body.invite_room_state` 上**，再修剪我們要存的那份。顛倒了就是悄悄削弱一道既有的安全檢查。

📎 spec 面站得住：`invite_room_state` 在 spec 裡是 **OPTIONAL、best-effort**（「may include」），
所以修剪是合規的，🚫 不是 breaking。
📎 `mark_as_knocked` 存的 knock state 走同一條路、同一個洞 ⇒ 同一個修剪函數一起用。

## 7. 推送的接點：**一處**，不是每個分支一處

⭐ **跟 /docs/design/events/event-push.md §3 接點 2 同一個做法**：掛在**寫入點**而不是 handler ——
邀請可能來自 client API、來自聯邦、來自 admin，而寫入點只有一個。

`update_membership`（`src/service/rooms/state_cache/update.rs`）：

```
let was_invited = is_invited(user, room).await;   // 🔴 在任何寫入之前讀一次
match membership { … }                            // 分支只管寫，不推
if was_invited && ends_invite(&membership) {      // 🔴 推一次
    push_invite_gone(user, room, membership.as_str(), count);
}
```

🚨 **第一版把它掛在每個 match 分支上，於是漏掉 `Knock`** —— 而 `mark_as_knocked` 一樣會
`del_raw(userroomid_invitestate)`。⭐ **漏掉不是意外，是那個形狀的必然**：下一個加分支的人也會漏。
📌 `ends_invite` ＝ `matches!(Join | Leave | Ban | Knock)`，**一處清單**，對得上「哪三個
`mark_as_*` 會清掉那一列」；🚫 `_ => {}` 那個分支不推，因為它根本沒刪，推了就是說謊。

**被邀請**那條在 `MembershipState::Invite` 的分支裡，`mark_as_invited` **之後**推
（要把剛寫進去的 state 讀回來，§5.2）。
📎 **被 ignore 的邀請在 `mark_as_invited` 之前就 return** ⇒ 不落庫也不推，與 Matrix 的忽略語意一致。
⚠️ **但「邀請之後才把對方加入 ignore」不過濾** —— 查過 `/sync` 也不過濾（`collect_invited_rooms`
直接從 `rooms_invited_state` 組，`ignored_filter` 只用在時間線與回條）⇒ **刻意跟上游一致**。

**推給誰**：`0x05 Invite` 自己的 topic（每個 user 一個，Occupancy 是 `Many`——一個人可以有
好幾條連線）。🚫 **不是 `RoomTopic::FollowsJoins`**（那是訊息那條訂閱的 topic，§4 說了不混）、
🚫 更不是 `listeners(room)`（被邀請的人**還沒加入**，不在那個房的 topic 裡）。
📎 實作上就是 `Streams` 多一個 `invites: Subscribers<InviteTopic>`，跟既有的
`rooms`／`devices` 並列。

## 8. client 的義務

| | |
|---|---|
| ① | 連線後 **`Invite/Subscribe` ＋ `Invite/Fetch`**，一直要到 **`r == 0`** ——🚨 補完才算有完整清單 |
| ② | 一輪補完之後，用結果**整份取代**本地清單，🚫 不要 merge（被收回的邀請靠「不在快照裡」才發現）。⚠️ 補到一半的結果**不是**快照，不能拿去取代 |
| ③ | `Push` 與 `Gone` 都**按 `room_id` 取代**，🚫 不是追加 —— 拒絕後被重新邀請會再來一次（新事件、更大的 `is`）；而 `is` 比手上更舊的可以直接忽略 |
| ④ | 看到 `gap: true` ⇒ 重跑一輪 `Fetch`。📎 這條流的 `gap` **只關於邀請**，不會跟訊息那條混 |
| ⑤ | `sc: 0`（§5.3）⇒ 想要那間房的預覽就從補窗拿 |

⚠️ 以上是 client 這側的契約 ⇒ 合併後要在 `amaid/wbf-matrix-client` 開同步 issue
（跟 /docs/design/keys/to-device.md §9 同一個做法）。

## 9. 不做什麼

- 🚫 **不改 `invite_state` 的縮減邏輯**（Matrix 規格定的那幾個 type）—— §6 只砍掉**不合法或多餘**的。
- 🚫 **不把被邀請的房塞進 `/joined_rooms`**（橋過去的端點，改它就是破 Matrix 契約）。
  📎 要一個「我所有的房 ＋ 成員狀態」的清單，那是另開一個原生 `Room/List`，不是改那支。
- 🚫 **不改房間版本號的語意** —— 被邀請者算不算進裝置清單在
  /docs/design/keys/invite-key-scope.md，另一支。
- 🚫 **新的 server → client subtype 不加 feature 旗標**（不像 `Event/DeviceChanged`）：
  擋錯了的失敗方式**正是這個功能要修的 bug**（client 靜默收不到邀請），而
  `DeviceChanged` 被擋是因為它的內容只對做 E2EE 版本比對的 client 有意義、而且**要先算**。
  ⭐ 協定本來就要求 client 忽略認不得的 frame。

## 10. 測試要釘住的

| | |
|---|---|
| 🔴 補得齊 | 連線全關時被邀請 → 重連 → `Fetch` 一輪拿得到；`r` 遞減到 0；`tc` 對得上 |
| 🔴 **被收回的不在快照裡** | 離線期間邀請被 kick 掉 → 重連一輪 ⇒ **不列它**（§3.3 的核心） |
| 🔴 只給被邀的那個人 | 另一個也 `Invite/Subscribe` 著、沒被邀的 user ⇒ 收不到 |
| 🔴 不混線 | 訊息那條訂閱（`Event/Subscribe`）**收不到**邀請推播；`gap` 各自獨立 |
| 🚨 單純離開不是 `Gone` | 從沒被邀請過的人離開自己的房 ⇒ 不推（釘「寫入前讀 `is_invited`」）|
| 🚨 knock 也要推 `Gone` | §7 那個一處接點的反面測試 |
| 🚨 挑對成員事件 | state 裡有兩則 `m.room.member` ⇒ 衍生欄位取的是**被邀請者**那則 |
| 🚨 閘 | 超大的 `invite_room_state` 進來 ⇒ 存下來的被修剪、**成員事件還在**、`validate_stripped_create` 仍然先跑 |
| | 分批：`batch` 小於總數 ⇒ 多個 `Batch`，`counts` 與 data 筆數一致 |
| | HTTP 走不通（`Fetch`／`Subscribe` ⇒ `Unsupported`）|

## 11. ✅ 維護者定的

| 日期 | 定了什麼 |
|---|---|
| 2026-10-07 | meta ＋ data 分開；`sc: 0` 的取捨 |
| 2026-10-09 | 🚫 **不併進訊息那條線**（§4）；**仿 `Subscribe` 但給水位**，另開一條訂閱；補窗語意「每要一批就回還剩多少」；方向我挑（§5.1 挑了舊→新）；**(甲) 每次連線一輪完整快照**，不留跨連線水位（§3.3）|

📕 **收回的號碼**：第一版用過 `Event/Invited`（`0x14`/`0x08`）、`Event/InviteGone`（`0x14`/`0x09`）、
`Room/InvitedRooms`（`0x13`/`0x01`）—— 三個**全部收回**，`0x13 Room` 回到「沒有原生 subtype」。
⚠️ 那三個號碼從沒上線過（client 還沒實作），所以收回是乾淨的。
