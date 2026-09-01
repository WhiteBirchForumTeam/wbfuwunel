# 媒體引用計數與真正的刪除

> **狀態：提案，尚未實作。** 這份文件要先經維護者同意，才動 `src/`。
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

### 3.1 三個新的 column family

| 名稱 | 鍵 → 值 | 為什麼要它 |
|---|---|---|
| `eventid_mxc` | event id → 該事件引用的 mxc 清單 | **讓 GC 能離線重建計數。** 見 §3.4 |
| `mxc_refcount` | mxc → 計數 | 「還有沒有人在用」 |
| `mxc_tombstone` | mxc → (刪除時間, 原因) | 讓 404 可區分。見 §3.6 |

### 3.2 加減的位置：接縫已經存在

⭐ `append_pdu_json`（`src/service/rooms/timeline/append.rs`）**已經在用一個交易**寫入事件：

```rust
let mut txn = self.db.db.txn();
txn.raw_put(&self.db.pduid_pdu, pdu_id, Json(json));
txn.insert_raw(&self.db.eventid_pduid, ...);
txn.put_raw(&self.db.roomid_tscount_pducount, ...);
txn.execute();
```

`eventid_mxc` 與 `mxc_refcount` 的 **+1 加進這個交易**，就天然原子 —— 不會出現「事件寫進去了
但計數沒加」。而且本地與遠端事件都經過 `append_pdu`（`append_incoming_pdu` 也呼叫它），
是**單一咽喉點**。

| 動作 | 位置 |
|---|---|
| **+1** | `append_pdu_json` 的既有交易裡 |
| **−1** | `redact.rs` 的 `redact_pdu`、`rooms/delete` 的 `purge_room`（批次） |
| **實際刪 bytes** | 新的 worker，抄 `src/service/rooms/retention/mod.rs` 的形狀 |

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

crash、bug、migration、有人手動改 DB —— 任何引用計數都會漂移。所以真正的要求是**能離線重掃
重建**，而重建需要「事件 → mxc」可枚舉。

⚠️ **這正是 `eventid_mxc` 存在的理由**：redaction 會毀掉 `content.url`，一旦 redact，那個事件
對 GC 就變成隱形的，事後再也掃不出它引用過什麼。**所以引用關係必須在事件寫入時就記下來，
不能靠刪除時解析內容。**

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
