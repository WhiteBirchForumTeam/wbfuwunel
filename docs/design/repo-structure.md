# 程式碼結構導覽

> ⚠️ **這是導覽，不是權威。** 權威永遠是程式碼本身。這份文件的用途是讓人**知道該去哪個
> 檔案看**，不是取代看那個檔案。行數與數字是 2026-09-01 在 `v1.9.0-91` 量的，會過期；
> 結論性的分層關係比數字耐用。

## 一句話版本

八個 crate，單向分層，沒有環：`macros → core → database → service → {api, admin} → router → main`。

## crate 分層

```
                    main        ← 二進位進入點、參數、執行期、訊號、健康檢查
                      │
                   router       ← 監聽、HTTP 中介層、啟動／停止的生命週期
                   ╱    ╲
                api      admin  ← Matrix HTTP 端點 ／ 管理員指令
                   ╲    ╱
                   service      ← 業務邏輯（房間、媒體、同步、聯邦……）
                      │
                   database     ← RocksDB 引擎、column family、序列化
                      │
                    core        ← 設定、錯誤、日誌、Matrix 型別、工具
                      │
                   macros       ← 程序巨集
```

| crate | 規模 | 負責回答 |
|---|---|---|
| `macros` | 10 檔 / 1.2k 行 | 程序巨集（`implement`、指令派發） |
| `core` | 168 檔 / 29k 行 | 設定、錯誤、日誌、Matrix 型別、平台工具 |
| `database` | 63 檔 / 11k 行 | RocksDB 怎麼開、怎麼讀寫、怎麼序列化 |
| `service` | 266 檔 / **76k 行** | 業務邏輯。**最大的一塊，改動多半落在這裡** |
| `api` | 302 檔 / 40k 行 | HTTP 端點：`client` / `server`（聯邦）/ `oidc` |
| `admin` | 285 檔 / 12k 行 | `!admin` 指令 |
| `router` | 12 檔 / 1.6k 行 | 綁定、中介層、啟動與停止 |
| `main` | 61 檔 / 14k 行 | `fn main`、CLI、執行期、訊號 |

總計約 1167 個 `.rs`、18.5 萬行。

## 執行的路徑

`src/main/main.rs` 短得可以整段讀完，順序是：

```
args::parse()  →  config::run()（--generate-config 這類「做完就結束」的模式在這裡攔截）
               →  health::check()（--health-check）
               →  Runtime::new()  →  Server::new()  →  tuwunel::exec()
```

`exec` 之後進 `src/router/run.rs`：`start()` 建起所有 service → `run()` 進主迴圈 →
`stop()` 收尾。監聽在 `src/router/serve.rs`。

## 要改東西的時候去哪裡找

| 想動的東西 | 去哪 |
|---|---|
| 新增／修改 Matrix client 端點 | `src/api/client/` |
| 聯邦端點 | `src/api/server/` |
| 媒體（上傳、下載、縮圖、遠端抓取） | `src/service/media/`（`mod.rs` / `data.rs` / `remote.rs` / `thumbnail.rs`） |
| 媒體的實體儲存後端 | `src/service/storage/` —— 已經有 `object_store` 的 provider 抽象 |
| 房間相關的一切 | `src/service/rooms/`（`timeline`、`state`、`event_handler`、`retention`…） |
| 同步 | `src/service/sync/` 與 `src/api/client/sync/` |
| **新的 column family** | `src/database/maps.rs` —— 目錄宣告 138 個，實跑開 133 個（dropped 的不開） |
| 設定項 | `src/core/config/mod.rs`。⚠️ 這個檔同時是**設定文件的來源** —— `tuwunel-example.toml` 由它產生 |
| 管理員指令 | `src/admin/` |
| HTTP 中介層（壓縮、追蹤、限流） | `src/router/layers/` |

## 對設計文件的意義

[why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md) 規劃的媒體層改造
（分塊 + Merkle + 引用計數）會落在三個地方，而它們彼此有清楚的接縫：

1. **`src/api/client/media.rs`** —— 新的分塊上傳／range 下載端點。
2. **`src/service/media/`** —— 分塊、Merkle 樹重算、引用計數的邏輯。
3. **`src/database/maps.rs` + `src/service/storage/`** —— 塊的索引與實際 bytes 的落點。

⭐ 值得注意的是 `src/service/storage/` **已經有一層 provider 抽象**（走 `object_store`），
所以「bytes 存哪裡」這件事不必從零長出來。現有的 media column family 也已經有
`mediaid_pending` 這種暫存概念可以參考。

## 平台

上游主力是 Linux。Windows 能編、能跑，但少掉一部分功能 ——
見 [windows-build.md](windows-build.md)。
