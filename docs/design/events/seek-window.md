# `Event/Seek`（`0x14 0x08`）：從一個位置往一個方向讀一扇窗

issue #64 的前半。形狀由維護者 2026-11-18 定：**一個新的 subtype，而不是在 `Recent` 上多一個欄位。**

## 0. 一句話

`Seek` 回答「**從 `g_seq` 這個位置，往 `newer` 或 `older`，給我一扇窗**」。
`Recent`（[/docs/design/events/room-seq-and-recent.md](room-seq-and-recent.md) §2）回答的是另一個問題：
「**追到現在** —— 比我的水位新的都給我」。兩者共用回應的 `0x03 Batch`。

## 1. 為什麼不是在 `Recent` 加一個欄位

第一版的提案是給 `Recent` 加一個 `after`。🔴 **維護者否決，而理由是對的**：

- `Recent` 的回應順序會變成**取決於 client 填了哪個欄位**（填 `before` 是新→舊、填 `after` 是舊→新）
  ⇒ client 讀答案之前得先回頭看自己剛剛問了什麼。
- `after` 與 `cg_seq` 都是「比它新」，但一個是**停下來**的界、一個是**開始填**的點
  ⇒ 同時出現沒有唯一答案，於是得多一條拒絕規則，而那條規則只是在補償欄位放錯地方。

⭐ **一個 subtype 一種語意**之後這兩件都不存在：順序由「**你問的是哪個 subtype ＋ `direction`**」
決定，而 `Seek` 根本沒有 `cg_seq` 可填。

## 2. 缺的能力是什麼：過濾有，**填的方向**沒有

`Recent` 的 `cg_seq` 本來就是一個「只要比它新」的**過濾器**。但窗一律從**最新那端**往回填
（`pdus_rev` ＋ 碰到 `cg_seq` 就停）：

| 想要的 | `Recent` 給的 |
|---|---|
| 位置 `g_seq = 50`、`limit = 20` → **50…69**（含錨點那則）| **481…500**（整個房最新的 20 則） |

🚨 **而 client 兜不出來**：`g_seq` 是**全域**的號碼 —— 房間裡下一則訊息可能是 51、也可能是 5000
（中間全是別房的事件）⇒ client **算不出「位置之後第 20 則」的號碼**，沒辦法用 `before` 框出往新的窗。
📎 唯一能走到 50…69 的現有方法是從最新那端一路往回翻；那是「離線回來補進度」的正確用法，
拿來做「跳過去再往下滾」等於要把整個房翻一遍。

⇒ 缺的是**窗從哪一端開始填**。底層的正向迭代器（`timeline::pdus(user, room, from)`，
`Direction::Forward`）本來就在 —— `Recent` 用的是它的反向版 `pdus_rev`。

### 2.1 🚨 「跳到訊息」的前半**已經有了** —— 橋上的 `0x25 Context`

⚠️ 動手之前要先知道這件事，否則整支的理由是錯的：**`/context/{event_id}` 已經在橋上**
（`0x14 0x25 Context`，見 [/docs/bridge-specs/0x14-event.md](../../bridge-specs/0x14-event.md)）。
它一個來回就給錨點那則 ＋ 前後各幾則 ＋ `start`／`end`，而回來的事件 `unsigned` 帶 `g_seq`。

⇒ **client 今天就跳得過去**。缺的是**跳過去之後往下滾**：

| | |
|---|---|
| `/messages?dir=f`（往新翻頁） | 🔴 **沒上橋**（`0x14` 的橋號 `0x20`–`0x2A` 裡沒有它） |
| `Recent` | 填不了錨點那一端（§2 那張表） |

📎 **考慮過、沒選的替代方案：把 `/messages` 也搬上橋**（橋表一列，而且它的 `start`／`end`
token 其實就是 `PduCount` 的十進位字串，`PduCount::from_str` 直接 `parse`，不是不透明 token）。
維護者 2026-11-18 選了新 subtype，理由是**它要比內建的那條豐富一點**，而且跟 client 在這條線上
已經在用的東西同形：一個窗、`Batch` 串流、`g_seq = ls` 續接、可見性走 `Recent` 的同一個函式。
🚫 橋上那條是 Matrix 的 JSON（`chunk` ＋ `start`／`end`），client 要另一套讀法。

## 3. wire

kind `0x14 Event`，subtype **`0x08 Seek`**（`0x01`–`0x07` 已用：`Recent`、`Send`、`Batch`、
`Subscribe`、`Unsubscribe`、`Push`、`DeviceChanged`）。
回應是**一串 `0x03 Batch`**，所以 `id` 由 client 選（Batch 抄它），型別 byte 是 `0x01`
（client 的會話號，[/docs/design/wire/wire-format.md](../wire/wire-format.md) §2.2）。
**只走 WebSocket**；HTTP 回 `Error(Unsupported)`，跟 `Recent` 同理（一次只回一個 pack 的路載不動串流）。

請求 meta（維護者 2026-11-18 定的形狀）：

```json
{ "room_id": "!r:localhost", "event_id": "$x", "direction": "newer", "limit": 20, "batch": 10 }
```

| 欄位 | 必填 | 意思 |
|---|---|---|
| `room_id` | 🔴 **必填，剛好一個房** | 🚫 不是 `Recent` 的 `rooms` 陣列：Seek 回答的是**房內的位置**問題（跳訊息、往上下滾），跨房的窗是 `Recent` 的工作。⭐ 連帶好處：不必 k 路合併，順序就是那個房自己的順序。**不在那個房 → `Forbidden`**（跟 `Recent` 點名不在的房一致）|
| **錨點** | 🔴 **`event_id` 與 `g_seq` 剛好給一個** | 兩個都給、或都不給 → `InvalidRequest`。⚠️ 都給的時候**不要挑一個優先**：那會讓「答案取決於 server 先看哪個欄位」 |
| `direction` | 🔴 **必填** | `"newer"`（錨點＋比它新的）或 `"older"`（錨點＋比它舊的），🔴 **兩個方向都含錨點那則**（§4.1）。⚠️ **故意沒有預設值**：有預設就等於「答案取決於你沒填什麼」，那正是這個 subtype 要避開的事。其他字串 → `InvalidRequest` |
| `limit` | 選填 | 這一窗最多幾則。**預設 20**（`wbf_seek_default_limit`，維護者 2026-11-18：320 是「批量抓全域聊天室」的數字，那是 `Recent` 的事），上限沿用 `wbf_recent_max_limit`（500）。⚠️ bytes 上限 `wbf_window_max_bytes` **先於** `limit` 生效 |
| `batch` | 選填 | 每個 Batch 幾則。預設 `wbf_recent_default_batch`（10），上限 `wbf_recent_max_batch`（100） |

📎 **沒有 `cg_seq`** ⇒ 不存在「兩個欄位都說『比它新』」那種拒絕規則（§1）。

### 3.1 兩種錨點，內部只有一種

⭐ **`event_id` 進來就翻成 `PduCount`（＝ `g_seq`），之後兩種錨點走完全同一條路。**
方向是這一邊而不是反過來（`g_seq` → `event_id`），理由很實際：窗的掃描本來就是**按 count 走**的
（`pdus` ／ `pdus_rev` 都收 `PduCount`），而 `event_id → count` 是一個現成的單次查詢
（`timeline::get_pdu_count`，六個地方在用）；反過來還得每個請求多查一次，而且換不到任何東西。

📌 **為什麼兩種都收**：維護者 2026-11-18 —— client 的 UI 與 DB 都以 `event_id` 為主
（它那邊會自動把 `g_seq` 換成 `event_id`），所以 **`event_id` 是實務上會用的那個**；
`g_seq` 留著是為了彈性（server 端已經有那個號碼時不必繞一圈），實務上可能用不到。

🚨 **翻不到的時候一律 `NotFound`**，三種情形同一個答案：那則不存在、是 outlier（存著但從沒進
時間線，`eventid_outlierpdu` 而不是 `eventid_pduid`）、或**這個使用者看不到它**。
⇒ 📎 **故意不分**：如果「不存在」與「存在但你看不到」回不同的碼，任何人都可以拿一個 `event_id`
去問，從回答的差別推出「有這麼一則」。`0x25 Context` 與 `GetEvent` 今天就是這樣做的，照它一致。
⚠️ 而「換條路拿」不存在：outlier 連 HTTP `/context` 都答不了（它一樣要時間線位置）。

⚠️ **檢查的順序是先房、後錨點**：不在那個房 → `Forbidden`（不去碰那則事件存不存在）；
在房裡但錨點翻不到 → `NotFound`。反過來會把「那個房裡有這則」洩漏給不在房裡的人。

📎 錨點**不在 `room_id` 那個房**時也是 `NotFound` —— 位置雖然是全域的，但這支的問題是
「這個房裡、這個位置的前後」，而一個不在這房的錨點對它沒有意義。


## 4. 回應：`0x03 Batch`，順序跟著方向

- `direction: "older"` → 事件 **新→舊**（跟 `Recent` 同一個順序，client 既有的讀法不用改）。
- `direction: "newer"` → 事件 **舊→新**。
- ⭐ **兩個方向共用同一條續接規則**：窗從 `g_seq` 那一端開始填，`ls`（最後一則）永遠是**遠端**那則
  ⇒ **下一窗帶 `g_seq = ls`、`direction` 不變**（⚠️ 錨點含在窗裡 ⇒ `ls` 那則會重送一次，§4.1）。這是把 `fs`／`ls` 定義成「第一則／最後一則」
  （而不是「最新／最舊」）換來的：一條規則蓋住兩個方向。
- `tc`／`bc`／`r` 與不變量 `tc = 已送 + bc + r` 跟 `Recent` 一樣；空窗一樣是一個
  `bc = 0, r = 0, fs = ls = 0` 的 Batch。
- `more`：這一窗停在**上限**（`limit` 或 bytes）⇒ **行進方向上**可能還有；`false` ＝ 停在沒有事件了。

🚨 **`newer` 那一端是活的，`older` 那一端不是。**
往新走會**追上現在**：`more = false` 只表示「問的那一刻沒有更新的了」，而更新的事件由
`Subscribe`／`Push` 送達 ⇒ client 照既有規則按 `g_seq` 收斂。
⚠️ 往舊走碰到的是**本站副本的開頭**（靜止的邊界，而且在開了聯邦之後它也不等於「這個房沒有更早的歷史」，
同 `Recent` §2.1 的那段警告）。⇒ **同一個 `more = false` 在兩個方向的意思不一樣**，文件寫明，client 別混。


### 4.1 🔴 窗含錨點那則 —— 兩個方向都含

維護者 2026-11-18 指定：**給 `event_id` 就要連它自己那則一起回來。**

📌 **理由在呼叫端**：這支的用途是「跳到某一則」—— 跳過去而**畫面上沒有那一則**，client 還得
另外發一次（`0x25 Context` 或 `GetEvent`）把它補回來，而那正是這支要省掉的那個來回。
⭐ 而且兩個方向同一條規則：`older` 的第一則與 `newer` 的第一則**都是錨點**，🚫 不是「往新含、
往舊不含」那種要查表才記得的不對稱。

⚠️ **代價寫明：續接時有一則重疊。** 下一窗帶 `g_seq = ls`，而窗含錨點 ⇒ 上一窗的最後一則
會是下一窗的第一則。這是**刻意**的選擇，兩個理由：

- client 本來就要按 `g_seq` 收斂（`Subscribe`／`Push` 與 `Recent` 都可能送到同一則），
  所以去重這件事不是為這支新加的；
- ⭐ 重疊那則同時是一個**續接檢查**：它回來的 `event_id` 跟 client 手上的 `ls` 對不上，
  就表示中間斷了。

🚫 **不另加一個「要不要含錨點」的開關** —— 多一個欄位就多一組組合要測，而「含」這一種
在兩個用途（跳過去、再往下滾）上都成立。

📎 **所以 `bc = 0` 的空窗在這支幾乎不會出現**，但不是不可能：錨點本身可以被過濾掉
（例如它來自被 ignore 的使用者 —— 可見性不通的錨點在 §3.1 就是 `NotFound` 了，走不到這裡）。
⇒ 實作**照樣要處理空窗**，🚫 不要靠「至少有錨點那則」這個巧合。

## 5. 可見性與過濾

跟 `Recent` **完全同一條路**，不是一份複製品：`ignored_filter` → `visibility_filter` →
`bundle_aggregations`（MSC4025 抹除、thread marker、edit／reference 折疊）。
🚨 這一段 `Recent` 吃過一次虧（它曾經只抄了 `/messages` 的兩個過濾、漏掉第三個）⇒ `Seek`
**呼叫同一個函式**，不另抄。

📎 只讀**加入中**的房：離開過的房的歷史是 HTTP `/messages` 的工作（它有 `history_visibility`
那一整套語意），`Recent` 從來只看 `rooms_joined`，`Seek` 一致。

## 6. 不在這一支

**中心窗**：一次請求就回「往舊 N ＋ 錨點 ＋ 往新 M」。這一支給的是**單一方向**的窗 ——
UI 真的要兩邊，就發兩次（`older` 一次、`newer` 一次），而錨點那則兩邊都會給（§4.1）⇒
拼起來不缺中間那則。

📎 **為什麼先不做**：兩邊的 `limit`、`more`、續接游標都要各自一份，而 `0x03 Batch` 只有一組
`fs`／`ls`／`more` ⇒ 要嘛改 `Batch`（它被三個 subtype 共用），要嘛在 meta 裡塞第二組游標。
⭐ 兩次請求的代價是一個來回，改線上格式的代價是所有既有的讀法。**先看 client 實際上痛不痛**，
痛了再回來。

📕 **已經有的那一半**：「跳到某一則、拿它前後」在橋上就是 `0x25 Context`（§2.1）——
`Seek` 補的是「拿到之後往哪一邊**滾下去**」。

## 7. 實作落點（給下一個讀的人）

- `src/core/wbf/`：`Seek` 的 meta 解析 —— `room_id` 必填、錨點**剛好一個**、`direction`
  必填且只收兩個字串（§3 的三條拒絕都在這裡，🚫 不要散到 handler 裡）。
- `src/api/client/wbf/mod.rs`：`event::SEEK = 0x08`、准入表一列（`logged_in_websocket_only`
  ＋ `IdType::ClientConversation`，跟 `RECENT` 同一條）、dispatch 一條。
- `src/api/client/wbf/recent.rs`：
  - 錨點翻譯 —— `event_id` → `PduCount`（`timeline::get_pdu_count`），失敗一律 `NotFound`；
    先驗房（`rooms_joined`）再驗錨點（§3.1 的順序）。
  - 窗的收集函式多一個方向參數 —— `pdus_rev` ↔ `pdus`、「比錨點新／舊」的判斷對偶、
    **起點含錨點**（§4.1：`pdus`／`pdus_rev` 的 `from` 本來就是含的，所以這一條不必特別做，
    但要有測試把它釘住 —— 哪天為了別的理由改成 `from + 1` 就壞了）。
  - ⭐ **同一個函式加方向，不是複製一份**（複製品會漂移，而漂掉的那次沒有人會喊 ——
    這個 repo 2026-11-17 才在 OIDC 的 refresh 上吃過一次）。
  - 📎 單一房 ⇒ **不需要 k 路合併**（`Recent` 的那段 heap 是為了跨房）。
- 設定：`wbf_seek_default_limit = 20`（新的一條；上限沿用 `wbf_recent_max_limit`、
  bytes 沿用 `wbf_window_max_bytes`、batch 沿用 `wbf_recent_*_batch`）。

## 8. 驗收

- **單元**
  - 🔴 **窗含錨點**：`older` 與 `newer` 的第一則都是錨點那則（§4.1）。
  - 🔴 **續接重疊**：第二窗的第一則＝第一窗的 `ls`，而兩窗合起來沒有缺號。
  - `newer` 填的方向（錨點之後的 N 則，不是最新的 N 則）、`older` 與 `Recent` 同一段拿到
    一樣的東西、`limit`／bytes 截斷之後 `ls` 是下一窗的起點、空窗。
  - 🔴 **錨點翻譯**：`event_id` 與 `g_seq` 指同一則時，兩種問法回**一模一樣**的窗。
  - 🔴 **三條拒絕**：錨點兩個都給／都不給 → `InvalidRequest`；`direction` 缺少或不是那兩個
    字串 → `InvalidRequest`；不在那個房 → `Forbidden`；錨點不存在／outlier／看不到／
    不在那個房 → **都是 `NotFound`**（四種情形各一條，釘住「不分」這件事）。
- **e2e**（`tests/e2e/e2e8.ps1`，`Recent` 的既有場景在那裡）：跳到中間那則往新翻兩窗 ——
  事件集合與順序對得上 `/messages` 的反序、第一則是錨點；追到最新時 `more = false`；
  `older` 同一段與 `Recent` 一致；拒絕那幾條。
- **向量**（`/docs/design/wire/wbf-vectors.json`）：`seek_newer_start`、`seek_newer_next`、
  `seek_older_start`、`seek_by_event_id`，加上被拒的那一組；client repo 照舊複製一份逐 byte 比。
- 合併後：`CHANGELOG-fork.md` 一列、roadmap 一節、
  [/docs/design/wire/wire-format.md](../wire/wire-format.md) §3.3 的表加一列。
