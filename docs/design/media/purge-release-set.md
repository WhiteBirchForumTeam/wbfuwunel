# 清歷史時「釋放的集合」不等於「刪除的集合」（外部審查 2026-09-29 #3）

> 📄 **設計說明，還沒實作。** 維護者 2026-09-29 定：這條**動手前先寫說明**。
> ⏳ §6 有要維護者決定的事；決定之前 🚫 不動 `src/`。
>
> 相關：[/docs/design/media/media-holders.md](media-holders.md)（持有者集合的語意）、
> [/docs/design/media/media-gc.md](media-gc.md)（收集器與掃描）。

## 1. 一句話

`purge_history` **先無條件釋放一整段的媒體持有者，然後才逐一刪事件 —— 而那個迴圈會刻意保留一部分事件。**
被保留的事件還活著、還指著它的媒體，可是持有者已經被拿掉、媒體已經交給收集器。

⇒ 🚨 **一個還在房間裡、client 還看得到的事件，它的媒體會被刪掉。**

## 2. 程式長什麼樣

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

| 現在的順序 | 失敗後的狀態 | 可逆嗎 |
|---|---|---|
| 先釋放、後刪除 | 媒體**已經放掉**、事件**還在** | 🚨 **不可逆**（媒體可能已被收集器刪掉）|
| 先刪除、後釋放 | 事件**已經刪了**、媒體還被持有 | ✅ 可逆（媒體是孤兒，掃描會收掉它）|

⭐ **而這個專案自己寫過這條原則**，在 `rooms/retention/mod.rs` 的 `drop_original`：

> 「An unreadable original releases nothing: **media held too long is recoverable, media released too early is not.**」

⇒ 🚨 **`purge_history` 的順序正好違反它自己那句話。**

## 6. ⏳ 要維護者決定的

### 6.1 修法選哪個

| | 做法 | 代價 |
|---|---|---|
| **A（我建議）** | 🚫 **拿掉 `release_range` 這個整段批次**，改成**在刪除每個事件的那同一個 transaction 裡**釋放那個事件的持有者 | ⭐ 釋放的集合**由構造保證**等於刪除的集合 —— 不是靠兩處各算一次然後希望它們一致（A4）。而且釋放與刪除**同一個 txn**，⇒ §5 的中斷窗口消失。<br>⚠️ 代價是 `release_range` 的「不讀事件、一批清掉」那個效能優勢沒了：變成每個被刪的事件多一次持有者刪除（它本來就要讀那個事件、本來就要開 txn）|
| **B** | 保留批次，但**先算出要保留哪些**，把它們排除在 `release_range` 之外 | 🚨 **兩處各算一次同一個判斷**（「這個事件會被保留嗎」）—— 遲早漂，而漂的那天沒有人會收到通知。而且 §5 的順序問題還在 |
| **C** | 保留批次，但**順序反過來**：先刪事件，最後才 `release_range` | 只修了 §5 的方向，**差集問題完全沒修** —— 保留的事件照樣被釋放 |

📌 **我建議 A**，理由是它把「兩個集合要相等」從一個**要維護的巧合**變成一個**結構性的事實**。

### 6.2 已經被清掉的房間怎麼辦

這個洞不是今天才有。已經跑過 `purge_history` 的房間，**狀態已經壞了**（頭像或本地圖片的媒體沒了，而事件還在）。

| | |
|---|---|
| 🚫 不能自動修 | 媒體檔案已經不在了，重建持有者也變不回來 |
| ⏳ 要決定的 | 要不要一條 admin 指令，**列出**「指著不存在媒體的活事件」？那是診斷不是修復 —— ⭐ 但它至少讓維護者知道損失範圍 |

### 6.3 要不要連 `Backup` 持有者一起改

`release_range` 同時清 `KIND_EVENT` 與 `KIND_BACKUP`。而 `Backup{room, g_seq}` 是「保留的原文」的持有者，
由 `purge_original` 在迴圈裡處理 —— 那一處**已經**是逐事件的，而且它的註解明說「持有者已經被
`release_range` 拿走了，所以這裡對媒體是 no-op」。

⇒ ⚠️ **選 A 的話那句註解要跟著改**（變成「這裡才是拿走它的地方」），而被保留的事件的原文也就不會被誤放。
📌 要維護者確認：**保留的事件，它的「保留原文」也應該留著**（我認為是，否則那是同一個洞的另一半）。

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
