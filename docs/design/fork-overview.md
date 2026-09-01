# 這個 fork 是什麼

> 這份文件回答一個問題：**這個 repo 跟上游 tuwunel 是什麼關係，改動要怎麼進來。**
> 程式碼結構看 [repo-structure.md](repo-structure.md)，設計方向看
> [why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md)。

## 來源與去處

| | |
|---|---|
| 上游 | [`matrix-construct/tuwunel`](https://github.com/matrix-construct/tuwunel)（Apache-2.0） |
| 這個專案 | **wbfuwunel** |
| 開發用 | `amaid/wbfuwunel`，維護者自架的 Forgejo。PR 在這裡開 |
| 對外 | [`WhiteBirchForumTeam/wbfuwunel`](https://github.com/WhiteBirchForumTeam/wbfuwunel)（GitHub，**公開**） |
| 目的 | 見 [why-not-matrix-and-core-design.md](why-not-matrix-and-core-design.md)：把媒體層改成分塊 + Merkle + 引用計數，拿到續傳、串流播放與真正的刪除 |

### 授權：這是公開重新發佈，有義務要盡

因為對外那份是**公開**的，Apache-2.0 的義務就是實際生效的，不是形式：

- 保留上游的 `LICENSE`、著作權標示與作者名單 —— **一個字都不刪**。
- **聲明本專案已對上游做過修改**（README 最上面那段就是）。改了什麼，用
  `git log --oneline upstream/main..main` 列得出來。
- 🚫 **不冒充上游、不代表上游。** README 明講了問題要回報到這裡，不要去麻煩上游的維護者。

⚠️ 這一段在 2026-09-01 之前寫的是「這個 fork 不對外發佈」—— 那句話當時就已經不成立
（GitHub 上那份是 public），已改正。**文件講反話比沒有文件更糟。**

### 命名：專案改名，程式碼不改名

專案叫 **wbfuwunel**，但 **crate 名（`tuwunel-*`）、binary 名（`tuwunel`）、設定路徑一律維持
上游的名字**。這是維護者 2026-09-01 的決定。

理由是成本不對稱：改名的收益只是好看，代價卻是**跟上游的每一次 merge 都在幾千行 `use`
語句上衝突，而且是永久的**。這個 fork 的價值來自持續跟上游同步，不值得為了名字把那條路弄難走。

👉 **所以看到 `tuwunel` 出現在程式碼、路徑或 binary 名裡，那是正確的，不是漏改的。**

## 這些文件不進 `docs/SUMMARY.md`

`docs/SUMMARY.md` 是上游 mdBook 的目錄，會被上游頻繁修改。fork 專屬的文件**刻意不加進去**，
理由有兩個：一是每加一行就多一個跟上游衝突的點；二是 `README.md` 已經有一份索引，兩份索引
遲早會漂移，而漂移的那天不會有人通知你。

👉 **新增 fork 文件時，只更新 `README.md` 的那張表。** 這個目錄裡的文件之間再互相連結即可。

## 分支模型

| 分支 | 是什麼 | 誰動它 |
|---|---|---|
| `main` | **這個 fork 自己的線**，上游 + 本地改動 | 只透過 PR 合併 |
| `upstream/main` | 遠端追蹤 ref，指向上游最新 | `git fetch upstream` 更新 |
| `upstream-main` | 上游的本地鏡像分支，只快轉 | `git fetch upstream && git branch -f upstream-main upstream/main` |

`main` **維持原名**，沒有改成 `custom/main` 之類 —— 維護者決定的，理由是上游那條線已經有
`upstream/` 前綴可以區分，再改名只會讓既有的 remote 設定與 CI 全部要跟著動。

⚠️ **本地鏡像分支不要取名 `upstream/main`。** git 解析 ref 時 `refs/heads/` 的優先權高於
`refs/remotes/`，所以同名的本地分支會**靜默蓋掉**遠端追蹤 ref（只給一行
`warning: refname is ambiguous`），此後每一次 `git log upstream/main` /
`git merge upstream/main` 講話的對象都是那支可能過期的本地分支。用連字號的
`upstream-main` 就沒有這個問題。

看「我改了什麼」：

```sh
git log --oneline upstream/main..main
```

## 跟上游同步：只 merge，不 rebase

```sh
git fetch upstream
git merge upstream/main        # 在 main 上
```

🚫 **不要 rebase、不要 `--amend`、不要 force push**，PR 也不要選 squash 或 rebase merge。
歷史是紀錄不是草稿 —— 一份難看但真實的歷史，永遠贏過一份漂亮但被改過的。要修就再 commit
一次，`fix: 上一個 commit 漏了 X` 本身就是有用的紀錄。

## 改動的流程（維護者指定）

```
① 先寫文件 / 方案 / 討論
        ↓
② 維護者同意
        ↓
③ 開分支
        ↓
④ 開 PR，目標分支 main
```

**順序不能顛倒。** ①「先寫」指的是把要做什麼、為什麼這樣做、有哪些取捨寫成文件放進這個
`docs/design/` 目錄，不是寫在聊天記錄裡 —— 下一個讀的人手上只有 repo。

②之前不要動 `src/`。方案被推翻的成本，在文件階段是改幾段字，在程式碼階段是整支分支重來。

## 誰在改

commit 會出現兩種身分，兩種都是真的：

| author | 是誰 |
|---|---|
| `Weil Jimmer <me@weils.net>` | 維護者本人，commit 有 GPG 簽章 |
| `claude <claude@weils.net>` | AI 助手，commit **不簽章**（它沒有維護者的金鑰，也不該有） |

工作目錄是共用的，所以身分是**依 remote 分流**的（`.git/config` 裡的 repo-local 設定），
不是靠誰記得下對參數。
