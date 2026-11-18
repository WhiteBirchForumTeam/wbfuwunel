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
| 位置 `g_seq = 50`、`limit = 20` → **51…70** | **481…500**（整個房最新的 20 則） |

🚨 **而 client 兜不出來**：`g_seq` 是**全域**的號碼 —— 房間裡下一則訊息可能是 51、也可能是 5000
（中間全是別房的事件）⇒ client **算不出「位置之後第 20 則」的號碼**，沒辦法用 `before` 框出往新的窗。
📎 唯一能走到 51…70 的現有方法是從最新那端一路往回翻；那是「離線回來補進度」的正確用法，
拿來做「跳過去再往下滾」等於要把整個房翻一遍。

⇒ 缺的是**窗從哪一端開始填**。底層的正向迭代器（`timeline::pdus(user, room, from)`，
`Direction::Forward`）本來就在 —— `Recent` 用的是它的反向版 `pdus_rev`。

## 3. wire

kind `0x14 Event`，subtype **`0x08 Seek`**（`0x01`–`0x07` 已用：`Recent`、`Send`、`Batch`、
`Subscribe`、`Unsubscribe`、`Push`、`DeviceChanged`）。
回應是**一串 `0x03 Batch`**，所以 `id` 由 client 選（Batch 抄它），型別 byte 是 `0x01`
（client 的會話號，[/docs/design/wire/wire-format.md](../wire/wire-format.md) §2.2）。
**只走 WebSocket**；HTTP 回 `Error(Unsupported)`，跟 `Recent` 同理（一次只回一個 pack 的路載不動串流）。

請求 meta：

```json
{ "rooms": ["!r:localhost"], "g_seq": <g_seq>, "direction": "newer", "limit": 320, "batch": 10 }
```

| 欄位 | 必填 | 意思 |
|---|---|---|
| `rooms` | 選填 | 跟 `Recent` 同一套規則：省略（或 `null`）＝ 每個加入的房；`[]` ＝ 問了零個房，拿到空窗；🚨 **點名了你不在的房 → 整個請求 `Forbidden`**（一扇窗是一個問題的答案，缺一個房的答案是錯的而且 client 看不出來）。📎 跳訊息時就是帶**一個**房 |
| `g_seq` | 🔴 **必填** | 從這個位置開始，**不含它本身**。不是這台 server 發過的 g_seq → `InvalidRequest` |
| `direction` | 🔴 **必填** | `"newer"`（比 `g_seq` 新）或 `"older"`（比它舊）。⚠️ **故意沒有預設值**：有預設就等於「答案取決於你沒填什麼」，那正是這個 subtype 要避開的事。其他字串 → `InvalidRequest` |
| `limit` | 選填 | 這一窗最多幾則。預設 `wbf_recent_default_limit`（320），上限 `wbf_recent_max_limit`（500）。⚠️ bytes 上限 `wbf_window_max_bytes` **先於** `limit` 生效 |
| `batch` | 選填 | 每個 Batch 幾則。預設 `wbf_recent_default_batch`（10），上限 `wbf_recent_max_batch`（100） |

📎 **沒有 `cg_seq`**，也沒有第二個位置欄位 ⇒ 不存在「兩個欄位互相矛盾」這種拒絕規則。

## 4. 回應：`0x03 Batch`，順序跟著方向

- `direction: "older"` → 事件 **新→舊**（跟 `Recent` 同一個順序，client 既有的讀法不用改）。
- `direction: "newer"` → 事件 **舊→新**。
- ⭐ **兩個方向共用同一條續接規則**：窗從 `g_seq` 那一端開始填，`ls`（最後一則）永遠是**遠端**那則
  ⇒ **下一窗帶 `g_seq = ls`、`direction` 不變**。這是把 `fs`／`ls` 定義成「第一則／最後一則」
  （而不是「最新／最舊」）換來的：一條規則蓋住兩個方向。
- `tc`／`bc`／`r` 與不變量 `tc = 已送 + bc + r` 跟 `Recent` 一樣；空窗一樣是一個
  `bc = 0, r = 0, fs = ls = 0` 的 Batch。
- `more`：這一窗停在**上限**（`limit` 或 bytes）⇒ **行進方向上**可能還有；`false` ＝ 停在沒有事件了。

🚨 **`newer` 那一端是活的，`older` 那一端不是。**
往新走會**追上現在**：`more = false` 只表示「問的那一刻沒有更新的了」，而更新的事件由
`Subscribe`／`Push` 送達 ⇒ client 照既有規則按 `g_seq` 收斂。
⚠️ 往舊走碰到的是**本站副本的開頭**（靜止的邊界，而且在開了聯邦之後它也不等於「這個房沒有更早的歷史」，
同 `Recent` §2.1 的那段警告）。⇒ **同一個 `more = false` 在兩個方向的意思不一樣**，文件寫明，client 別混。

## 5. 可見性與過濾

跟 `Recent` **完全同一條路**，不是一份複製品：`ignored_filter` → `visibility_filter` →
`bundle_aggregations`（MSC4025 抹除、thread marker、edit／reference 折疊）。
🚨 這一段 `Recent` 吃過一次虧（它曾經只抄了 `/messages` 的兩個過濾、漏掉第三個）⇒ `Seek`
**呼叫同一個函式**，不另抄。

📎 只讀**加入中**的房：離開過的房的歷史是 HTTP `/messages` 的工作（它有 `history_visibility`
那一整套語意），`Recent` 從來只看 `rooms_joined`，`Seek` 一致。

## 6. 不在這一支

**`event_id` 當位置**（`{ "event_id": "$x", "direction": … }`）與**中心窗**
（一次要「往舊 N ＋ 錨點 ＋ 往新 M」）是 issue #64 的後半，另一支 —— 它們長在這一支上。

🚨 **後半要定的事先記著**，免得到時重想：

- `event_id` → `PduCount` 的翻譯失敗時**一律 `NotFound`**，🚫 不分「不存在」與「你看不到」——
  否則任何人都可以拿一個 id 去問，從回答的差別推出「有這麼一則」。
- ⚠️ **outlier**（存著但從沒進時間線）也走這條：它連 HTTP `/context` 都答不了，所以沒有
  「換條路拿」這回事 ⇒ 不需要第二個錯誤碼。
  📕 維護者 2026-11-18 否決了「另給一個碼讓 client 改走 HTTP」的提案，而那個提案建立在
  **「舊資料沒有位置」這個錯誤前提**上：位置來自 `pdu_id`（上游本來就有），不是 `unsigned` 的
  `g_seq`（PR #22 才加），而且舊資料有遷移補過（`migrations/backfill_room_seq.rs`）。

## 7. 實作落點（給下一個讀的人）

- `src/api/client/wbf/mod.rs`：`event::SEEK = 0x08`、准入表一列（`logged_in_websocket_only` ＋
  `IdType::ClientConversation`，跟 `RECENT` 同一條）、dispatch 一條。
- `src/api/client/wbf/recent.rs`：窗的收集函式多一個方向參數 —— `pdus_rev` ↔ `pdus`、heap 的
  排序反過來、「比 `g_seq` 新／舊」的判斷對偶。⭐ **同一個函式加方向，不是複製一份**
  （複製品會漂移，而漂掉的那次沒有人會喊 —— 這個 repo 2026-11-17 才在 OIDC 的 refresh 上
  吃過一次）。
- `src/core/wbf/`：`Seek` 的 meta 解析與 `direction` 的拒絕。

## 8. 驗收

- **單元**：`newer` 填的方向（位置之後的 N 則，不是最新的 N 則）、`older` 等價於 `Recent` 的
  `before`、`limit`／bytes 截斷之後 `ls` 是下一窗的起點、空窗、k 路合併在兩個方向各自成立、
  🔴 `direction` 缺少或不是那兩個字串要被拒、`g_seq` 不是這台 server 發的要被拒。
- **e2e**（`tests/e2e/e2e8.ps1`，`Recent` 的既有場景在那裡）：跳到中間那則往新翻兩窗 ——
  事件集合與順序對得上 `/messages` 的反序；追到最新時 `more = false`；`older` 與 `Recent` 同一段
  拿到一樣的東西；兩條拒絕。
- **向量**（`/docs/design/wire/wbf-vectors.json`）：`seek_newer_start`、`seek_newer_next`、
  `seek_older_start`，加上被拒的那一組；client repo 照舊複製一份逐 byte 比。
- 合併後：`CHANGELOG-fork.md` 一列、roadmap 一節、
  [/docs/design/wire/wire-format.md](../wire/wire-format.md) §3.3 的表加一列。
