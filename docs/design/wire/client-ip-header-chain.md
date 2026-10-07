# client 位址：閘，與閘之內那條寫死的 header 鏈

✅ **已實作：PR #108**（維護者 2026-10-05 定鏈與順序，2026-10-06 改了「值無效怎麼辦」那一條）。
程式在 `src/api/router/client_ip.rs`；規則的敘述在
[/docs/design/wire/pack-pipeline.md](pack-pipeline.md) §2.2 與
[/docs/design/wire/wire-format.md](wire-format.md) §6.1，部署面在 `/docs/deploying/generic.md`。

⭐ **這一份只留「為什麼」** —— 規則本身在上面那幾處，過程（測試計畫、要跟著改的文件清單）已經刪掉。

## 1. 兩層分開看：閘（誰可以被信）＋ 鏈（信哪一個）

**閘**（維護者 2026-09-25 定，PR #108 **一個字沒動**）：設了 `reverse_proxy_ip_header` 就讀那一個、
**而且只讀那一個**；OR peer 落在 `localhost_ip` 裡就走鏈；兩條都不成立就只信傳輸層 peer、**完全不碰 header**。

🚨 **這就是為什麼「多支援幾個 header」不是新增攻擊面**（維護者 2026-10-05）：
直連過來的 client 送什麼 header 都不會被看一眼。而閘之內，peer 是我們自己的代理或同一台機器，
**清掉 client 送來的假 header 是代理的責任** —— 那是運維的部署承諾，不是程式能代替的保證。
📎 這個信任劃分跟業界一致（代理 sanitize、應用讀），`generic.md` 把那個承諾寫明了。

**鏈**：`X-Forwarded-For`（最右）→ `X-Real-IP` → `X-Client-IP` → `Client-IP` → peer。
🚫 `Forwarded`（RFC 7239）與四個供應商 header（CF／Akamai／Fly／CloudFront）**刻意不在鏈裡** ——
那種部署要在設定裡指名，而指名比猜精確。
🚫 **指名了一個就不走鏈**：那是運維說「我只信這個」的方式，也是把鏈關掉的方式。

## 2. 為什麼「最右邊」是最後一格，不是最後一格**解得開的**

`X-Forwarded-For` 是一串，每一跳把**它看到的來源**接到**右邊**：

```
client(1.2.3.4) → nginx → tuwunel        X-Forwarded-For: 1.2.3.4
                                                          ↑ nginx 寫的，最右
```

⚠️ **左邊的每一格都是 client 寫得出來的**：client 連線前先送 `X-Forwarded-For: 8.8.8.8`，
nginx 不刪、只 append ⇒ server 收到 `8.8.8.8, 1.2.3.4`。所以只有**最右邊**那一格有人為它負責。

🚨 而原本的實作是 `filter_map(parse).next_back()` ＝「最右邊**解得開的**」⇒
`8.8.8.8, garbage` **往左退一格，退到 client 自己挑的值**。那是 fail open，也是外部審查那條 🟡。
⇒ 現在：**最後一格就是答案，解不開就是沒有答案**，而沒有答案的去處是傳輸層 peer。

## 3. 為什麼「值無效」跟「沒設」走同一條路（改過一次）

⭐ **這一節留著是因為它被推翻過一次，而推翻它的理由比原來的規則好。**

提案初版寫的是「鏈往下走的條件是**這個 header 不在**，不是**在、但值無效**」，理由是
「代理寫出垃圾就該退回 peer」。🚨 而第一版實作跟那句話相反（一行 `find_map`，`Option` 說不出
那個差別），抓到它的是我照測試計畫**先寫好的那條測試** —— ⭐ 所以矛盾在第一次跑測試時就浮出來。
當時的修法是把區別放進型別（`enum HeaderOutcome { Absent, Address, Unusable }`）。

✅ **維護者 2026-10-06 定了，而且是往下走：**

> 如果 header 值無效，就跳下一個 header，全部都無效或沒設 就 fallback 到 tcp ip

| 為什麼這個比較好 | |
|---|---|
| ⭐ **救得到一種真實部署** | 代理只寫 `X-Real-IP`、而 client 送來的 XFF 被原封轉過來（nginx 沒設 `proxy_set_header X-Forwarded-For` 時就是透傳）。「停下來」會讓那種部署的每個請求都變成代理的 peer ⇒ 🚨 **全站共用一個限流桶**，正是這整條規則要消滅的症狀 |
| 🚨 **沒有多開任何門** | client 只能靠「讓前一格無效」把讀取推到後面的 header —— 但⭐ **寫得出前一格的 client，直接在那裡放一個解得開的位址就會被第一格採用**。⇒ 「停下來」防不住會偽造的人，只罰得到設定沒寫全的運維 |
| 📎 **更常被觸發** | `proxy_set_header X-Forwarded-For "";`、尾巴逗號、空值 —— 都是「header 在、值無效」 |

⇒ 🚫 **`HeaderOutcome` 三態一起拆掉**：`Absent` 與 `Unusable` 行為完全相同之後，留著那個 enum
就是**純儀式**（CLAUDE.md A2）。鏈回到一行 `find_map`。

🚨 **而 §2 那個修法完全不受影響** —— 改掉的只是 `None` 之後往哪裡去：往**後面的 header**，
🚫 不是往 XFF 串的**左邊**（左邊離我們更遠，而夠左邊就是 client 自己）。
📌 `8.8.8.8, garbage` 在兩種語意下都落到 peer。
