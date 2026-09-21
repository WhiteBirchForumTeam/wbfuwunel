# `0x19 Media`：相容的媒體路徑

> 號碼總表與共通規則在 [index.md](index.md)。這份只寫**每支端點帶什麼、回什麼、會怎麼被拒**。
> 請求的前 4 bytes 一律是 `01 19 SS 10`；成功回覆 `01 01 02 14`、失敗 `01 01 03 14`。`id` 填 0。

🚨 **這個 kind 不是 fork 的媒體通道。** 分塊上傳／下載是原生的 `0x03 Upload`／`0x04 Download`（[chunked-upload-spec.md](../design/chunked-upload-spec.md)），
那才是檔案真正走的路。`0x19` 只放兩支「關於媒體的問題」，跟傳檔案本身無關。

📎 批 4 開的 kind，**沒有原生的 subtype**，所以 `0x19` 不帶 `IS_BRIDGED`（bit4）一律是 `UnknownKind`。
📎 兩支走的都是**認證媒體**的路徑（`/_matrix/client/v1/media/…`，MSC3916 進規格 1.11），**不是**已經棄用的 `/_matrix/media/v3/…`。

## `0x20` MediaConfig —— 這台 server 的上傳上限

`GET /_matrix/client/v1/media/config`

| | |
|---|---|
| 請求 meta | 空的（長度 0）或 `{}` |
| 請求 data | — |
| 回覆 data | `{"m.upload.size":50000000}` |

⚠️ **這是舊的整檔上傳的上限，不是分塊上傳的。** 分塊那條路的限制是 `Hello` 回覆裡的 `chunk_size_default`／`chunk_size_large`／`data_max_bytes`，跟這支是兩套數字。要傳大檔請看 `Hello`，不要看這裡。
📎 欄位缺席代表「沒有限制」，不是 0。

## `0x21` MediaPreview —— 一個連結的預覽

`GET /_matrix/client/v1/media/preview_url`

| | |
|---|---|
| 請求 meta | `{"url":"https://example.org/article"}`；可加 `"ts"`（毫秒，「想要哪個時間點的預覽」） |
| 請求 data | — |
| 回覆 data | `{"og:title":"…","og:description":"…","og:image":"mxc://…","matrix:image:size":12345}`；沒抓到東西是 `{}` |

⚠️ **這台 server 可以整個關掉預覽**，也可以用允許清單限制能預覽哪些網址 —— 關著的時候是 `Forbidden`（403），跟 HTTP 一樣。client 不要把 403 當成「這個連結壞了」。
📎 是 **server 去抓那個網址**，不是 client。所以它看得到 server 的出口 IP，也受 server 的網路政策限制。
📎 **`ts` 這台 server 收下但忽略**（沒有歷史版本的預覽）。橋還是宣告了它 —— **因為 HTTP 收得下它**，而橋要在 HTTP 拒絕的地方才拒絕；漏宣告的話，帶 `ts` 的 client 走橋會拿到橋自己的 `InvalidRequest`（1201），走 HTTP 卻正常 —— 兩條路就不一致了（PR #81 審查，cirno）。

## 這個 kind 共通的拒絕

| 情況 | `code` | 來自 |
|---|---|---|
| meta 少了 `url`、或多了表上沒有的變數 | `InvalidRequest`（1201） | 橋 |
| `0x19` 的 pack 沒帶 bit4，或 subtype 不在表上 | `UnknownKind`（1101） | 橋 |
| 預覽被關掉、或網址不在允許清單 | `Forbidden`（403） | 端點 |
| 沒登入 | `Unauthorized`（1301） | 端點 |
| 問太快 | `RateLimited`（429 `M_LIMIT_EXCEEDED`） | 端點 |
