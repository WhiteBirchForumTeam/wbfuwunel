# client 位址讀一條寫死的 header 鏈，而每一個 header 自己 fail closed

✅ **維護者 2026-10-05 同意**（含 §3.1 的 `X-Real-IP`、§3.2 的「指名了就不走鏈」、§3.3），
**2026-10-06 改了一條**：header 的值無效時跟「沒設」走同一條路（往下一個），⇒ §8。現行做法在
[/docs/design/wire/pack-pipeline.md](pack-pipeline.md) §2.2，程式在 `src/api/router/client_ip.rs`。

兩件事放在一支裡，因為它們改的是同一個函數家族，但**彼此獨立**：

| | 來自 | 性質 |
|---|---|---|
| **① 「最右邊」要是真的最右邊** | 外部審查 2026-09-29 的 🟡 | 🚨 **修 bug**，壞的方向是 fail open |
| **② 閘之內讀一條寫死的 header 鏈** | 維護者 2026-10-05 | ✨ 新功能 |

## 1. 閘不動 —— 先把這件事釘住，因為 ② 的安全性全靠它

⚠️ **以下是提案當時的程式**（這一支把函數改名成 `find_headers_to_believe`、回傳型別換成
`HeadersToBelieve`，但⭐ **閘的判斷一個字沒動**，那才是這一節要釘的東西）：

```rust
fn header_to_believe(extensions: &Extensions) -> Option<ReverseProxyIpHeader> {
    if let Some(&ConfiguredIpHeader(source)) = extensions.get::<ConfiguredIpHeader>() {
        return Some(source);                                   // 運維明講了
    }
    is_peer_local(extensions).then_some(LOCAL_PEER_DEFAULT)     // 否則只信本機 peer
}
```

⭐ **沒設 `reverse_proxy_ip_header`、peer 又不在 `localhost_ip` 裡 ⇒ 一個 header 都不讀。**

🚨 **這就是為什麼「多支援幾個 header」不是新增攻擊面**（維護者 2026-10-05）：
直連過來的 client 送什麼 header 都不會被看一眼。而閘之內，peer 是我們自己的代理或同一台機器，
**清掉 client 送來的假 header 是代理的責任** —— 那是運維的部署承諾，不是程式能代替的保證。
📎 這個信任劃分跟業界一致（代理 sanitize、應用讀），而且 `generic.md` 已經把承諾寫明了。

## 2. ①：「最右邊解得開的」不是「最右邊」

`X-Forwarded-For` 是一串，每一跳把**它看到的來源**接到**右邊**：

```
client(1.2.3.4) → nginx → tuwunel        X-Forwarded-For: 1.2.3.4
                                                          ↑ nginx 寫的，最右
```

⚠️ **左邊的每一格都是 client 寫得出來的**：client 連線時先送 `X-Forwarded-For: 8.8.8.8`，
nginx 不刪、只 append ⇒ server 收到 `8.8.8.8, 1.2.3.4`。所以只有**最右邊**那一格有人為它負責。

**原本**的程式（這一支換掉的就是它）：

```rust
.filter_map(|v| v.to_str().ok())                   // 讀不懂的整個 header 值跳過
.flat_map(|s| s.split(','))
.filter_map(|s| s.trim().parse::<IpAddr>().ok())   // 解不開的元素跳過
.next_back()                                       // ⇒ 最右邊「解得開的」
```

⇒ 🚨 `X-Forwarded-For: 8.8.8.8, garbage` 得到 **`8.8.8.8`** —— 往左退一格，退到 client 寫的那一格。

**修法**：最後一格就是答案，解不開就是沒有答案。

```rust
fn rightmost_x_forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()?          // 最後一個 header 值（多個值時只看最後那個）
        .to_str()
        .ok()?                 // 讀不懂 ⇒ 沒有答案，🚫 不退回前一個值
        .rsplit(',')
        .next()?               // 最右邊那一格，解不開也是它
        .trim()
        .parse()
        .ok()
}
```

⭐ **每一步失敗都是 `None`，而 `None` 的去處是 peer 位址**（`client_ip.rs:84`）—— 一個 client 改不了的值。
`rightmost_forwarded`（RFC 7239）同形修。

⚠️ **這一條跟 ② 無關，而且 ② 不會讓它變得沒必要**：鏈往下走是「這個 header 不在」，
🚫 不是「這個 header 裡面有垃圾」。後者表示代理寫的東西跟我們的預期不符 —— 那時退回 peer，不是去猜。

## 3. ②：閘之內讀一條寫死的鏈

維護者 2026-10-05：「我建議你 header 直接寫死，就是只抓 x-forwarded-for 沒有就抓 x-client-ip
再找不到 就是 client-ip 逐級下來。」

### 3.1 鏈的順序

**規則一句話**（維護者 2026-10-06 定）：**第一個「說得出一個位址」的 header 就是答案；
沒設的、以及設了但值無效的，都往下一個；全部都無效或都沒設 ⇒ 落到傳輸層 peer。**

| 順序 | header | 讀法 | 為什麼在這裡 |
|---|---|---|---|
| 1 | `X-Forwarded-For` | **最右邊**一格 | 唯一有「串」結構的，而且每個代理都寫 |
| 2 | `X-Real-IP` | 整個值 | ⭐ 我提議補的 —— nginx 最常見的那一行就是 `proxy_set_header X-Real-IP $remote_addr;`，比 `X-Client-IP` 常見得多（維護者 2026-10-05 同意）|
| 3 | `X-Client-IP` | 整個值 | 維護者指定 |
| 4 | `Client-IP` | 整個值 | 維護者指定 |
| — | 都沒有、或都無效 | → **peer 位址** | 落點永遠是傳輸層看到的東西 |

⚠️ **「無效」跟「沒設」同一條路**（這一條 2026-10-06 改過，原本是「無效就停下來」，見 §8）：
`X-Forwarded-For: garbage`、`X-Forwarded-For: `（空）、`1.2.3.4,`（尾巴是空格）都算無效 ⇒ 往下試。

🚫 **不在鏈裡**：`CF-Connecting-IP`、`True-Client-IP`、`Fly-Client-IP`、`CloudFront-Viewer-Address`、
RFC 7239 `Forwarded`。理由：它們是**特定供應商**的，而鏈是給「一般代理」的預設。
用那些的部署本來就要在設定裡指名一個（現在就支援），而指名比猜精確。

### 3.2 設定裡指名了一個的時候，🚫 不走鏈

現行契約是「naming one header does not open the others」（`client_ip.rs:101`）。⭐ **保留。**
指名是一句精確的話：「我前面那台代理把 client 寫在**這個** header」。
⇒ 讀不到就落到 peer，🚫 不往下試別的 —— 📎 這也讓運維可以**關掉**鏈（指名 XFF 就只有 XFF）。

✅ **維護者 2026-10-05 同意。** 另一種做法是「指名的排第一、然後接著走鏈」，🚫 不採 ——
它讓「我只信這個」變成無法表達的意思。

### 3.3 新增兩個可指名的 variant

`ReverseProxyIpHeader` 補 `XClientIp`、`ClientIp`（snake_case：`x_client_ip`、`client_ip`）。
📎 鏈裡讀得到的，設定裡也要指名得到 —— 兩邊的清單不一致會變成「文件說支援、設定寫了卻拒絕啟動」。

## 4. 這條鏈改了誰的意思

| | 之前 | 之後 |
|---|---|---|
| 沒設定、peer 在 `localhost_ip` | 只讀 `X-Forwarded-For` 最右邊 | 讀鏈（XFF → X-Real-IP → X-Client-IP → Client-IP）|
| 沒設定、peer 不在 `localhost_ip` | 🚫 不讀任何 header | **不變** 🚫 不讀任何 header |
| 設了 `reverse_proxy_ip_header` | 只讀那一個 | **不變** 只讀那一個 |

⇒ ⚠️ **唯一變的是「同機代理／unix socket」那一格**，而它變得更寬容：
代理只寫了 `X-Real-IP` 而沒寫 XFF 的部署，現在位址才會對。
📎 那正是今天會塌成「全站一個限流桶」的那種部署（§2.2 的 🚨），所以這個方向是把洞補小。

## 5. 測試計畫

`client_ip.rs` 已有 `mod tests`（axum `FromRequestParts` 直接驗），加表格式的案例：

| 要釘住的 | 輸入 | 期望 |
|---|---|---|
| ① 最右邊是垃圾 | `X-Forwarded-For: 8.8.8.8, garbage` | **peer**，🚫 不是 `8.8.8.8` |
| ① 最右邊是非 UTF-8 | 兩個 XFF header，後一個 invalid bytes | **peer** |
| ① 正常 | `8.8.8.8, 1.2.3.4` | `1.2.3.4` |
| ② 鏈：XFF 不在 | 只有 `X-Real-IP: 1.2.3.4` | `1.2.3.4` |
| ② 鏈：逐級 | 只有 `Client-IP: 1.2.3.4` | `1.2.3.4` |
| ② 鏈：XFF 在但無效 | `X-Forwarded-For: garbage` ＋ `X-Client-IP: 9.1.2.3` | **`9.1.2.3`** —— ⭐ 無效跟沒設同一條路（§8.2，維護者 2026-10-06 改過；初版這一列寫的是 peer）|
| 🔴 ② 鏈：全部無效 | 四個 header 都在、值都無效，而 XFF 的**左邊**有 `1.2.3.4` | **peer** —— 🚫 永遠不是 client 寫在左邊那個值 |
| 🔴 閘：非本機 peer | 上面每一種 header，peer 在公網 | **peer**，一律 |
| 🔴 指名了一個 | 指名 `x_real_ip`，只送 `Client-IP` | **peer**，🚫 不走鏈 |

⭐ 最後兩列是這支真正要守的東西：**鏈只在閘之內存在。**

## 6. 要跟著改的文件

| | 改什麼 |
|---|---|
| [/docs/design/wire/pack-pipeline.md](pack-pipeline.md) §2.2 | 「peer 落在 `localhost_ip` 裡就讀 `X-Forwarded-For`（最右邊那個）」→ 讀這條鏈；指向本文件 |
| `/docs/deploying/generic.md` | 第 208–227 行那段「which header is believed」 |
| `/docs/deploying/reverse-proxy-{nginx,caddy,traefik}.md` | 各自的 `proxy_set_header` 範例要說清楚哪些現在不用設了 |
| `/docs/design/index.md` | 收這一份 |
| `tuwunel-example.toml` | `reverse_proxy_ip_header` 的註解（由 `config_example_generator` 重生，🚫 不手改）|

## 7. 🚫 這支不做

- **閘的任何改動** —— `localhost_ip`、`reverse_proxy_ip_header` 的語意一個字不動。
- **`single_ip_header` 的 `headers.get()` 取第一個值** ⚠️ 我原本把它列成第三個問題，
  但它跟 ① 不同形：那幾個 header 按慣例是**覆寫**而不是 append，而代理 append 它們是配置錯誤。
  📌 鏈裡新加的三個（`X-Real-IP`／`X-Client-IP`／`Client-IP`）會沿用同一個讀法。
  要不要改成「只看最後一個值」是另一個題目，不在這支。
- **bridge router 的 `CatchPanicLayer`**（外部審查的另一條 🟡）—— 另一支。
  ✅ 那一支是 **PR #109**（[/docs/design/wire/bridge-catch-panic.md](bridge-catch-panic.md)）。

## 8. 「值無效」怎麼辦 —— 改過一次，最後是「跟沒設一樣，往下走」

⭐ **這一節留著是因為它被推翻過一次，而推翻它的理由比原來的規則好。**

### 8.1 原本的規則：無效就停下來

提案（§3.1 的初版）寫的是「鏈往下走的條件是**這個 header 不在**，不是**在、但裡面是垃圾**」，
理由是「代理寫出垃圾就該退回 peer，不該去讀一個 client 可能送的 header」。

🚨 **第一版實作跟那句話相反**：鏈是一行 `find_map`，而 `secure_extract` 回 `Option<IpAddr>` ——
「不在」跟「在、但是垃圾」都是 `None`，兩種都往下走。
📎 抓到它的是我照 §5 先寫好的那條測試，⭐ **所以矛盾在第一次跑測試時就浮出來，不是留給審查或使用者。**
當時的修法是把區別放進型別：`enum HeaderOutcome { Absent, Address(IpAddr), Unusable }`，鏈上
`Absent => continue`、`Unusable => return None`。

### 8.2 ✅ 最後的規則（維護者 2026-10-06 定）：無效跟沒設走同一條路

> 「如果 header 值無效，就跳下一個 header，全部都無效或沒設 就 fallback 到 tcp ip」

**為什麼這個比較好，而且不是把安全性讓掉**：

| | |
|---|---|
| ⭐ **它救得到一種真實部署** | 代理只寫 `X-Real-IP`，而 client 送來的 XFF 被原封轉過來（nginx 沒設 `proxy_set_header X-Forwarded-For` 時就是透傳）。「停下來」會讓每個請求都變成代理的 peer ⇒ 🚨 **全站共用一個限流桶**，正是這整條規則要消滅的症狀 |
| 🚨 **它沒有多開任何門** | client 只能靠「讓前一格無效」把讀取推到後面的 header —— 但⭐ **寫得出前一格的 client，直接在那裡放一個解得開的位址就會被第一格採用**。⇒ 「停下來」防不住會偽造的人，只罰得到設定沒寫全的運維 |
| 📎 **它更常被觸發** | `proxy_set_header X-Forwarded-For "";`、尾巴是逗號、空值 —— 都是「header 在、值無效」（cirno 與 rumia 在 PR #108 各自點到）|

⇒ 🚫 **`HeaderOutcome` 三態一起拆掉。** `Absent` 與 `Unusable` 現在行為完全相同，留著那個 enum 就是
**純儀式**（CLAUDE.md A2）：沒有第二種行為的型別不值得讀者多跳一層。鏈回到一行 `find_map`，
而 ① 的修法**完全不受影響** —— `rightmost_x_forwarded_for` 仍然是「最後一格，解不開就 `None`」，
改掉的只是 `None` 之後往哪裡去。

| | 之前（8.1）| 現在（8.2）|
|---|---|---|
| 指名一個 header，值無效 | peer | **peer**（不變）|
| 鏈，第一格無效、後面有有效的 | peer | **用後面那個** |
| 鏈，全部無效或都沒設 | peer | **peer**（不變）|
| XFF `8.8.8.8, garbage` | peer | **peer**（不變 —— 🚫 永遠不往左退）|

⭐ **最後一列是重點**：這兩版語意都**不影響 §2 那個 bug 修**，因為「往下走」是往**後面的 header**走，
🚫 不是往 XFF 串的**左邊**走 —— 左邊離我們更遠，而夠左邊就是 client 自己。

測試：`an_unusable_earlier_link_hands_over_to_the_next_one`（四種無效值都跳到 `X-Client-IP`）、
`every_link_unusable_leaves_the_peer`（🔴 四格全無效 ⇒ peer，而且**不是** client 寫在 XFF 左邊的那個值）、
`an_empty_rightmost_element_names_nobody`。
