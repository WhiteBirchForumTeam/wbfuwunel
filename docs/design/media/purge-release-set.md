# 清歷史只釋放它真的刪掉的，而每房頭像有自己的持有者

✅ **已實作：PR #106（說明）＋ PR #107（實作）**，外部審查 2026-09-29 #3。
現在的路徑表在 [/docs/design/media/media-holders.md](media-holders.md) §4，刪除的判斷在它的 §5。

⭐ **這一份只留「為什麼」** —— 過程（原本的程式長怎樣、測試計畫、逐節的決定紀錄）已經刪掉。

## 1. 原本錯在哪，以及為什麼不是「少一個持有者」而已

`purge_history` **先無條件釋放一整段的媒體持有者，然後才逐一刪事件 —— 而那個迴圈會刻意保留一部分事件**
（state 事件；`delete_local_events = false` 時本地寄件者的事件）。
⇒ 🚨 **一個還在房間裡、client 還看得到的事件，它的媒體被刪掉**，而 GET 之後回 **410 墓碑**（終局，不是可重試）。
📌 最具體的例子：`m.room.avatar` 是 state 事件、永遠保留，所以**清一次歷史就把房間還指著的那張圖刪掉**。

⇒ 釋放的集合是刪除的集合的**超集**，而那個方向是不可逆的那一邊。

## 2. 修法：A 語意 —— 兩個集合由構造相等

在**刪除那個事件的同一個 txn 裡**釋放它的持有者，而不是事前整段釋放：

```rust
txn.del_raw(&self.db.pduid_pdu, key);
…
media_refs.release_all_of(&mut txn, &Holder::event(room_id, count.into_signed())).await;
txn.execute();
```

⭐ **兩個集合由構造相等**，不是靠兩處各算一次然後希望一致（A4）；而且沒有「放掉了但還沒刪」的中斷窗口。
🚫 整段批次的 `release_range` 整個刪掉了 —— 它「不讀事件內容」正是它不可能做對的原因。

📎 **順帶修掉一條沒人注意到的**：backfilled 持有者的 `g_seq` 是**負數**（`PduCount::Backfilled` 建構時保證 ≤ 0），
低於**任何**正的 `until` ⇒ 原本**每一次**清歷史都把整個 backfill 回來的歷史的持有者全部釋放，
**跟邊界設在哪裡無關**。A 語意下「保留的就不放」自動涵蓋它，🚫 不需要 backfill 特例。

## 3. 每房成員頭像：為什麼鍵在 `(room, user)` 而不是事件

`m.room.member` 的 `avatar_url` 從來不是持有者（`MXC_CONTENT_PATHS` 只讀 `url`／`info.thumbnail_url`／
`file.url`／`info.thumbnail_file.url`）⇒ 🚨 **設完每房頭像，上傳滿保護期的下一次掃描就把它刪掉 ——
沒有人做過任何事、沒有錯誤、沒有 log 指向原因。** 📌 跟 §1 的差別是「誰把它弄壞的」：
§1 要有人下 purge，**這條自己會發生**。

⭐ **鍵是 `(room, user)` 而不是事件，因為它保護的是一個「狀態」**：載著 `avatar_url` 的 member 事件每次
成員／profile 變動就被取代，而只有當前那個會被顯示。鍵在事件上就得在「被取代」時釋放 —— 那是跟 backfill、
跟狀態解析翻回舊事件的**競賽**；鍵在這一對上就是**一次 swap，沒有順序可以搞錯**。
📎 它跟 `Holder::Avatar{localpart}` 完全同形（profile 也只有「現在」）。
⚠️ **但鍵用完整的 user id 不是 localpart**：房間有遠端成員，而 localpart 當鍵正是外部審查 **#2** 那個洞。

兩個實作上必須守的（路徑寫在 `media-holders.md` §4）：

| | |
|---|---|
| ⚠️ `release_room` 要跟著清它 | 否則刪了房間會留下一個**誰都放不掉**的持有者：唯一會釋放它的是「那個房間裡的下一個 member 事件」，而房間已經不在了 |
| 🚨 只有「成為當前狀態」能移動它 | 守衛 `count.into_signed() > 0`。📎 **不是現在的依賴**（`backfill_pdu` 之後的 `match` 只處理 `RoomMessage`），那道閘是給以後加 member 分支的人。📌 維護者的規則：「**backfilled 你就不應該去做任何 release media 的事**」—— ⭐ 單向（只加、永不放）是可逆的那一邊 |

📎 離開房間：`leave` 通常沒有 `avatar_url` ⇒ swap 成「沒有」⇒ 那張圖被放掉。那是刻意的語意。
🚫 **不加禁用旋鈕**（維護者 2026-10-04）：既然有 swap 機制，功能就讓用。

## 4. ✅ 不需要遷移，而理由不是「從沒上線」—— 是 `mxc_managed`

`RoomAvatar` 只在那個 `(room, user)` 的**下一則** member 事件被 append 時才出現，而 `src/service/migrations/`
裡沒有任何 walk 會替既有的當前成員狀態補上它。那麼**部署前就設好的每房頭像呢？**

⭐ **決定性的事實在刪除那一端，不是持有者那一端**（`media_refs/collect.rs`）：

```rust
/// The one decision: local, managed by this model, and held by nothing.
/// Unknown (not managed) is "no", never "yes".
if self.managed_since(mxc).await.is_none() { return false; }
```

而掃描那半**只走 `mxc_managed`**，而 `mxc_managed` 只由 `create_file_metadata(…, FileOrigin::NewMedia)` 寫
⇒ ⭐ **只有「這個持有者模型上線之後、在這個 fork 上傳的」媒體有那一列。**

| 既有資料庫 | 有 `mxc_managed` 嗎 | 每房頭像有危險嗎 |
|---|---|---|
| 從 Conduit／conduwuit／上游 tuwunel **匯入** | 🚫 **一列都沒有** | ✅ **沒有** —— 它的媒體**永遠不會被自動刪** |
| 本 fork 自己寫的、**有** `mxc_managed` 但**還沒有** `RoomAvatar` 的版本區間 | ✅ 有 | 🚨 有 —— ⭐ **而這一類不存在：這個 fork 從來沒有上線過** |

⭐ **而 `migrations/` 這個資料夾主要服務的正是第一列**（`import_conduit_knocks`、
`split_conduit_highlight_counts`…）—— 維護者 2026-10-05 指出：那些遷移是**原生 Matrix 實作 → 這個 fork**，
不是**這個 fork 的舊版 → 新版**。

⚠️ **我先寫了那支 walk，再拿掉**（`3bf066c38` 加、`683c2279e` 拿掉）。當時的理由是「遲早會有資料比程式老」——
⭐ **那句話沒錯，錯在我沒去看刪除那一端**：到的時候那些媒體**沒有 `mxc_managed`**，所以不在刪除的範圍裡。
📎 真的有一天需要它，做法是走每個房間的**當前成員狀態**（`room_state_keys(room, m.room.member)` →
`get_member` 的 `avatar_url`），🚫 不走歷史事件，鎖與 append 路徑同形。

## 5. ⏳ 還沒決定的一件

已經被清壞的房間**修不回來**（媒體檔案已經不在）。要不要一條 admin 指令**列出**「指著不存在媒體的活事件」
（診斷，不修復）仍未決定。
