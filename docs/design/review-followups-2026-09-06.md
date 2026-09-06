# PR #24 合併後的再審，與外部審查（2026-09-05 兩輪）的逐條驗證

> **這份文件回答：main `389152df6`（PR #24 合併後）還有哪些已確認的缺陷、各自的證據在哪一行、建議怎麼修、怎麼驗。**
> 狀態：維護者 2026-09-06 同意三支都修（§4）。✅ `media/managed-origin`（§2.1、§2.6、§2.7、§2.9）PR #26 已合併；🔧 `wbf/auth-and-ws-lifetime`（§2.3、§2.4）實作中；`media/upload-lifecycle`（§2.2、§2.5、§2.8）待開。
> 來源兩個：(1) 持有者集合是實作到一半重做的（[media-holders.md](media-holders.md)），合併後對 main 重看一次；
> (2) `../external-review/wbfuwunel-2026-09-05.md` 與 `-v2.md`，一位外部審查者對 `0c964d522`／`3091c7ce3` 做的靜態審查，共 9＋5 條。
> 外部審查的每一條都**對現在的程式碼重新讀過**再下結論，不沿用它的判定；它看的版本沒有 #22 與 #24。

## 0. 一句話

持有者集合本身站得住：五張表只有 `media_refs` 寫、所有拿掉都是集合刪除、決策在每 mxc 一鎖下。**但有一條 P1 是新引進的**：既存媒體
第一次被生成縮圖時拿到 `mxc_managed` 列，從那一刻起它是「受管、沒有持有者」，7 天後被掃掉 —— 正好是外部審查第 2 條（舊媒體生成縮圖後失去
哨兵保護）在新模型上的同一個形狀。外部審查 14 條裡：4 條已由 #24 修掉、1 條變成漏水方向、**8 條仍在**、1 條是文件講反話。

## 1. 外部審查逐條對照

| # | 外部審查的判定 | 現在（`389152df6`） | 證據 | 結論 |
|---|---|---|---|---|
| v1-1 | E2EE 附件引用不可見，migrate 會刪活附件 | migrate 拔掉；E2EE 由宣告建立持有者 | `media_refs/attachments.rs`、`api/client/send.rs:135-150` | ✅ 已修 |
| v1-2 | 舊媒體生成縮圖後失去哨兵保護 | **同一形狀重現**：縮圖建列 → 既存媒體拿到 `mxc_managed` → 沒有持有者 → 7 天後掃掉 | `media/data.rs:352-376`（`exists_blocking(...).is_err()` 就 insert）、`media/thumbnail.rs:296`（生成縮圖呼叫它）、`media_refs/collect.rs:33-45, 63-90` | 🔴 **P1，§2.1** |
| v1-3 | 並行更新頭像重複 −1 | 集合語意下不會多刪；但兩個並行更新各自 `hold` 自己的新頭像、`release` 同一個舊的，輸的那個變**幽靈持有者**，永不釋放 | `profile/mod.rs:370-410`、`media_refs/mod.rs:416-430` | 🟡 漏水方向，§2.7 |
| v1-4 | 並行 purge 重複 −1 | `release_range` 是集合刪除，重做 no-op；`drop_original` 拿 `Backup` 也是 | `media_refs/mod.rs:262-300`、`retention/mod.rs:170-180`；e2e10「同一 purge 跑兩次」綠 | ✅ 已修 |
| v1-5／v2-3 | Status 冷載入不持鎖，能復活已 Seal 的上傳 | `upload_status → owned_upload → hot_upload → remember_upload` **沒有** `upload_locks` | `media/upload.rs:363-364, 557-591` | 🔴 **P2，§2.5** |
| v2-5 | sweeper 鎖下只確認宣告還在，不重讀 `last_chunk_at_secs` | 仍是 | `media/upload.rs:480-504` | 🔴 **P2，§2.5** |
| v1-6 | 本地儲存的 Seal 把整檔收進記憶體 | 仍是：`store_staging_file` 傳 `Some(len)`，本地 provider 的 `multipart_threshold` 是 `usize::MAX`，走 `try_collect::<Vec<_>>()` | `media/upload.rs:644-668`、`storage/provider.rs:107-125, 583-589` | 🔴 **P1，§2.2** |
| v1-7 | S3 的 Seal 產生 1 MiB parts（S3 非末段至少 5 MiB） | 仍是：同一段 1 MiB 的 `try_unfold` 進 `put_multi` 每塊一個 `put_part` | 同上＋`provider.rs:186-190` | 🔴 **P1，§2.2** |
| v1-8 | `/_wbf/v1/pack` 不查帳號鎖定 | 仍是；WebSocket 握手沿用同一個 `authenticate`；也沒有 `suspended` 檢查 | `api/client/wbf/mod.rs:102-127`（只查 token 與到期）、對照 `api/router/auth.rs:96-121` | 🔴 **P1，§2.3** |
| v1-9 | 墓碑 365 天 TTL 不會清 key | 正確：`RANDOM_SMALL` 是 Universal compaction，`set_ttl` 在 Universal／Leveled 只是 compaction 觸發條件，不刪 key；只有 FIFO 會刪整個檔 | `database/maps.rs:277-283`、`descriptor.rs:167-168`、`cf_opts.rs:46` | 🟡 文件講反話，§2.9 |
| v2-1 | WebSocket 的 `State` 逃出 request 生命週期 | 正確：`State` 是 `*const Services`，`on_upgrade` 另 spawn task，`stop()` 之後 `try_unwrap` 不知道有這條 task | `api/router/state.rs:5-8, 55-75`、`api/client/wbf/ws.rs:72, 82`、`router/run.rs:108-125` | 🔴 **P1，§2.4** |
| v2-2 | WebSocket 在登出／到期後仍有權限 | 正確：`serve` 只拿到 `OwnedUserId`，之後每個 pack 不再驗 | `ws.rs:82-135` | 🔴 **P1，§2.4** |
| v2-4 | 宣告／進度拆表沒有遷移，舊上傳卡住配額 | 正確：sweeper 只掃 `mediaid_upload_progress`；沒有進度列的舊宣告永遠不會被掃，staging 檔也因宣告還在而不刪，`count_uploads_for_user` 仍算它 | `media/upload.rs:484-491, 517-519` | 🟡 P2，§2.8（影響範圍取決於有沒有 #16～#18 之間的庫） |
| v2「上次 8 項未修」 | — | 對 `3091c7ce3` 成立；對 `389152df6`：v1-1、v1-4 已修，v1-3 降級，其餘仍在 | — | 上表 |

外部審查**沒有**編譯或跑過（它自己寫明），也沒看到 #22／#24；但上面每一條「仍在」都是我讀現在的程式碼確認的，不是轉述。
它沒抓到的、我這輪抓到的在 §2.6（`Interfix`）。它的兩個 CRC／向量驗證與 `git diff --check` 跟我們的單元測試一致。

## 2. 要修的，照嚴重度

### 2.1 🔴 P1：既存媒體被縮圖拉進 `mxc_managed`，7 天後被掃（✅ `media/managed-origin`，PR #26）

維護者 2026-09-06 的判斷，與這裡的修法一致：只有 `create` 與 Seal 確立受管；縮圖跟原檔同一個 mxc，壽命已經跟著原檔（前綴刪除），不需要自己的 mxc 或外鍵。

**現況**：`create_file_metadata`（`media/data.rs:352`）對**任何**一列（原檔、上傳的縮圖、生成的縮圖）都做「`mxc_managed` 沒列就 insert 現在時間」。
註解只想到「同一 mxc 的縮圖不能重設時鐘」，沒想到**沒列的可能是既存媒體**。生成縮圖（`thumbnail.rs:296`）對一張 2026-09-06 之前上傳、
被十則明文事件引用的圖做完，它就變成「受管、建立時間＝現在、持有者集合為空」（舊事件從沒建過持有者）。7 天後 `sweep_unreferenced` 刪它，
十則活訊息指向 410。**媒體壽命的承諾在既存資料上被打破**，而觸發條件只是有人看了一眼縮圖。

**修法**：`create_file_metadata` 加一個呼叫者必填的參數說明**建的是什麼**：

```
enum FileOrigin { NewMedia, DerivedOfExisting }
```

只有 `NewMedia` 寫 `mxc_managed`（仍保留「已有列不重設」）；`DerivedOfExisting` 永不寫。四個呼叫點：`media/mod.rs:287`（`create`，新原檔）與
`upload.rs:434`（seal，新原檔）傳 `NewMedia`；`thumbnail.rs:64`（上傳縮圖）與 `thumbnail.rs:296`（生成縮圖）傳 `DerivedOfExisting`。
不用「查 `mediaid_file` 前綴是否為空」推斷 —— 那是在 consumer 端靠巧合，呼叫者本來就知道自己在建什麼（A4：跨邊界只傳資料）。
順手把 `exists_blocking(...).is_err()` 改成只認 `NotFound`：DB 讀錯時不寫列（fail closed 是「不受管」）。

**驗收**：e2e10 加一個情境：用舊 binary 建庫、上傳一張圖並在明文事件引用 → 換新 binary、`WBFUWUNEL_MEDIA_GRACE_SECONDS=3` → 抓縮圖 →
等兩輪掃描 → 原圖與縮圖仍 200，`!admin media refcount` 印「predates the holder model」。單元：`create_file_metadata` 兩種 origin 對
`mxc_managed` 的寫入（真 DB 測試，`media/data.rs` 已有夾具可抄）。

**結果（2026-09-06）**：e2e10 情境 4 綠（17/17）；**紅燈驗過** —— 把判斷暫時改回舊行為重建，[4.2] 讀到 `held=410 free=410 thumb=410`，
連被引用的既存圖都被掃掉；還原後再綠。e2e9 26、e2e8 37 回歸綠。單元：`holder.rs` 加 `a_media_prefix_does_not_match_a_longer_uri`（§2.6）；
`create_file_metadata` 兩種 origin 的真 DB 單元測試**沒做**（`media/data.rs` 的測試夾具只有 CBOR round-trip，沒有開 DB 的），由 e2e 情境 4 涵蓋。

### 2.2 🔴 P1：Seal 在本地儲存收整檔進記憶體；S3 的 parts 太小

**現況**（`upload.rs:644-668`）：讀 1 MiB 一塊做成 stream，但 `provider.put(name, Some(len), chunks)`：本地 provider 的門檻是 `usize::MAX`，
`size < threshold` 永遠成立 → `try_collect::<Vec<_>>()` 把整個 stream 收進記憶體再 `put_single`（`provider.rs:117-125`）。10 GiB 的合法上傳
Seal 一次 RSS 加 10 GiB。S3 那邊超過 100 MiB 門檻走 `put_multi`，但每個 `put_part` 是 1 MiB（`provider.rs:186-190`），違反 S3 非末段 ≥ 5 MiB，
`complete()` 會失敗 —— 而 A 支的驗收只跑過本地。

**修法**：`store_staging_file` 改成**永遠**走串流：`size` 傳 `None`（`put` 對 `None` 直接 `put_multi`，`provider.rs:107`），每塊大小用
provider 的 `multipart_part_size()`（S3 預設 10 MiB；本地回 `usize::MAX`，夾成 8 MiB）。`multipart_part_size` 現在是 `Provider` 的私有 fn，
改 `pub(crate)`。object_store 的 `LocalFileSystem::put_multipart` 是寫暫存檔再 rename，記憶體只有一塊。

**驗收**：e2e6 加「本地 Seal 512 MiB，過程中量 server 的 working set 不超過啟動值 + 64 MiB」（PowerShell 讀 `Get-Process` 的 `WorkingSet64`）；
S3 用 MinIO 容器跑一次 ≥ 100 MiB 的 Seal（沒有 MinIO 就記成「未驗」，不要寫成綠）。

### 2.3 🔴 P1：`/_wbf/*` 與 WebSocket 不查帳號鎖定

**現況**：`wbf/mod.rs:102-127` 的 `authenticate` 只做 token 查找與到期；標準路由的 `locked_account_check`／`suspended_account_check`
（`router/auth.rs:96-121`）它都沒走。鎖帳號刻意保留 access token（MSC3939），所以被鎖的人拿同一個 token 打 `/_wbf/v1/pack` 照樣
Create／Chunk／Seal／Read，WebSocket 握手也一樣。

**修法**：`authenticate` 在拿到 user 之後呼叫 `services.users.locked_check(&user)`（就是 auth.rs 用的那個），錯誤照 MSC3939 回 401
`M_USER_LOCKED`。`suspended` 那組是路由白名單（membership、建房、profile），媒體不在裡面，**不加**，跟標準端點一致。
`authenticate` 回傳改成一個 struct（`user`、`device`、`expires_at`、`token`），§2.4 要用。

**驗收**：e2e 加「鎖帳號後同 token 打 `/_wbf/v1/pack` 與 WS 握手都 401；解鎖後恢復」。

### 2.4 🔴 P1：WebSocket 連線的生命週期與 session

兩件事，同一條線：

- **`State` 懸空**（`ws.rs:72`）：`on_upgrade` 把 `crate::State`（`*const Services`，`state.rs:5-8`）搬進另一個 spawn 的 task；它的安全前提
  「所有 request 結束後才釋放 Services」（`state.rs:63-68`）對升級後的 socket 不成立 —— HTTP 回 101 就算結束。`stop()`（`run.rs:108-125`）
  `services.stop().await` 後 `Arc::try_unwrap`；還在等下一個 frame 的 socket task 醒來就 deref 已釋放的 Services。正常關機時 runtime 還活著，
  這是可觸發的 use-after-free。
- **登出／到期後仍有權限**（`ws.rs:82`）：握手驗一次，之後只留 `OwnedUserId`。登出、撤 device、token 到期都不會關線，只要每 300 秒動一下就能
  無限期用。

**修法**：
1. `serve` 的 loop 改 `tokio::select!`，多一支 `services.server.until_shutdown()` → break。把 socket task 登記進 `Services` 的一個
   `JoinSet`（或既有 `manager` 的 task 表），`services.stop()` 等它們 join 完再回 —— 光靠 signal 還有「signal 到 task 醒來」的窗口，
   要有人等它。`until_shutdown` 的 `Arc<Server>` 是可以 clone 出來持有的，task 裡對 Services 的每次 deref 都在 join 之前。
2. 每個 pack 處理前重驗：`find_from_token(token)` 仍回同一個 user、`expires_at` 未到、`locked_check` 過；任何一項不過 → 回一個
   `Error(Unauthorized)` pack 然後關線。一次點讀，跟一個 `Chunk` 的 DB 成本同量級；不做「每 N 秒」的快取，那又是一份會過期的真相。

**驗收**：e2e7 加「WS 連著 → HTTP logout → 下一個 pack 回 Unauthorized 且線關」；「WS 連著 → `server shutdown` → 程序乾淨退出、log 沒有
dangling references」（`run.rs:125` 的 `debug_error!` 就是現成的探針，e2e 開 debug log 抓它）。

### 2.5 🔴 P2：上傳 `Status` 冷載入不持鎖；sweeper 鎖下不重讀進度

- `upload_status`（`upload.rs:363`）→ `hot_upload`（`:566`）：快取沒中就讀 DB 再 `remember_upload` 寫回，**全程沒拿 `upload_locks`**。
  交錯：重啟後快取空 → Status 讀到已收完的宣告與進度 → 另一條線 Seal 成功、刪列、`forget_upload` → 延遲的 Status 把舊快照 `remember` 回去 →
  之後 Abort 信任快取，`discard_upload` 刪掉**已 seal 媒體**的 `mxc_chunk`／`mxc_chunked`（含加密描述）。同型的交錯也能把 Chunk 已 ACK 的
  進度蓋回舊值。
- `sweep_uploads`（`:480-504`）：過期清單來自鎖外的快照；拿到鎖後只確認宣告還在，**沒重讀 `last_chunk_at_secs`**。sweeper 等鎖時進來的
  Chunk 成功寫入並 ACK，sweeper 接著照舊快照 discard 整個上傳。

**修法**：`upload_status` 也拿 `upload_locks.lock(&upload_id)`（讀路徑會寫快取，就是寫路徑）。sweeper 鎖下 `find_progress` 重讀，
用新的 `last_chunk_at_secs` 再判一次過期，沒過期就 `continue`。兩處都是幾行。

**驗收**：單元不好做交錯；e2e 加「Chunk 到最後一塊 → 立刻 Status ＋ Seal 並行（PowerShell 兩個 job）→ Abort 回 NotFound、媒體 Info 正常」；
sweeper 用 `media_upload_ttl=2`：「等 3 秒 → 同時送 Chunk 與觸發 sweep → 上傳仍在、Status 正確」。

### 2.6 🟡 P2：`(mxc,)` 前綴少了 `Interfix`

`has_holders`（`media_refs/mod.rs:478-483`）與 `forget_media`（`:237`）用 `(mxc,)` 當前綴。一元 tuple 序列化**沒有尾端分隔符**（`ser.rs:151-171`，
分隔符只寫在元素之間），所以 `mxc://s/abc` 的前綴也匹配 `mxc://s/abcd‖…`。這個 repo 的慣例是 `(mxc, Interfix)`（`media/data.rs:544`）。
現在不會出事是因為本站媒體 id 是定長隨機字串、分塊上傳的 id 是 16 位 hex —— 一個 16 字元的 id 是另一個 32 字元 id 的前綴，機率是 16⁻¹⁶；
但正確性不該靠這個（A5：不要靠巧合正確）。後果方向：`has_holders` 假陽性 → 媒體永不刪（漏水）；`forget_media` 列到別人的房間 → 只刪
自己 `(room, mxc)` 的列，無害。

**修法**：兩處改 `(mxc, Interfix)`；`holder.rs` 的 `keys_start_with_their_prefixes` 測試加一條「相似 mxc 的鍵不被對方的前綴匹配」。
PR #26 review（rumia）再抓到同形的第三處：`release_room` 掃 `room_mxc` 用 `(room,)`，兩個 room id 一個是另一個的位元組前綴時（`!x:server` 與 `!x:server2`）
刪房會連後者的媒體一起釋放。同支補成 `(room, Interfix)`。`media_refs` 裡其餘前綴都是雙元素以上。

### 2.7 🟡 P2：頭像併發更新留下幽靈持有者

`set_profile_keys`（`profile/mod.rs:372-374`）在交易外讀舊頭像 A；兩個並行更新 A→B、A→C 都讀到 A，各自 `hold(B)`／`hold(C)`、`release(A)`。
profile 最後是 B 或 C 其中之一，**輸的那個 mxc 掛著 `(Avatar, u)` 永遠不釋放**：下次換頭像 `release(old)` 讀的是 profile 現值，不是它。
方向是漏水（外部審查 v1-3 講的是刪活媒體，那在集合語意下不會發生），但它違反「Avatar 持有者一個人只有一個」。

**修法**：`set_avatar_ref` 不信任呼叫者傳的 `old_mxc`，改用反向索引：`list_mxcs_of(Holder::avatar(user))` 列出這個人現在持有的**全部**，
除了 `new_mxc` 以外全 `release`，再 `hold(new_mxc)`。冪等、自癒（幽靈在下次更新時被清）。`set_avatar_ref` 因此變 async，兩個呼叫點都已在 async 裡。
`clear_profile_keys` 同理不用先讀 `avatar_url`。

**驗收**：單元（真 DB）：對同一 user 連續 `set_avatar_ref(A→B)`、`set_avatar_ref(A→C)`（模擬都讀到舊值 A）→ `list_holders(B)` 為空、
`list_holders(C)` 只有 `(Avatar, u)`。

### 2.8 🟡 P2：升級前的舊上傳沒有清理路徑

若有 #16 之後、#18 之前建的庫，`mediaid_upload` 裡有帶進度的舊格式宣告、沒有 `mediaid_upload_progress` 列。現在：`hot_upload` 讀不到進度回 `None`
→ Status／Chunk／Seal／Abort 全 NotFound；sweeper 只掃進度表不會碰它；staging 檔因「宣告還在」不刪（`:517-519`）；`count_uploads_for_user`
仍把它算進每人 5 個的配額。**卡死一格配額，永不釋放。**

**修法**：sweeper 多走一遍 `mediaid_upload`：宣告存在、進度不存在、且宣告的建立時間超過 `media_upload_ttl` → `discard_upload`（刪宣告、
span、staging）。不做格式轉換 —— 那批上傳續不了也沒關係，client 重傳；要的是資源回收。舊 `Upload` 若 CBOR 解不開（多了欄位），
`find_upload` 現在回 `None` 就會被當「沒有宣告」→ 走原本的「進度沒宣告」分支，但那條只 `del_upload`，要確認它也刪 staging。
**維護者先答**：你的伺服器有沒有跑過 #16～#18 之間的版本？沒有的話這條只是防禦，優先度最低。

### 2.9 🟡 文件講反話：墓碑的「365 天 TTL」不會刪 key

`mxc_tombstone`（`maps.rs:277-283`）設 `ttl: 365 天`，但 `RANDOM_SMALL` 是 Universal compaction（`descriptor.rs:167-168`）；RocksDB 的
`Options::ttl` 在 Leveled／Universal 下只是「超過 ttl 的檔案排進 compaction」，**不刪 key**；只有 FIFO 會把整個過期檔刪掉（`RANDOM_SMALL_CACHE`
就是那樣用的）。`find_tombstone` 也不看 `deleted_at_secs`。所以墓碑是永久的，media-gc.md §6（第 190 行）說「365 天 TTL」是假的。`mediaid_upload_progress`
的 ttl 同理，但它有 sweeper 真的刪列，只是註解不準。

**修法**：文件改成「墓碑永久保留，一筆約 80 byte；一百萬次刪除約 80 MB」—— 那是可接受的、而且 410 永遠成立比一年後變 404 更好懂。
🚫 不改成 FIFO：FIFO 超過 `limit_size` 會丟**最舊的整個檔**，新墓碑也可能一起沒了。要真的到期就得寫 compaction filter，不值得。
`maps.rs` 的 `ttl` 欄位留著（無害），註解改對。

## 3. 這輪再審沒抓到問題、但看過的

- `release_range` 用 `PduCount::into_signed()` 當 `g_seq` 邊界（`purge.rs:48`）：`Positions.g_seq` 就是這個值（`pdu/seq.rs:5`），一致。
- `redact_pdu` 對沒有 positions 的事件（#22 之前）不動持有者（`redact.rs:105`）：那些事件本來也沒建過持有者，媒體是既存的、不受管。對。
- `hand_to_collector` 在收集器沒跑時（啟動／關機）只 `debug!`，交給掃描（`mod.rs:502-522`）：掃描只看「建立超過 7 天」，所以關機瞬間被釋放的
  **新**媒體要等到它滿 7 天才會被掃。可接受，已在註解裡。
- `forget_media` 在 `media.collect` 裡、收集器的鎖之下（`collect.rs:19-30` → `media/mod.rs:344-371`）；`!admin media delete` 走同一條，沒有鎖
  —— admin 手動刪跟一個正在 `hold` 的 append 交錯，最壞是事件指向墓碑，那是 admin 明確要的結果。
- 鎖序：`hold_media_list` 在房間 `insert_lock` 之內、排序去重；收集器與掃描不拿房間鎖；`upload_locks` 與 mxc 鎖不重疊。沒有環。
- `Event/Send` 與 header 兩個入口共用 `send_message_event`，txn_id 冪等先於宣告驗證（`send.rs:135-139`）。

## 4. 建議的分支切法

三支，互不依賴，可以分開審：

| 分支 | 內容 | 為什麼分開 |
|---|---|---|
| `media/managed-origin` | §2.1、§2.6、§2.7、§2.9 | 全在 `media_refs`／`media/data.rs`，一起看得完；§2.1 是唯一會刪活資料的 |
| `media/upload-lifecycle` | §2.2、§2.5、§2.8 | 全在 `media/upload.rs` 與 `storage/provider.rs` |
| `wbf/auth-and-ws-lifetime` | §2.3、§2.4 | `api/client/wbf/`，動到 `Services` 的關機流程 |

每支：改 → 相關單元 → e2e 對應情境 → commit → 全套一次 → PR。CHANGELOG 各一列。
