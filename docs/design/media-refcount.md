# 媒體引用計數與真正的刪除

> **狀態：索引部分已實作，刪除部分仍是提案。**
>
> ✅ 已實作：`mxc_holder` 索引（服務 `src/service/media_refs/`），以及在事件寫入、backfill、
> redact、歷史清除、房間清除、頭像設定、帳號停用**七處**的維護。
> ⏳ 未實作：**任何會刪掉 bytes 的東西**、墓碑、重建工具。
> 👉 那一半的方案在 [media-gc.md](media-gc.md)。
> ⚠️ 2026-09-02：維護者要求**精確的數字計數**，[media-gc.md](media-gc.md) 第二版用 RocksDB merge operator
> 做到純寫入的 ±1；本文件的列式索引 `mxc_holder` 將在該階段退場。
>
> ⚠️ §3.1 的設計在實作時被修正過兩次（三個 column family 變一個；索引一般化成帶種類碼的
> holder，把頭像也納入），理由都寫在該節與 §3.7。
>
> 撰寫日期：2026-09-01。上位文件：
> [why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md) §5.4。
>
> ⚠️ **命名**：專案名為 **wbfuwunel**。程式碼裡的 crate 與路徑仍是上游的 `tuwunel-*`，
> 這份文件引用路徑時照實寫。

---

## 1. 現況：為什麼刪不掉

三件事疊起來，媒體 bytes 在這個 server 上實際上是永久的：

1. **`redact_pdu` 完全沒碰媒體。** 它只做四件事：存下原始 PDU、從搜尋索引移除、刪關聯、
   原地剝空後寫回（`src/service/rooms/timeline/redact.rs`）。
2. **沒有任何「事件 ↔ 媒體」的索引，也沒有引用計數。** 媒體的 column family 全部以 mxc 為鍵
   （`mediaid_file`、`mediaid_user`、`mediaid_lazy`、`mediaid_lazycontent`、`mediaid_pending`）。
   所以 server 問不出「這個 mxc 還有沒有人在用」。
3. **房間刪除也一樣。** `src/service/rooms/delete/mod.rs` 的 `purge_room` 沒有一個
   `media` 字。

⚠️ **而且 redaction 會毀掉找得到媒體的那個資訊。** 現有的 `!admin media delete-by-event`
（`src/admin/media/delete_by_event.rs`）靠解析事件內容裡的 `content.url`、
`info.thumbnail_url`、`file.url` 找 mxc；但它讀的是 `timeline.get_pdu_json()`，而
`redact_pdu` 最後會 `replace_pdu()` 覆蓋掉那份 JSON。**redact 之後這個指令就再也找不到媒體**
—— 偏偏那正是最想刪它的時刻。

## 2. 已定的決策

維護者 2026-09-01 決定：

| 決策 | 內容 |
|---|---|
| **轉發的語意** | 轉發 = **複製指標**，共用同一把金鑰、同一份密文。不重新加密 |
| **刪除的權限** | **引用計數歸零就刪，不看是誰刪的**，也不看誰是上傳者 |
| **墓碑** | 有意刪除的媒體要留墓碑，讓客戶端能區分「已刪除」與「載入失敗」 |

### 2.1 ⚠️ 共用金鑰的安全後果（必須明講，不能默默發生）

選了「轉發＝共用密文」就等於選了這個：**B 把訊息轉發到一個 A 不在的房間，那個房間的成員
就拿到了能解開 A 原始檔案的金鑰。**

這在自架、成員彼此認識的情境下是可接受的取捨，但它**不是免費的** —— 另一條路（轉發時重新
加密）會讓每份密文都不同，引用計數永遠是 1，整個去重的收益歸零。兩者只能選一個，這裡選了前者。

👉 客戶端在轉發時應該讓使用者看得出「這會把檔案的解密能力交給新房間」。

## 3. 設計

### 3.1 一個新的 column family

> ⚠️ **2026-09-01 修正。** 這一節原本規劃**三個** column family：`eventid_mxc`（事件→媒體）、
> `mxc_refcount`（計數）、`mxc_tombstone`（墓碑）。實作時讀了程式碼，前兩個都不需要，
> 而且 `mxc_refcount` 根本行不通。理由在下面，因為它同時是這個設計為什麼安全的理由。

| 名稱 | 鍵 → 值 | 狀態 |
|---|---|---|
| `mxc_holder` | `mxc \|\| holder 種類 \|\| holder id` → 空值 | ✅ 已實作 |
| `mxc_tombstone` | mxc → (刪除時間, 原因) | ⏳ 提案中，見 §3.6。刪除功能接上時才需要 |

**「還有沒有人在用」＝ 對 mxc 前綴 seek 一次，有沒有列。列本身就是計數。**

#### holder 種類（`src/service/media_refs/mod.rs` 的 `Holder`）

| 碼 | 種類 | 誰寫的 |
|---|---|---|
| `0x01` | 事件 | 事件寫入時，從 `content` 讀出 mxc |
| `0x02` | 個人頭像 | profile 寫入時 |

⭐ 種類碼放在 mxc 的**後面**，所以**一個前綴 seek 就答完所有種類** —— GC 只問一個問題，
不需要知道有幾種 holder。種類只在檢視與除錯時才需要區分。

🚨 **這張表就是刪除功能可以上線的閘門：每一種持有者都必須在裡面。** 沒列進來的持有者，
在索引看來就是「沒人用」。種類碼是**磁碟上的格式**，改了等於把舊列改名成沒人找得到的東西。

#### 為什麼不是 `mxc_refcount`（一個數字）

🚨 **`Txn` 是純寫入的 WriteBatch —— 它沒有讀取能力**（`src/database/txn.rs` 只有
`put` / `del` / `insert` / `execute`）。一個數字要 +1 就得先讀，所以計數器**根本塞不進
`append_pdu_json` 那個既有交易**，只能另開一次讀-改-寫 —— 那正好會失去我們要的原子性，
還引入丟失更新的競態。

而複合鍵的插入與刪除是**純寫入、且冪等**：重試同一個交易不會把計數算成兩次。
⭐ **這不只是比較好，是唯一塞得進既有接縫的作法。**

#### 為什麼不需要 `eventid_mxc`

原本以為 −1 的時候讀不到內容（因為 redaction 會剝空），所以需要一份事件→媒體的索引。
**讀了程式碼之後發現不成立**：兩個 −1 的地方**都拿得到未剝空的內容**。

- `redact_pdu` 在呼叫 `redact_in_place` **之前**就持有完整的 PDU JSON。
- `delete_pdus`（房間清除）讀的是資料庫裡存的 PDU JSON。

⭐ 而「redaction 毀掉指標」這件事對**重建**也不成問題：一個已經 redact 的事件**本來就不該
有引用**（它的引用在 redact 當下就移除了），所以「掃描所有事件、從內容重算」得到的正是正確的
集合。少一個索引就少一份會漂移的資料。

### 3.2 加減的位置：接縫已經存在

⭐ `append_pdu_json`（`src/service/rooms/timeline/append.rs`）**已經在用一個交易**寫入事件：

```rust
let mut txn = self.db.db.txn();
txn.raw_put(&self.db.pduid_pdu, pdu_id, Json(json));
txn.insert_raw(&self.db.eventid_pduid, ...);
txn.put_raw(&self.db.roomid_tscount_pducount, ...);
txn.execute();
```

`mxc_holder` 的**寫入加進這個交易**，就天然原子 —— 不會出現「事件寫進去了但引用沒記」。
而且本地與遠端事件都經過 `append_pdu`（`append_incoming_pdu` 也呼叫它），是**單一咽喉點**。

| 動作 | 位置 | 狀態 |
|---|---|---|
| **記錄引用** | `timeline/append.rs` 的 `append_pdu_json`，在它既有的交易裡 | ✅ |
| **記錄引用（backfill）** | `timeline/backfill.rs` 的 `prepend_backfill_pdu`，同位交易 | ✅ |
| **移除引用（redact）** | `timeline/redact.rs` 的 `redact_pdu` | ✅ |
| **移除引用（歷史清除）** | `timeline/purge.rs` 的 `purge_history`，跟著它每筆 PDU 的既有交易 | ✅ |
| **移除引用（房間清除）** | `timeline/pdus.rs` 的 `delete_pdus`，跟著它每筆 PDU 的既有交易 | ✅ |
| **記錄頭像引用** | `profile/mod.rs` 的 `set_profile_keys`，與 profile 寫入同一交易 | ✅ |
| **移除頭像引用（停用）** | `profile/mod.rs` 的 `clear_profile_keys` | ✅ |
| **實際刪 bytes** | 新的 worker，抄 `src/service/rooms/retention/mod.rs` 的形狀 | ⏳ 尚未實作 |

⚠️ **頭像必須跟 profile 寫入同一個交易**，因為拆成兩步**沒有安全的順序**：先寫引用再寫
profile，中斷會留下 profile 還指著舊頭像、舊引用已釋放；反過來則是新頭像沒人持有。
這也是 `set_profile_keys` 順帶改成單一交易的原因（以前是一個欄位一次寫入）。

📌 **新增一個寫 `pduid_pdu` 的地方，就欠這個索引一筆。** 目前共三處：`append_pdu_json`、
`prepend_backfill_pdu`（兩者都記錄），以及 `replace_pdu`（不記錄 —— 它只取代已存在的事件，
redact 自己處理引用，而另一個呼叫者 `threads` 只改 `unsigned`、不動 `content`）。
⚠️ **backfill 那筆是審查時才被拓出來的** —— 漏掉的原因是當初只掃了「誰刪 `pduid_pdu`」，
沒掃「誰寫」。寫入端漏一個，那些事件的媒體就會讀成「無人引用」—— 而那是不可逆的方向。

⚠️ **`redact_pdu` 的移除不在交易裡**，因為它收尾用的 `replace_pdu` 本身不是交易。所以順序是：
**先把剝空的事件存好，成功之後才移除引用列**。中間 crash 的話會留下「引用列還在、事件已剝空」
—— 媒體被多扣住一份，這是安全的方向。反過來就會變成事件還指著一份已經沒人保護的媒體。

### 3.3 ⚠️ 順序：先減計數並 redact，bytes 的刪除交給非同步 worker

**不要**「先確認是最後一個 → 清媒體 → redact」。這兩步不是原子的，中間 crash 的話：

| 順序 | crash 之後 | 壞掉的方向 |
|---|---|---|
| 先刪 bytes 再 redact | bytes 沒了、事件還指著它，而且**永遠不會被修正**（計數已歸零，沒人會回來 redact） | ❌ **內容不見** |
| 先減計數並 redact，bytes 非同步刪 | 事件已剝空、bytes 變孤兒 | ✅ **垃圾留著**，下一輪 GC 再回收 |

⭐ **讓失敗落在「垃圾留著」，不要落在「內容不見」。空間可以再回收，bytes 刪了就沒了。**

還有一個獨立理由：**redact 是協定語意**（別的客戶端看得到），**bytes 刪除是本地儲存管理**。
把磁碟 IO 的成敗擋在協定動作前面，等於讓儲存層決定一則訊息能不能被撤回。

### 3.4 計數一定會漂移，所以「能重算」比「算得準」重要

crash、bug、migration、有人手動改 DB —— 任何索引都會漂移。所以真正的要求是**能離線重掃重建**。

重建的作法是掃描所有存下來的 PDU、從內容重算引用集合。⭐ 這是**正確的**，即使 redaction 已經
剝空了那些事件的內容 —— 因為一個被 redact 的事件本來就不該有引用，它的引用在 redact 當下
就移除了。所以「內容裡沒有 mxc」與「不該有引用列」是同一件事。

⚠️ **重建工具尚未實作。** 它要跟刪除功能一起來 —— 在沒有東西會被刪之前，漂移只是佔一點空間。

**漂移的方向要選**：

| 錯的方向 | 後果 | 可逆嗎 |
|---|---|---|
| 漏加（少算） | 提早刪掉還有人用的 bytes | ❌ 不可逆 |
| 漏減（多算） | 該刪的沒刪，佔空間 | ✅ 重算或 TTL 兜底可回收 |

👉 **fail closed：寧可多算。** 算不出來就當它還有人用。

### 3.5 ⚠️ 上傳與引用之間的空窗

正常流程是 `POST /upload` 拿到 mxc → 才送出引用它的事件。**這中間計數是 0。** 如果 GC 這時候
跑，會刪掉剛上傳、還沒送出的檔案。

👉 **GC 只回收「曾經 ≥ 1、現在歸零」的媒體。從未被引用過的完全不碰**，交給既有的
`delete-range` / `delete-by-date-size` 這些 operator 工具按日期處理。

這樣空窗期天然安全，而且不需要額外的寬限期計時器。

### 3.6 墓碑：讓 404 可區分

現在媒體找不到一律是 `Err!(Request(NotFound("Media not found.")))` → HTTP 404 /
`M_NOT_FOUND`。這對使用者是最糟的狀態：**訊息不會消失**（事件內容還在，仍顯示檔名、大小、
mimetype），只是縮圖破圖、下載失敗 —— 看起來像「壞掉」，不像「被刪除」。

有了 `mxc_tombstone`，被**有意**刪除的媒體可以回一個可區分的錯誤，客戶端才說得出
「這個檔案已被刪除」而不是「載入失敗，請重試」。

⭐ 墓碑同時是引用計數的安全網：日後若發現還有人引用它，至少知道「它是被刪的」，不是
「從來沒存在過」，可以往下查。

### 3.7 索引的涵蓋範圍

**事件**的部分只認 `content` 裡四個位置，定義在 `src/core/matrix/media_ref.rs` 的
`MXC_CONTENT_PATHS`：

| 位置 | 涵蓋 |
|---|---|
| `url` | 未加密的 `m.image` / `m.file` / `m.video` / `m.audio`，以及 `m.room.avatar` |
| `info.thumbnail_url` | 未加密的縮圖 |
| `file.url` | 加密內容（`EncryptedFile`） |
| `info.thumbnail_file.url` | 加密縮圖 |

**個人頭像**不走這條，它有自己的 holder 種類（`0x02`），寫在 profile 的寫入點上。

📎 **房間頭像不需要特別處理** —— `m.room.avatar` 是 state 事件，它的 `content.url` 已經在上表裡。
它的引用會一直留著，因為那個 state 事件一直存在，而**那是正確的**：時間軸上還看得到那則事件，
媒體當然還被引用著。

#### ⭐ 為什麼頭像用索引，不用 `avatar_mxcs()` 全掃

媒體服務有一個 `avatar_mxcs()`，會枚舉每個本地使用者的頭像加上每個房間的頭像；
`delete_by_date_size` 就用它豁免頭像。**但那不該拿來當 GC 的判斷依據**：

- 它是 O(使用者 + 房間) 的**全掃**，而 GC 問的是**單一 mxc 的成員關係** —— 那是索引的工作。
  （`delete_by_date_size` 用得合理，是因為它本來就是要掃過全部 mxc 的批次指令。）
- 🚫 **而「把 `avatar_url` 加進 `MXC_CONTENT_PATHS`」是錯的做法**，兩個理由：
  1. **它關不上真正的缺口。** 使用者的頭像存在 profile 裡，`m.room.member` 事件只是副本。
     不在任何房間的使用者，索引裡一筆都沒有。
  2. **舊頭像永遠回收不了。** member 是 state 事件，而 `purge_history` 明確跳過 state 事件，
     所以每個歷史頭像的 member 事件都永遠留著，索引會永遠說「還被引用」。
     索引變成只會加不會減 —— 那等於沒做。

👉 所以頭像的引用**寫在 profile 的寫入點**，跟事件一樣是「誰持有、誰負責記」。

📎 **改動 `MXC_CONTENT_PATHS` 或新增 holder 種類，都需要重建索引**，因為已經存下來的資料是用
舊規則掃過的。

## 4. 待決

1. **寬限期。** 歸零之後多久才真的刪？要不要跟現有的 `redaction_retention_seconds`
   （預設 60 天）對齊？兩個保留期不一致會讓「原文還在但檔案沒了」變成可能。
2. **墓碑保留多久。** 永久？還是也有 TTL？永久的話它會單調成長（但每筆很小）。
3. **可區分的錯誤怎麼表達。** 自訂 error code，還是沿用 `M_NOT_FOUND` 但加欄位？非聯邦的
   自架環境可以自由一點，但客戶端要跟著改。
4. **`delete_by_event` 的修法。** 它應該讀 `retention.get_original_pdu()` 而不是被剝空的
   timeline PDU。這是獨立的小修，也可能有我沒看到的理由（例如刻意不讓管理員繞過 redaction
   的語意）——**要先確認是不是 bug。**
5. **遠端媒體。** 非聯邦是既定的非目標，但 `src/service/media/remote.rs` 仍會抓遠端媒體。
   快取來的東西要不要納入引用計數，還是照舊走 TTL？

## 5. 不做什麼

- 🚫 **不做跨使用者去重。** E2EE 下每次上傳用不同金鑰，同一份明文產生完全不同的密文，內容
  位址不會撞。去重只在「同一份密文被多處引用」（＝轉發）時發生，而那正是引用計數在算的東西。
- 🚫 **不動事件 DAG。** 這份提案只碰媒體 bytes 的生命週期。事件層要不要保留 DAG 是
  [why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md) 的待驗 1，另案。
- 🚫 **不做分塊與 Merkle。** 那是同一份設計文件的 §5.2，跟這份互相獨立：引用計數算的是
  「一份媒體」，不管它內部怎麼切。兩者可以分開做，先做哪個都行。
