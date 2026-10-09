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
剩下的只是「連線中的 client 要不要**即時**知道」，而那是 issue #116。

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
| ⭐ **一條流回答一種問題** | `Recent` 的 `tc`／`fs`／`ls` 是「已加入房間的訊息窗」，塞進一種沒有訊息的東西會讓它們失去意義 |
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
| `0x02 Batch` | **server → client** | `{ "tc", "bc", "os"?, "ns"?, "counts": [u64…], "r" }`；🚨 `bc == 0` 時**不帶** `os`／`ns`（見 §5.1）| `bc` 筆，每筆 u32 大端長度 ＋ 一間房的 JSON |
| `0x04 Subscribe` | client | `{}`；**`id` 由 client 選**（之後每個 `Push`／`Gone` 抄它）；回應 `{ "latest_count" }` —— **註冊完成的那一刻**的位置，所以 client 知道接下來那一輪 `Fetch` 涵著哪些推播。⚠️ 註冊**之後**才讀：反過來會讓 client 以為自己看過一則它沒看過的邀請 | 無 |
| `0x05 Unsubscribe` | client | `{}`；回應 `{}`；沒訂也是 no-op | 無 |
| `0x06 Push` | **server → client** | `{ "room_id", "inviter"?, "is_direct", "reason"?, "is": <count>, "sc": n, "gap": bool }` | `sc` 則縮減狀態事件 |
| `0x07 Gone` | **server → client** | `{ "room_id", "membership", "is": <count>, "gap": bool }` | 無 |

📎 **`0x03` 故意留空**：`Event` 的 `0x03` 是 `Batch`、`Device` 的 `0x03` 是 `ItemsDestroy`，
兩邊本來就不一致。⭐ 讓 `0x04`／`0x05`／`0x06` 跟**兩條既有的流對齊**比填滿號碼值錢。

⚠️ /docs/design/wire/wire-format.md §2.2 那張 **`id` 型別表**要加一列：
`Invite/Fetch`、`Invite/Subscribe`／`Unsubscribe` 的 `id` **由 client 挑**。
🚨 那張表自己寫著「先在這張表加一列，才能發」。

⭐ **而 `Kind` 這個 enum 有一條測試在守這件事**：`all_kind_bytes_round_trip`（`src/core/wbf/pack.rs`）
掃全 256 個 byte 數有幾個解得開，並把那個數寫成斷言 —— 📌 加一個 kind 就會紅，而它的
訊息直接指著 §3.3 那張分配表。這一支把它從 19 改成 20。

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

🚨 **空批次（`bc == 0`）不帶 `os`／`ns`。** 一輪當中整個 chunk 的邀請都剛好結束時就會出現空批次，
而 client 正在拿 `ns` 推進續傳游標 —— ⭐ **那裡放一個 0 會把游標往後拉到開頭**（外部審查
2026-10-09）。📌 「讀起來像資料的佔位值」比「欄位不存在」更危險，所以是不帶而不是帶 0。

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
`is_direct` 永遠是 `false`、`reason` 掉了（issue #117）。而**真的那則 PDU 就在它存的 state 裡**
（那段 `.chain([pdu.to_format()])`）。

⭐ 兩條路都保證那則成員事件在 state 裡（本地是 `rooms::state::summary_stripped` 結尾的
`.chain(once(event.to_format()))`），所以這個取法沒有退化情形。

### 🚨 5.2.1 不變量：名字是被邀請者的成員事件**只有一則，而且是我們造的**

⚠️ **「從存起來的那份讀」本身不構成安全論證** —— 那個陣列裡混著兩種來源：

| 來自 | 誰驗過 |
|---|---|
| 對方 server 的 `invite_room_state` | 🚫 **沒有人**。內容由它決定，`sender` 連網域都不必是它自己的 |
| 我們自己接上去的那則 PDU | ✅ `validate_origins` 驗過 |

🚨 **第一版的兩個決定剛好對立**：存的時候我們的那則接在**最後**（`.chain`），而讀的時候取的是
**第一則**（`.find`）⇒ 對方只要在 `invite_room_state` 裡放一則「被邀請者的成員事件」，
`inviter`／`is_direct`／`reason` 就全由它決定（外部審查 2026-10-09，oliver 與 salvia）。
📎 ⚠️ 原生 Matrix client 多半是「後者覆蓋前者」，所以 wbf 這條線反而**比原生更容易被騙**。

**修法是建立不變量，而不是換一個位置規則**：

1. 🔴 **進門就丟掉對方給的那一格** —— `list_stripped_state_without_member_of(state, 被邀請者)`，
   跑在 `list_stripped_state_kept` 之後、接上我們那則之前。⇒ 存進去的 state 裡，名字是被邀請者的
   成員事件**恰好一則**。
2. 📎 **讀的時候取最後一則**（`find_own_member_event`）當第二道鎖 —— 讓一個忘了第 1 條的新寫入點
   也不會讓偽造的那則贏。

🚫 **單靠第 2 條不夠**：`.chain` 的尾碼順序是每個寫入點自己的實作細節，不是契約；靠它就等於
要求下一個加寫入點的人記得一個沒人會告訴他的約定（salvia 的話：**形狀保證不了的事，別靠列舉**）。

⚠️ **那份 state 裡本來就有兩則 `m.room.member`** —— 邀請者的（`summary_stripped` 的清單裡有一格
是 `event.sender()`）跟被邀請者的。所以比對 `state_key` 是必要的，但**還不夠**：上面那兩條才是。
📌 三件事各有測試釘住（挑對人、偽造的不贏、進門就丟掉）。

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

🔴 **而「並列」還不夠：三條生命週期路徑都要清它。** `remove_connection`（換身分）、
`end_connection`（登出）、`forget_connection`（連線結束）—— 🚨 第一版那三個函數各自手寫
`rooms` 與 `devices` 的清單，而它們的註解寫著「One place, so a new stream cannot be added
without its cleanup」⇒ **註解宣稱的性質它並不具備**，於是加了邀請這條流卻沒接上
（外部審查 2026-10-09，salvia）。後果是斷線留幽靈訂閱、而且換身分之後會把**前一個 user 的
邀請推給新身分**（跟 PR #43 修掉的 device queue 是同一個 bug 的鏡像）。
⇒ 改成 `fn streams(&self) -> [&dyn ConnectionLifecycle; 3]` **一處列出**，三個函數走它；
一條測試對三條路徑各驗一次。

## 8. client 的義務

| | |
|---|---|
| ① | 連線後 **`Invite/Subscribe` ＋ `Invite/Fetch`**，一直要到 **`r == 0`** ——🚨 補完才算有完整清單 |
| ② | 一輪補完之後，用結果**整份取代**本地清單，🚫 不要 merge（被收回的邀請靠「不在快照裡」才發現）。⚠️ 補到一半的結果**不是**快照，不能拿去取代。🔴 **但「取代」的對象不含 ②' 那些**（見下）|
| ②' | 🚨 **`Subscribe` 之後收到的 `Push`／`Gone` 要疊在快照上** —— 每間房以 **`is` 最大**的那筆為準。只有「本地舊清單裡有、這一輪沒出現、而且這段期間沒被 `Push`／`Gone` 碰過」的房才因為「不在快照裡」而刪掉 |
| ③ | `Push` 與 `Gone` 都**按 `room_id` 取代**，🚫 不是追加 —— 拒絕後被重新邀請會再來一次（新事件、更大的 `is`）；而 `is` 比手上更舊的可以直接忽略 |
| ④ | 看到 `gap: true` ⇒ 重跑一輪 `Fetch`。📎 這條流的 `gap` **只關於邀請**，不會跟訊息那條混 |
| ⑤ | `sc: 0`（§5.3）⇒ 想要那間房的預覽就從補窗拿 |

🚨 **②' 是外部審查 2026-10-09（oliver）補上的，而少了它 ② 會自己造出漏掉**：

| 時間 | 發生什麼 | 少了 ②' 的後果 |
|---|---|---|
| 補窗跑到一半 | 新邀請進來，它的 count **大於這一輪的上界** ⇒ 只會以 `Push` 送達，🚫 不在快照裡 | `r == 0` 時整份取代 ⇒ **那則邀請消失**，要到下次重連才回來 |
| 同樣在補窗期間 | 某間房的條目已經被 `read_entries` 讀出來、還沒送出，而它在那之間結束 ⇒ `Gone` 可能**比**那個 `Batch` 先到 | 整份取代之後留下一個**幽靈邀請** |

⭐ **而這條規則自洽的理由是 `is` 跟 `counts` 是同一個號碼空間**（§3.1）：兩邊講的是同一條時間線，
所以「取 `is` 最大的那筆」永遠分得出誰比較新。

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

## 10. 測試

**e2e11 情境 6，20 條**（`tests/e2e/e2e11.ps1`，跑在前幾個情境的同一台 server 上）：

| | 釘住什麼 |
|---|---|
| `[6.0]` | `Subscribe` 的 `latest_count` |
| `[6.1]`／`[6.1b]`／`[6.1c]` | 推得到：`id` 抄 `Subscribe`、`inviter` 對、`is` > 0、`sc` 跟 data 的則數一致且 **≥ 1**、含**他自己的** member 事件 |
| 🔴 `[6.2]` | **只給被邀的那個人** —— 另一個也 `Invite/Subscribe` 著、沒被邀的 user 收不到 |
| 🔴 `[6.3]` | **不混線** —— 只訂了訊息（`Event/Subscribe`）的那條連線收不到邀請推播 |
| `[6.4]`／`[6.4b]`／`[6.4c]` | 一輪 `Fetch` 列得齊、`r` 收在 0、`tc` 對得上；**舊 → 新**；而那一筆跟推播說的是同一件事（state 則數、`inviter`、`is_direct`）|
| `[6.5]` | `Fetch` 與 `Subscribe` 走 HTTP 都是 `Unsupported` |
| `[6.6]`／`[6.6b]` | 拒絕 ⇒ `Gone(leave)` 帶 `is`，而且下一輪不再列它 |
| `[6.7]` | 接受 ⇒ `Gone(join)` |
| 🚨 `[6.8]` | **單純離開不是 `Gone`** —— 從沒被邀請過的人離開自己的房 ⇒ 不推（釘「寫入前讀 `is_invited`」）|
| 🔴 `[6.9]` | 連線全關時進來的邀請，重連一輪補得到 |
| 🔴 `[6.10]` | **離線期間被收回的不在快照裡** —— ⭐ §3.3 的核心，也是 `ci_seq` 做不到的那一格 |
| `[6.11]`／`[6.11b]` | `batch = 1` ⇒ 一房一個 pack、`seq` 從 0 遞增、`r` 收到 0、`tc` 每個 pack 一致、`os` 只往前 |
| `[6.12]`／`[6.12b]` | `Unsubscribe` 之後不再推，而漏掉的那則還在下一輪的補窗裡 |

📎 **e2e 到不了、改由單元測試守的兩格**（16 條，`core` 5、`service` 7、`api` 4）：

| | 為什麼 e2e 到不了 | 誰守 |
|---|---|---|
| §6 的閘 | 它只對**別的 server 送來**的 state 生效，而 e2e 是單機 | `list_stripped_state_kept` 4 條（留建議的 type、丟超大的、格數上限、空的不炸）|
| §7 的 `Gone(knock)` | 授權規則禁止 invite → knock，沒有 API 叫得到那個分支 | 結構上成立：接點是 match **之後**一處，不是每個分支一處 |
| §5.2 挑對成員事件 | 要構造「兩則 member 事件」的 state | `get_invite_fields` 的那條測試（🚨 state 裡確實有兩則，邀請者的在前面）|
| §5.3 兩個降級 | 要構造超過 2 MiB 的 state／60 KiB 的 `reason` | `framed_state`／`invited_meta` 各一條 |

⚠️ **`Call` 這個測試輔助函式這一支改成看 `gap` 欄位**，不再逐個列 subtype ——
📎 第一版列了 `0x14/0x06` 一個，而 `0x14/0x07` 也會打爛它（只是被 feature 擋著從沒浮現），
加邀請推播時就真的弄壞了 `[1.7b]`。⭐ **每個 server → client 的推播都帶 `gap`，而回覆從不帶**
（`Ack`、`Error`、三種 `Batch` 都沒有）⇒ 以後加新推播不用再改它。

## 11. ✅ 維護者定的

| 日期 | 定了什麼 |
|---|---|
| 2026-10-07 | meta ＋ data 分開；`sc: 0` 的取捨 |
| 2026-10-09 | 🚫 **不併進訊息那條線**（§4）；**仿 `Subscribe` 但給水位**，另開一條訂閱；補窗語意「每要一批就回還剩多少」；方向我挑（§5.1 挑了舊→新）；**(甲) 每次連線一輪完整快照**，不留跨連線水位（§3.3）|

📕 **收回的號碼**：第一版用過 `Event/Invited`（`0x14`/`0x08`）、`Event/InviteGone`（`0x14`/`0x09`）、
`Room/InvitedRooms`（`0x13`/`0x01`）—— 三個**全部收回**，`0x13 Room` 回到「沒有原生 subtype」。
⚠️ 那三個號碼從沒上線過（client 還沒實作），所以收回是乾淨的。
