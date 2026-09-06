# 媒體持有者集合：以外鍵取代引用計數（第三版，打掉重做）

> **這份文件回答：一份媒體「還有沒有人在用」怎麼記，才不會漏、不會扣兩次；誰加、誰拿掉、什麼時候刪。**
> 狀態：✅ 實作完成（`media/attachments` 分支，PR #24），2026-09-06；驗收結果在 §9。
> 取代：[media-gc.md](media-gc.md) §2（merge operator 計數器）、[media-attachments.md](media-attachments.md) §4（`eventid_mxcs` 列與 fallback）。
> 不變：宣告附件的方式（[media-attachments.md](media-attachments.md) §3、§5、§6）、墓碑與 410（media-gc.md §6）、每 mxc 一鎖、7 天保護期、bot 警告。

## 0. 一句話

媒體不記「被引用幾次」，記**「被誰持有」**：一個集合，元素是外鍵。事件持有它就是 `(room_id, g_seq)`，原文備份持有它就是
`(room_id, backup, g_seq)`，本站使用者頭像持有它就是 `(avatar, localpart)`。加入與拿掉都是集合操作，**重複做是 no-op**。
集合空了、而且媒體曾被管過或已超過保護期，就刪。

## 1. 為什麼不用計數（維護者 2026-09-06 的判斷）

計數的每個 ±1 都必須「恰好一次」。兩個 feat PR（#7、#10）加上 #24 三次修，每次都在補「誰負責這個 −1」：redact 不留備份時扣、
留備份時不扣、備份到期扣、purge 扣、purge 順手清備份又扣。第三個 PR 還是被 review 抓到雙扣（purge 先照列扣、`purge_original`
退回讀備份 content 再扣）。問題不在哪一個實作，在**數字本身不記得是誰扣的**，所以任何路徑都可能重複。

外鍵集合把「誰」記進去：`(room_b, 1234)` 拿掉兩次，第二次什麼都沒發生。這不是靠小心，是資料結構保證的。

## 2. 模型

### 2.1 持有者的三種外鍵

| kind | id | 誰加 | 誰拿掉 |
|---|---|---|---|
| `Event` | `(room_id, g_seq)` | `append_pdu`（宣告 ∪ 明文 content 讀到的） | redact（不留備份時）、purge_history、刪房 |
| `Backup` | `(room_id, g_seq)` | redact 而原文備份保留時，**用 `Backup` 換掉 `Event`**（同交易） | 備份到期（retention reap）、`purge_original`、刪房 |
| `Avatar` | 本站使用者的 **localpart**（不帶 domain：domain 可能會變，外站的頭像本來就計不到） | 設頭像 | 換頭像／清頭像（拿掉舊的、加新的，同交易）、刪使用者 |

房間頭像（`m.room.avatar` state 事件）就是一個 `Event` 持有者，不另立種類。聯邦來的使用者頭像不記（那是別站的媒體或別站的人）。
g_seq 用有號值（backfill 的歷史是負的）。

### 2.2 為什麼外鍵是 `g_seq` 而不是 `event_id`

- purge_history 的邊界就是一個位置（「這個 g_seq 之前的全部」），用 g_seq 當鍵可以直接前綴／範圍掃描反向索引，不用逐則事件解析內容。
- 刪房是「這個 room_id 的全部」，一樣是前綴。
- redact 時要換成 `Backup`，g_seq 從事件的 `unsigned`（PR #22 已存）讀得到，不用另查。
- 每則事件的 g_seq 全站唯一，跟 event_id 一樣可以當外鍵。

### 2.3 維護者舉的例子（照抄，這就是驗收）

```
timeline
!room_a g_seq=123   !room_a g_seq=456   !room_a g_seq=789   !room_b g_seq=1234   !room_b g_seq=5678
媒體 M 的持有者：{(a,123), (a,456), (a,789), (b,1234), (b,5678)}

刪掉 room_a（刪房不進備份）      → {(b,1234), (b,5678)}
redact b 的 1234，備份保留       → {(b,backup,1234), (b,5678)}
備份到期                         → {(b,5678)}
某本站使用者拿 M 當頭像          → {(b,5678), (avatar,u)}
redact 5678、備份到期、清頭像    → {}  → 刪
```

## 3. 表

| 表 | 鍵 | 值 | 用途 |
|---|---|---|---|
| `mxc_holder` | `mxc ‖ kind ‖ id` | 空 | 「M 被誰持有」。**空集合的判定 = 前綴 seek 一筆都沒有**。 |
| `holder_mxc` | `room_id ‖ kind ‖ g_seq ‖ mxc`（Avatar：`localpart ‖ mxc`） | 空 | 反向索引：「這則事件／這份備份／這個人持有哪些媒體」。purge 與刪房用前綴掃它。 |
| `mxc_managed` | `mxc` | 建立時間（ms） | **只有這個模型上線後上傳的媒體**才有列。沒列＝既存媒體＝永不自動刪。也是保護期的時鐘，不再看檔案 mtime。 |
| `room_mxc` | `room_id ‖ mxc` | 空 | **加速索引**：這個 room 持有或持有過哪些媒體（去重）。刪房時直接走它，每個媒體前綴刪 `mxc_holder(M, Event, room…)` 與 `(M, Backup, room…)`，不必掃反向索引的每一則事件。 |
| `mxc_room` | `mxc ‖ room_id` | 空 | `room_mxc` 的反向。**只在媒體被刪掉那一刻讀**：把它列出的 `room_mxc` 列一起清掉，所以 `room_mxc` 以「活著的媒體 × 房間」為界（review 抓到的無界成長，rumia）。 |

五張表都是「鍵即資料」，沒有 merge operator、沒有哨兵、沒有計數列。

**加速索引的原則（維護者 2026-09-06）**：平時它們只是多寫一筆、永遠不讀（no-op），只在清除時當指標用。要多快就多加幾張，
每張都是「從清除的入口直接指到媒體」：房間 → 媒體（`room_mxc`）、事件範圍 → 媒體（`holder_mxc`）、使用者 → 媒體（Avatar 的 `holder_mxc`）。
之後若加「刪使用者連他上傳的都清」之類的入口，就再加一張 `user_mxc`，形狀一樣。拆掉 `mxc_refcount`、`CounterOperand`、`COUNTER_SENTINEL`、
`eventid_mxcs`、`roomid_seqbounds` 以外與計數相關的東西（引擎的 merge 掛點留著不用也可以，之後再拆）。

## 3.1 一個管理器，所有插入點只叫它

維護者 2026-09-06：抽象成管理器，下次不用再找出所有插入點。`media_refs` 服務只露出這幾個入口，**其他模組不碰五張表**：

```
hold(txn, mxc, holder)              加一個外鍵（含所有索引）
release(txn, mxc, holder)           拿掉一個外鍵；commit 後交收集器
swap(txn, mxc, from, to)            換外鍵（redact → Backup）
release_room(txn, room_id)          走 room_mxc，拔掉這個 room 的全部外鍵
release_range(txn, room_id, kind, until)   走 holder_mxc，拔掉 g_seq < until 的
release_avatar(txn, localpart)
list_holders(mxc) -> Vec<Holder>    admin 印用
is_removable(mxc) -> bool           收集器與掃描共用的決策
```

`holder` 是一個 enum（`Event{room, g_seq}`、`Backup{room, g_seq}`、`Avatar{localpart}`），編碼只在這個模組裡。
呼叫點清單寫在這份文件 §4，新的路徑出現時加一列、叫一次 `hold`／`release`，不用理解表。

## 4. 每條路徑做什麼（全部是集合操作，全部冪等）

| 路徑 | 動作（同一筆交易） |
|---|---|
| `append_pdu` | 列出媒體（宣告 ∪ 明文）；每個：`put mxc_holder(M, Event, room, g_seq)`、`put holder_mxc(room, Event, g_seq, M)` |
| `backfill_pdu` | 同上（只有明文） |
| redact，備份**不**保留 | 反向索引找這則的媒體；每個：`del Event`；commit 後交收集器 |
| redact，備份保留 | 每個：`del Event`、`put Backup`（同一交易換掉）。媒體不會空 |
| 備份到期／`purge_original` | 反向索引 `(room, Backup, g_seq)` 找媒體；每個 `del Backup`；交收集器 |
| `purge_history(room, until)` | `release_range(room, Event, until)` ＋ `release_range(room, Backup, until)`：走 `holder_mxc` 範圍；每個 `del`；交收集器。**不讀事件內容** |
| 刪房 | `release_room(room)`：走 `room_mxc`，每個媒體前綴刪它在這個 room 的 Event／Backup 外鍵；交收集器。**不走事件** |
| 設頭像 | `del (Avatar, localpart)` 舊的、`put (Avatar, localpart)` 新的；舊的交收集器 |
| 刪使用者 | `del (Avatar, localpart)` |

「交收集器」= 交易 commit 後把 mxc 丟給收集器（既有的 `on_execute` 掛鉤）。任何一條路徑重跑一次，結果一樣。

## 5. 什麼時候刪

一個決策函數，收集器與掃描共用，**在每 mxc 一鎖之下**：

```
可刪 ⇔ 本地 且 mxc_managed 有列 且 mxc_holder 前綴為空 且（曾經有過持有者 或 建立時間超過保護期）
```

- 「曾經有過持有者」：收集器收到的 mxc 一定是有人剛拿掉外鍵的，所以收集器**不看年齡**，空了就刪 —— 維護者要的「訊息消失媒體立刻消失」。
- 週期掃描走 `mxc_managed`（不再掃全部媒體），只處理「從沒被指過、超過 7 天」的：沒有持有者且 `created + 7d < now`。
- 既存媒體沒有 `mxc_managed` 列，兩條路都不會碰它。不需要哨兵。
- 鎖：加持有者的人從寫入前持鎖到 commit（現有做法）；收集器與掃描持鎖下 seek 前綴再刪。同型的窗口論述沿用 media-gc.md §3.3。
- 刪掉媒體之後順手 `forget_media`：走 `mxc_room` 把 `room_mxc`／`mxc_room` 的列清掉。拿掉一個持有者時**不**清這兩張（那要掃一次才知道這房還有沒有別的持有者），
  所以它們記的是「持有或持有過」，上界是活著的媒體 × 房間。

## 6. 宣告、警告、7 天：不變

- 密文事件（E2EE）**必須**在送訊息的請求裡宣告 `attachments`（header 或 `Event/Send`），明文可宣告可不宣告，聯集去重。驗證規則照
  [media-attachments.md](media-attachments.md) §4.2 fail closed。
- 上傳後 7 天內沒有任何持有者 → 掃描刪；用舊 client 在 E2EE 房間傳檔的人收到一次英文警告。
- 保護期至少 7 天，時鐘是 `mxc_managed` 的建立時間；環境變數 `WBFUWUNEL_MEDIA_GRACE_SECONDS` 只給測試覆寫（§9）。

## 7. 這個模型不會有的問題

- **雙扣**：拿掉已經不在的外鍵是 no-op。#24 抓到的 purge＋purge_original 雙扣，在這裡是 `del Event`（已在 redact 時換成 Backup，no-op）＋ `del Backup`（真的拿掉）。
- **漏扣**：redact 換 Backup、備份到期拿 Backup，兩步都是同交易，沒有「誰負責」要記。
- **看不到密文**：宣告解決，同前。
- **既存資料**：沒列就不管，沒有遷移、沒有哨兵。
- **計數變負**：沒有計數。

## 8. 拆掉、留下

| 拆 | 留 |
|---|---|
| `mxc_refcount`、counter merge operator 的使用、`COUNTER_SENTINEL`、`refcount()`、`!admin media refcount` 的計數輸出（改成列持有者） | 每 mxc 一鎖、收集器 worker 與 `on_execute` 掛鉤、墓碑與 410、`media.collect()` |
| `eventid_mxcs` 與 content fallback、`is_original_retained` 那個補丁 | 宣告驗證、`X-Wbf-Attachments`、`Event/Send`、警告、`mxc_managed` 取代 legacy-upload 時間 |
| `media_gc_migrate_*`（已拆） | `media_unreferenced_grace_seconds`、`media_gc_sweep_interval`、`attachments_max_per_event`、`media_gc_enabled` |

`!admin media refcount <mxc>` 改成印持有者清單（「被 room_b 的 5678 與 @u 的頭像持有」），比一個數字有用。

## 9. 驗收（e2e，照 §2.3 的例子）

**2026-09-06 結果**（腳本 `tests/e2e/e2e10.ps1`）：e2e10 **13 個檢查點全綠**（下列前四項）；e2e9（宣告、拒送、`Event/Send`、警告、redact 保留備份後 purge）26 綠；e2e8（`r_seq`／`g_seq`）37 綠回歸；單元五個 crate 全綠。

- 五則事件（兩房）引用同一媒體 → 刪房 a → 仍在；redact b1（備份保留）→ 仍在；重啟讓 retention（1 秒）清備份 → 仍在（b2 持有）；設頭像後 redact b2 並清備份 → 仍在（頭像持有）；清頭像 → 410。同一 redact 與刪房再做一次：無錯。
- purge_history 到 marker 之前：範圍內的 Event（c2）與 Backup（redact 過的 c1）都拿掉 → 410；marker 之後的 c4 持有的媒體 200；同一範圍再 purge 一次無錯、狀態不變。
- 掃描：`WBFUWUNEL_MEDIA_GRACE_SECONDS=3`、`media_gc_sweep_interval=2` → 沒被指的上傳 8 秒後 410、被指的 200，啟動 log 有 override 警告；沒設變數時新上傳不被掃。
- E2EE 宣告、四種拒送、`Event/Send`、警告一次：沿用 e2e9 情境 1。
- 既存媒體（用舊 binary 建的庫）：沒有 `mxc_managed` 列，掃描與收集器都不碰。
- 反覆執行：同一則 redact 兩次、同一 purge 跑兩次、備份到期後再 purge —— 集合狀態與第一次相同，log 無錯。
- 單元：三種外鍵的編碼／前綴、`g_seq` 偏移編碼保序（`holder.rs`）。「可刪」決策（本地 ∧ 有 `mxc_managed` ∧ 無持有者）要 `Services`，
  由 e2e 涵蓋。7 天後的掃描刪除在 e2e 用環境變數觸發（下條）。
- **保護期的環境變數**（維護者 2026-09-06）：`WBFUWUNEL_MEDIA_GRACE_SECONDS`，設了就用它、**不夾底線**；沒設就 config 值且至少 7 天。
  只給測試用，啟動時若設了會 warn。e2e 用它壓到幾秒，驗「沒被指的上傳過期被掃、被持有的不動、既存（無 `mxc_managed`）不動」。

## 10. 落點

| 什麼 | 哪裡 |
|---|---|
| 五張表 | `src/database/maps.rs` |
| `Holder` enum、編碼、§3.1 的管理器 | `src/service/media_refs/`（重寫 mod.rs；`attachments.rs` 留） |
| 各路徑 | `timeline/{append,backfill,redact,purge,pdus}.rs`、`retention/mod.rs`、`profile/mod.rs`、`rooms/delete/mod.rs`、`users`（刪使用者） |
| 收集器＋掃描 | `media_refs/collect.rs`（決策函數一個） |
| `mxc_managed` 寫入 | `media/data.rs::create_file_metadata`（取代 `Init` merge） |
| admin | `admin/media/refcount.rs` → 列持有者 |
