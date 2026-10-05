# 清歷史時「釋放的集合」不等於「刪除的集合」（外部審查 2026-09-29 #3）

> ✅ **已實作**（PR #107）。維護者 2026-09-29 定：這條動手前先寫說明；2026-10-04 回了 §6，
> 結論就地寫進各節，並新增 §9（每房成員頭像）。
>
> 相關：[/docs/design/media/media-holders.md](media-holders.md)（持有者集合的語意）、
> [/docs/design/media/media-gc.md](media-gc.md)（收集器與掃描）。

## 1. 一句話

`purge_history` **先無條件釋放一整段的媒體持有者，然後才逐一刪事件 —— 而那個迴圈會刻意保留一部分事件。**
被保留的事件還活著、還指著它的媒體，可是持有者已經被拿掉、媒體已經交給收集器。

⇒ 🚨 **一個還在房間裡、client 還看得到的事件，它的媒體會被刪掉。**

## 2. 程式原本長什麼樣（已修，PR #107）

`src/service/rooms/timeline/purge.rs` 的 `purge_history(room, until, delete_local_events)`：

```rust
// ① 一整段的持有者，一次清掉，不讀任何事件
let released = self.services.media_refs
    .release_range(&mut txn, room_id, until.into_signed()).await;
txn.execute();

// ② 然後才逐一走事件，而這裡會「跳過」一部分
if pdu.state_key.is_some()
    || (!delete_local_events && self.services.globals.user_is_local(&pdu.sender))
{
    return Ok(purged);   // ← 保留：事件留在房間裡
}
```

| | 集合 |
|---|---|
| **① 釋放的** | 這個房間 `until` 之前的**每一個** `Event` 與 `Backup` 持有者（`release_range`，照 `g_seq` 掃到邊界為止）|
| **② 刪除的** | 同一段裡**非 state** ∧（`delete_local_events` ∨ 寄件者非本地）的事件 |

⇒ ⭐ **① 是 ② 的超集**，而差集就是漏洞：

1. **所有 state 事件**（永遠保留）
2. **本地寄件者的事件**，當 `delete_local_events = false`

## 3. 最具體的那個例子：房間頭像

`src/core/matrix/media_ref.rs` 的 `MXC_CONTENT_PATHS` 第一條是 `&["url"]`，而**它自己的註解就寫著**：

```rust
// Unencrypted body: m.image, m.file, m.video, m.audio, m.room.avatar.
&["url"],
```

`m.room.avatar` **是 state 事件**。而 `list_event_refs_at_store` 在 `append` 時對**每一個** PDU 都跑，
沒有 `state_key` 的過濾 —— 所以房間頭像**是**一個 `Event{room, g_seq}` 持有者。

⇒ 🚨 **清一次歷史，房間頭像的媒體被釋放、交給收集器刪掉，而 `m.room.avatar` 這個 state 事件還在。**
房間頭像從此是一個 410 墓碑。

📎 同一條也打到：`delete_local_events = false`（admin 指令可以這樣下）時，**本地使用者自己貼的每張圖**
—— 訊息留著、圖沒了。

## 4. 為什麼它會真的刪掉，而不是只是「少一個持有者」

`hand_to_collector` 在 **txn commit 時**就把 mxc 丟給跑著的收集器（`media_refs/mod.rs`），
而媒體 GC 的設計是**沒有寬限期**的（[/docs/design/media/media-gc.md](media-gc.md)）：沒有別的持有者就刪。

⇒ 所以差集裡的媒體，**只要沒有第二個持有者就是當場消失**。
📎 有第二個持有者時這次不會刪（另一個事件也指著同一張圖），所以後果是**資料相依**的 ——
⚠️ 那讓它更難查，不是更安全。

## 5. 🚨 順序本身也是錯的方向

```
release_range → txn.execute()      ← 先提交「放掉」
    （然後才）逐一 del + txn.execute()  ← 再提交「刪掉」
```

兩段是**分開的 transaction**。所以中間任何一次失敗、panic、斷電：

| 順序 | 失敗後的狀態 | 可逆嗎 |
|---|---|---|
| **原本**：先釋放、後刪除 | 媒體**已經放掉**、事件**還在** | 🚨 **不可逆**（媒體可能已被收集器刪掉）|
| 先刪除、後釋放 | 事件**已經刪了**、媒體還被持有 | ✅ 可逆（媒體是孤兒，掃描會收掉它）|
| ✅ **現在（PR #107）**：同一筆交易裡刪除**並**釋放 | 兩者一起成立，或一起不成立 | ✅ **沒有中斷窗口** |

⭐ **而這個專案自己寫過這條原則**，在 `rooms/retention/mod.rs` 的 `drop_original`：

> 「An unreadable original releases nothing: **media held too long is recoverable, media released too early is not.**」

⇒ 🚨 **`purge_history` 的順序正好違反它自己那句話。**

## 6. ✅ 維護者 2026-10-04 的決定

### 6.1 修法：**A**

> 「A 語意我同意，清理的部分應該只清理真的訊息不見的才釋放 holder。」

| | 做法 | 代價 |
|---|---|---|
| ✅ **A（定案）** | 🚫 **拿掉 `release_range` 這個整段批次**，改成**在刪除每個事件的那同一個 transaction 裡**釋放那個事件的持有者 | ⭐ 釋放的集合**由構造保證**等於刪除的集合 —— 不是靠兩處各算一次然後希望它們一致（A4）。而且釋放與刪除**同一個 txn**，⇒ §5 的中斷窗口消失。<br>⚠️ 代價是 `release_range` 的「不讀事件、一批清掉」那個效能優勢沒了：變成每個被刪的事件多一次持有者刪除（它本來就要讀那個事件、本來就要開 txn）|
| **B** | 保留批次，但**先算出要保留哪些**，把它們排除在 `release_range` 之外 | 🚨 **兩處各算一次同一個判斷**（「這個事件會被保留嗎」）—— 遲早漂，而漂的那天沒有人會收到通知。而且 §5 的順序問題還在 |
| **C** | 保留批次，但**順序反過來**：先刪事件，最後才 `release_range` | 只修了 §5 的方向，**差集問題完全沒修** —— 保留的事件照樣被釋放 |

⭐ 它把「兩個集合要相等」從一個**要維護的巧合**變成一個**結構性的事實**。

📎 **而 A 順便修掉一條沒人注意到的**：backfilled 持有者的 `g_seq` 是**負數**（`PduCount::Backfilled`
在建構時就保證 ≤ 0），而負數低於**任何**正的 `until` ⇒ 原本**每一次**清歷史都把整個 backfill 回來的
歷史的持有者全部釋放，跟邊界設在哪裡無關。A 語意下「保留的就不放」自動涵蓋它，🚫 不需要為 backfill
加任何特例。

### 6.2 已經被清掉的房間怎麼辦

這個洞不是今天才有。已經跑過 `purge_history` 的房間，**狀態已經壞了**（頭像或本地圖片的媒體沒了，而事件還在）。

| | |
|---|---|
| 🚫 不能自動修 | 媒體檔案已經不在了，重建持有者也變不回來 |
| ⏳ **仍未決定** | 要不要一條 admin 指令，**列出**「指著不存在媒體的活事件」？那是診斷不是修復 —— ⭐ 但它至少讓維護者知道損失範圍。📌 不在 PR #107 的範圍 |

### 6.3 要不要連 `Backup` 持有者一起改

`release_range` 同時清 `KIND_EVENT` 與 `KIND_BACKUP`。而 `Backup{room, g_seq}` 是「保留的原文」的持有者，
由 `purge_original` 在迴圈裡處理 —— 那一處**已經**是逐事件的，而且它的註解明說「持有者已經被
`release_range` 拿走了，所以這裡對媒體是 no-op」。

⇒ ⚠️ **選 A 的話那句註解要跟著改**（變成「這裡才是拿走它的地方」），而被保留的事件的原文也就不會被誤放。
✅ **決定：是** —— 保留的事件，它的「保留原文」也留著。實作上不需要額外的程式：`purge_original`
本來就逐事件呼叫，而它只在**真的被刪的**事件那條路上，所以 A 語意一落地，它就自然只釋放該釋放的。
📎 那句 no-op 註解已改成「⚠️ 現在這裡才是真的拿走 `Backup` 持有者的地方」。

## 7. 測試計畫（實作那支要做的）

| 測什麼 | 怎麼測 |
|---|---|
| 🚨 **房間頭像活下來** | 設一個房間頭像 → 清歷史（`until` 在頭像之後）→ 頭像的 mxc **還下載得到**，而 `m.room.avatar` 還在 |
| **本地事件的圖活下來** | `delete_local_events = false` → 本地使用者貼圖 → 清歷史 → 訊息還在、**圖還下載得到** |
| **該刪的真的刪了** | 遠端寄件者的圖、`delete_local_events = true` 的本地圖：清完 → **410** |
| **兩個事件共用一張圖** | 一個被刪一個被留 → 圖**還在**（留著的那個還持有它）|
| **順序** | ⚠️ 選 A 之後「釋放」與「刪除」在同一個 txn，所以要測的是**中斷**：刪一半失敗，已處理的事件與其媒體一致、未處理的**兩者都還在** |
| 冪等 | 同一段清兩次：第二次 0 筆、不多釋放任何東西 |

## 8. 不做什麼

- 🚫 **不改媒體 GC 的寬限期**（沒有寬限期是 [/docs/design/media/media-gc.md](media-gc.md) 定過的）——
  ⭐ 寬限期會把這個洞變成「晚一點才遺失」，那不是修好。
- 🚫 **不碰 `release_room`**（刪整個房間時清掉該房的所有持有者）：那裡事件**全部**都刪，兩個集合本來就相等。
- 🚫 **不改 `until` 的語意**（照 stream order，不是拓樸深度）。

## 9. 🚨 每房成員頭像：同一家族的另一個洞（維護者 2026-10-04 定）

### 9.1 它比 §1 那個更糟：不用任何人動手

`m.room.member` 的 **`avatar_url`**（某人「在這個房間裡」的頭像）**從來不是持有者**。
`src/core/matrix/media_ref.rs` 的 `MXC_CONTENT_PATHS` 只讀四條路徑：

```rust
&["url"],                          // m.image / m.file / m.video / m.audio / m.room.avatar
&["info", "thumbnail_url"],
&["file", "url"],
&["info", "thumbnail_file", "url"],
```

🚫 **沒有 `avatar_url`。** 而掃描的判斷是 `created < now − grace`（看**上傳時間**，不是最後引用時間），
`media_unreferenced_grace_seconds` 又有 `.max(MEDIA_UNREFERENCED_GRACE_MIN_SECONDS)` ＝ **7 天下限**。

⇒ ⭐ **設完每房頭像，第 8 天的下一次掃描就把它刪掉 —— 沒有人做過任何事、沒有錯誤、沒有 log 指向原因。**
📌 那跟 §1 的差別是「誰把它弄壞的」：§1 要有人下 `purge-history`，這條**自己會發生**。

⚠️ 要分清三個都叫「頭像」的東西：

| | 存在哪 | 持有者 |
|---|---|---|
| **房間頭像**（整個房間）| `m.room.avatar` 的 `content.url` | ✅ `Event{room, g_seq}`（`url` 在表上）—— 它的問題只有 §1 |
| **使用者全域頭像** | profile 表 | ✅ `Avatar{localpart}`，由 `set_avatar_ref` 維護 |
| **每房成員頭像** | `m.room.member` 的 `content.avatar_url` | 🚨 **本節：原本沒有** |

### 9.2 三個做法，與為什麼選 B2

| | 誰持有 | 清歷史之後 | 媒體會回收嗎 | 遷移 |
|---|---|---|---|---|
| **A** | 每個 member 事件各自持有（`avatar_url` 進 `MXC_CONTENT_PATHS`）| 全部留著（member 是 state 事件）| 🚫 **永不** —— 換幾次累積幾份 | ⚠️ **重建事件引用索引**（走全部歷史）|
| **B1** | 同 A，但被取代時主動放掉 | 當前的留著 | ✅ 會 | ⚠️ 同上 |
| ✅ **B2（定案）** | **新的 `RoomAvatar{room, user}`** | 當前的留著 | ✅ 會 | ✅ 不需要遷移 —— ⚠️ 不是因為索引沒變，是因為**沒有 `mxc_managed` 的媒體不會被刪**，見 §9.6 |

⭐ **B2 的關鍵理由：它保護的是一個「狀態」，不是一個「事件」。** 載著 `avatar_url` 的 member 事件
每次成員或 profile 變動就被取代，而只有當前那個會被當成「現在的頭像」顯示。鍵在事件上，就得在「被取代」
時釋放 —— 而那是跟 backfill、跟狀態解析翻回舊事件的競賽；鍵在 `(room, user)` 上就是**一次 swap，
沒有順序可以搞錯**。

📎 **它跟 `Avatar{localpart}` 完全同形** —— `set_avatar_ref` 就是「舊的釋放、新的持有」，因為 profile
只有「現在」。
⚠️ 但鍵用**完整的 user id** 不是 localpart：房間有遠端成員，而 localpart 當鍵正是外部審查 #2 那個洞
（`@alice:other.org` 與本地 `@alice` 變成同一個持有者）。

### 9.3 三件實作上必須處理的

1. ⚠️ **`release_room` 要跟著清 `RoomAvatar`** —— 否則刪了房間會留下一個**誰都放不掉**的持有者：
   唯一會釋放它的是「那個房間裡的下一個 member 事件」，而房間已經不在了。
2. 🚨 **只有「成為當前狀態」的 member 事件可以移動它**。守衛是 `count.into_signed() > 0`：
   backfilled 的 `g_seq` 照建構保證是負的，而拿一個舊事件去 swap 當前狀態的持有者，失敗方向是
   **提早釋放** —— ⭐「media held too long is recoverable, media released too early is not」的反面。
   📎 **今天 backfill 到不了這裡**（`backfill_pdu` 之後的 `match` 只處理 `RoomMessage`，不呼叫
   `append_member_effects`）—— 所以那道閘是給以後在 backfill 加 member 分支的人，🚫 不是現在的依賴。
   📌 維護者 2026-10-04：「backfilled 你就不應該去做任何 release media 的事」——
   ⭐ 那是一條**單向**規則（backfill 只加持有者、永不放），而單向正是可逆的那一邊。
3. **離開房間** ⇒ `membership: leave` 的 member 事件通常沒有 `avatar_url` ⇒ swap 成「沒有」⇒ 那張圖被放掉。
   那是 B2 的語意（只有當前狀態持有），要知道它會發生。📎 若那張圖同時是本人的全域頭像，
   `Avatar{localpart}` 還握著它，所以只有「只用在這個房間」的圖會死。

### 9.4 🚫 不加禁用旋鈕（維護者 2026-10-04 定）

> 「禁用旋鈕就不必了。既然有 swap 機制，就無妨，功能就讓用。」

曾經考慮過一個設定，禁止本地使用者設每房頭像（閘門現成：`api/client/state.rs` 的 `validate_member`）。
⇒ **不做**：B2 有 swap，所以成長有界，而那個旋鈕的唯一理由就是成長。
📎 而它本來也只治得到本地使用者 —— 聯邦進來的 member 事件擋不掉（擋它等於破壞房間）。

### 9.5 測試

| | |
|---|---|
| e2e9 `[2b.3]`／`[2b.6]` | 清歷史後每房頭像還在 |
| e2e9 `[2b.7]` | 換頭像 ⇒ 舊的 410、新的 200（swap 真的在動）|
| **e2e10 `[3.3]`** | 🚨 **撐過掃描** —— 那正是沒有持有者時殺掉它的那條路 |
| **e2e10 `[3.4]`** | ⭐ 同一次掃描裡，**沒人持有的上傳照樣被刪（410）** —— 不然 `[3.3]` 綠只代表「掃描壞了」|

🚨 **這一節原本寫著「7 天掃描那半測不到，因為下限是 7 天且 `.max(MIN)` 不能調低」—— 那是錯的。**
`WBFUWUNEL_MEDIA_GRACE_SECONDS` **在套用下限之前就 return**（`Config::media_unreferenced_grace_seconds_effective`），
而 e2e10 情境 3 本來就在用它（`=3`）。
⭐ 而**那句話就寫在 [/docs/design/media/media-holders.md](media-holders.md) §5**（「7 天後的掃描刪除在 e2e 用環境變數觸發」）——
也就是 cirno 在 PR #107 指出我沒掃的那份文件。
⚠️ **一份沒讀的權威文件，代價不只是文件過期 —— 我憑它宣稱了一件假的事。**

### 9.6 ✅ 不需要遷移，而理由不是「從沒上線」—— 是 `mxc_managed`（維護者 2026-10-05）

`RoomAvatar` 只在那個 `(room, user)` 的**下一則** member 事件被 append 時才出現，而 `src/service/migrations/`
裡沒有任何 walk 會替既有的當前成員狀態補上它。所以 cirno 在 PR #107 問得對：**部署前就設好的每房頭像呢？**

⭐ **決定性的事實是刪除那一端，不是持有者那一端** —— [/docs/design/media/media-holders.md](media-holders.md) §5 的判斷：

```rust
/// The one decision: local, managed by this model, and held by nothing.
/// Unknown (not managed) is "no", never "yes".
if self.managed_since(mxc).await.is_none() { return false; }   // collect.rs
```

而掃描那半**只走 `mxc_managed`**。`mxc_managed` 只由 `create_file_metadata(…, FileOrigin::NewMedia)` 寫
⇒ ⭐ **只有「這個持有者模型上線之後、在這個 fork 上傳的」媒體有那一列。**

⇒ 於是「哪一種既有資料庫的每房頭像真的有危險」只剩一種：

| 既有資料庫 | 有 `mxc_managed` 嗎 | 每房頭像有危險嗎 |
|---|---|---|
| 從 Conduit／conduwuit／上游 tuwunel **匯入** | 🚫 **一列都沒有** | ✅ **沒有** —— 它的媒體**永遠不會被自動刪**，有沒有持有者都一樣 |
| 本 fork 自己寫的，**有** `mxc_managed` 但**還沒有** `RoomAvatar` 的那個版本區間 | ✅ 有 | 🚨 有 —— ⭐ **而這一類不存在：這個 fork 從來沒有上線過** |

⭐ **而 `migrations/` 這個資料夾主要服務的正是第一列**（`import_conduit_knocks`、`split_conduit_highlight_counts`、
`fix_bad_double_separator_in_state_cache`…）—— 維護者 2026-10-05 指出的就是這件事：
這些遷移是**原生 Matrix 實作 → 這個 fork**，不是**這個 fork 的舊版 → 新版**。

⇒ 🚫 **不寫那支 walk。** 它對唯一會發生的那種資料庫（匯入的）是 **no-op**，而它要救的那種資料庫不存在。

⚠️ **我先寫了它，再拿掉**（`3bf066c38` 加，它的下一個 commit 拿掉）。留下這段是因為當時的理由錯在哪很值得記：
我寫的是「遷移是寫給『資料比程式老』那個情形的，而那個情形遲早會到」—— ⭐ **那句話本身沒錯，錯在我沒去看
刪除那一端**。「資料比程式老」確實遲早會到，但到的時候那些媒體**沒有 `mxc_managed`**，所以不在刪除的範圍裡。
📎 這跟 §9.5 是同一個形狀：**我憑一個沒查的機制宣稱了一件假的事**（那次是說掃描測不到，這次是說舊頭像會被掃掉）。

📎 真的有一天需要它（第一台 server 跑起來、再升級到更後面的版本），那時的做法記在這裡：
走每個房間的**當前成員狀態**（`room_state_keys(room, m.room.member)` → `get_member` 的 `avatar_url`），
🚫 不走歷史事件，鎖與 append 路徑同形（`hold_media` → `txn.execute()` → `drop`）。