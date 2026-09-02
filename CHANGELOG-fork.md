# wbfuwunel 的變更紀錄

**這份檔案只記這個 fork 加的東西，不記上游的變更。** 上游自己的紀錄看
[`RELEASE.md`](RELEASE.md)。

> 📌 **每一筆新功能都要在這裡留一行。** 這是「這個 fork 到底跟上游差在哪」的**唯一**權威清單
> —— 讀 `git log --oneline upstream/main..main` 只看得到動了什麼，看不到**為什麼**。

## 怎麼寫

- **最新的放最上面。**
- 一筆一個標題，底下三件事：**做了什麼**、**為什麼**、**去哪看細節**（設計文件與 PR）。
- 🚫 **不要只寫「加了 X」** —— 三個月後讀的人需要的是為什麼，不是 changelog 本身就看得出來的東西。
- ⚠️ **行為有變、或有已知限制，要寫進來。** 這裡是使用者與維護者會先看到的地方，
  藏在設計文件深處的警告等於沒寫。
- 只在**功能實際併進 `main`** 之後才寫。提案階段的東西留在 `docs/design/`。

---

## 未發佈

### 媒體引用計數（`mxc_refcount`）—— 取代下面那筆的列式索引

**做了什麼** —— 引用不再是「一列一個持有者」，而是**一個精確的有號整數**：`mxc_refcount: mxc → i64`。
+1／−1 走 RocksDB 的 **merge operator**（`src/database/engine/merge.rs`）—— 它是一筆寫入，排進事件或
profile 既有的交易裡，讀取時才合併；所以原子性跟之前一樣，但 worker 讀到的是**數字**，不是前綴 seek。
為此新增了 `Txn::merge` 與 descriptor 的 `merge: MergeKind` 欄位；`mxc_holder` 標為 `DROPPED`。

**為什麼** —— 維護者的批評成立：列式索引在移除引用的當下答不出「現在幾個」，只能事後 seek，那不算計數。
詳見 [`docs/design/media-gc.md`](docs/design/media-gc.md) §2。

**⚠️ 三條行為，都要知道**：

1. **redact 不歸零。** `save_unredacted_events`（預設開）保留的原文備份才是持有者：**備份被丟掉的地方就是
   −1 的地方** —— retention worker reap 過期備份、`purge_history` 丟備份，都從備份內容讀 mxc 再扣，同一交易。
   備份關閉時 redact 當下扣。所以媒體活得跟「管理員還看得到那則訊息」一樣久。
2. **既存媒體不重算、也不會被自動清。** 計數上線前建立的媒體沒有列；第一次被 ±1 時合併函數把它變成
   **哨兵**（`i64::MIN`），之後所有 ±1 都被吞掉。`!admin media refcount <mxc>` 會印「no reference count」或
   「sentinel」—— **兩者都不是 0**。重算要等 `migrate-references`（尚未實作）。
3. **上傳不是引用。** 媒體建立時（`create_file_metadata`）只把列開在 0；引用是事件內容與個人頭像，各 +1。
   上傳後從未送出的檔案停在 0，但沒有任何路徑會因此刪它（刪除尚未實作；實作後也只看被扣過的 mxc）。

**怎麼觀察** —— `!admin media list-references` 改名為：

```
!admin media refcount mxc://example.org/abc123
```

**⚠️ 這一版仍然一個 byte 都不刪。** 刪除 worker、墓碑、`migrate-references` 是下一支。

**去哪看** —— [`docs/design/media-gc.md`](docs/design/media-gc.md)；實作 `src/database/engine/merge.rs`、
`src/service/media_refs/`、`src/service/rooms/retention/mod.rs`（`drop_original`）。

### 媒體引用索引（`mxc_holder`）—— 已被上面那筆取代，留作紀錄

**做了什麼** —— 新增服務 `media_refs` 與 column family `mxc_holder`
（鍵是 `mxc || 持有者種類 || 持有者 id`，值是空的）。任何東西引用一份媒體時記一列，
放棄引用時移除。於是「這份媒體還有沒有人在用」第一次變成**一次前綴 seek 就答得出來**的問題。

目前有兩種持有者：**事件**（`0x01`，寫入／backfill／redact／歷史清除／房間清除五處維護）與
**個人頭像**（`0x02`，設定頭像與停用帳號兩處維護）。房間頭像不需要特別處理 ——
`m.room.avatar` 是 state 事件，本來就走事件那條。

📌 **維護規則：新增一個寫 `pduid_pdu` 的地方，就欠這個索引一筆。** 寫入端漏一個，那些事件的
媒體會讀成「無人引用」—— 而那是不可逆的方向。

**為什麼** —— 在這之前，redaction 只把事件剝空，完全不碰媒體 bytes；而且沒有任何索引
把事件連到媒體，所以 server 根本無法判斷一份媒體是否還被引用。媒體因此比引用它的每一則
訊息都活得久。詳見 [`docs/design/media-refcount.md`](docs/design/media-refcount.md)。

**⚠️ 這一版不刪任何東西。** 只建立與維護索引，**一個 byte 都不會被移除**。刪除是不可逆的
那一半，要等索引在真實流量上驗證過才接上。

**怎麼觀察** —— 新增管理員指令：

```
!admin media list-references mxc://example.org/abc123
```

⭐ 沒有這個指令，「索引是對的」就是**不可否證**的宣稱。**接上刪除之前，請先用它在真實資料上
看幾筆**：傳一張圖、查一次（應該有一筆）、redact 那則訊息、再查一次（應該沒有了）。

**⚠️ 唯一剩下的限制** —— **索引只認它上線之後發生的事**。這個版本之前就存在的事件與頭像
從來沒被記錄過，所以在索引裡會顯示為「無人引用」。
🚨 **重建工具要跟刪除功能一起來**，否則第一次 GC 會刪掉所有舊資料引用的媒體。

📎 種類碼（`0x01` / `0x02`）是**磁碟上的格式**，而那張表就是刪除功能可以上線的閘門：
**每一種持有者都必須列進去**，沒列進來的在索引看來就是「沒人用」。

**去哪看** —— 設計文件 [`docs/design/media-refcount.md`](docs/design/media-refcount.md)；
實作 `src/service/media_refs/`、`src/core/matrix/media_ref.rs`。
