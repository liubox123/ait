# ait 使用说明

[English](usage.md) · [中文](usage.zh-CN.md) · [返回 README](../README.zh-CN.md)

这份说明从启动服务开始，用一个两人协作的例子走完日常流程：建仓库、划分职责、提交、越权改动的评审、分支与合并请求，再介绍 agent 接入、校验与 AI 审查、系统 Git 客户端和常见报错。文中的输出都来自实际运行，提交编号会因环境不同而变化。

## 目录

1. [基本概念](#基本概念)
2. [安装与启动服务](#安装与启动服务)
3. [账号](#账号)
4. [建仓库与划分职责](#建仓库与划分职责)
5. [日常开发](#日常开发)
6. [越权改动与推送评审](#越权改动与推送评审)
7. [分支与合并请求](#分支与合并请求)
8. [让 agent 接入](#让-agent-接入)
9. [成员、限制与权限策略](#成员限制与权限策略)
10. [校验与 AI 审查](#校验与-ai-审查)
11. [用系统 Git 访问](#用系统-git-访问)
12. [撤销与改写速查](#撤销与改写速查)
13. [环境变量](#环境变量)
14. [常见报错](#常见报错)

## 基本概念

- **仓库所有者。** 创建仓库的人是所有者，可以修改整个仓库、改规划、管受保护分支和标签。服务端第一个注册的用户是管理员，对所有仓库都有所有者权限。
- **规划与职责。** 一份规划把目录分给人和 agent，例如“bob 负责 `src/catalog/**`”。应用规划后，每人得到一组写权限。
- **权限清单。** 克隆、拉取、推送时，服务端把当前用户的权限清单发到本地，保存在 `.git/ait/manifest.json`。`ait perm` 可以随时查看。
- **三种访问级别。** `write` 可以直接推送；没有写权限的路径推送时进入评审（`review`）；`deny` 会拒绝整次推送。
- **推送评审。** 直接推到分支、但改了写权限之外的路径时，分支不会移动，服务端生成一条评审（编号 `r-…`），由对这些路径有写权限的人通过或拒绝。
- **合并请求。** 在自己的分支上开发，推上去后用 `ait pr create` 请求合并（编号 `pr-…`）。
- **本地不设限。** 任何文件在本地都能改、能提交，边界只在推送到服务端时生效。

## 安装与启动服务

安装 Rust 后从源码安装：

```bash
cargo install --path .
```

也可以从 [Releases](https://github.com/liubox123/ait/releases) 下载各系统的可执行文件。

启动服务：

```bash
ait serve --bind 127.0.0.1:7878 --data ./ait-data
```

- `--bind` 是监听地址，默认 `127.0.0.1:7878`。要给局域网使用，改成 `0.0.0.0:7878`。
- `--data` 是数据目录，默认 `./ait-data`，里面是各仓库的裸 Git 仓库、账号和令牌。
- 浏览器打开 `http://127.0.0.1:7878` 是自带的管理页面。

用 Docker 运行：

```bash
docker compose up -d --build
```

数据放在名为 `ait-data` 的卷里。管理员注册好之后，在 `docker-compose.yml` 里把 `AIT_ALLOW_REGISTER` 设为 `false`，再启动一次容器。

服务放在反向代理后面时，设置 `AIT_PUBLIC_URL=https://git.example.com`，克隆地址才会写对。

## 账号

```bash
export AIT_PASSWORD=secret
ait register --server http://127.0.0.1:7878 --user alice
```

```text
已注册 alice（首位用户，管理员）
```

- 第一个注册的用户是管理员。之后其他人用同样的命令注册。
- 已有账号用 `ait login --server … --user …` 登录。
- 密码从 `AIT_PASSWORD` 读取，没有设置时在终端里输入。口令以 Argon2id 保存。
- 登录令牌保存在 `~/.ait/credentials.json`，按服务地址分开。设置 `AIT_HOME` 可以换目录，同一台机器上的多个身份（比如人和 agent）就靠它区分。
- 管理员建好后，用 `AIT_ALLOW_REGISTER=false` 启动服务，就不再接受新注册。

提交时的作者：

```bash
ait config user.name "Alice"
ait config user.email alice@example.com
```

也可以用环境变量 `AIT_AUTHOR_NAME`、`AIT_AUTHOR_EMAIL`。

令牌与密码：

- 令牌默认 30 天过期，过期后命令会提示“登录已过期，请重新登录”，重新 `ait login` 即可。服务端用 `AIT_TOKEN_TTL_DAYS` 调整，`0` 表示不过期。
- 修改密码：`POST /api/password`，正文 `{ "old_password", "new_password" }`。改完会注销这个用户的全部会话，返回新令牌。
- 注销全部会话：`POST /api/logout-all`。令牌泄露时用它。

## 建仓库与划分职责

alice 建仓库并克隆：

```bash
ait repo create shop -d "示例商店"
ait repo list
ait clone http://127.0.0.1:7878/r/shop
```

```text
已创建仓库 shop，所有者 alice
克隆地址 http://127.0.0.1:7878/r/shop
已克隆空仓库到 shop
权限清单（alice）
职责: 仓库所有者
可直接修改:
  **
```

放入初始代码并推送：

```bash
cd shop
ait add
ait commit -m "初始化"
ait push
```

```text
[main 72ab6c7] 初始化
 * [新引用]          main -> main  已更新分支
```

写一份规划 `plan.md`：

```text
## 账户 (@alice)
职责: 登录、注册与会话
目录: src/account/**

## 商品 (@bob)
职责: 商品与库存
目录: src/catalog/**
```

应用它（只有仓库所有者可以）：

```bash
ait plan apply --file plan.md
```

```text
已根据规划文本更新权限
- alice：账户；登录、注册与会话
    src/account/**
- bob：商品；商品与库存
    src/catalog/**
```

`ait plan show` 显示服务端保存的规划。规划还支持两种写法：

```text
@bob src/catalog/** 商品与库存
```

```json
{"assignments":[{"user":"bob","duty":"商品与库存","paths":["src/catalog/**"]}]}
```

规划写成自然语言时，加 `--llm` 让模型解析：`ait plan apply --file plan.md --llm`。服务进程需要配置 `AIT_LLM_BASE_URL`、`AIT_LLM_API_KEY`，可选 `AIT_LLM_MODEL`（默认 `gpt-4o-mini`）。

路径写法：`src/catalog/**` 表示这个目录下的全部文件；`*` 匹配一层目录名或文件名中的任意字符，`?` 匹配一个字符；`**` 单独使用表示整个仓库。

## 日常开发

bob 注册、克隆，先看自己的权限：

```bash
ait register --server http://127.0.0.1:7878 --user bob
ait clone http://127.0.0.1:7878/r/shop
cd shop
ait perm
```

```text
权限清单（bob）
职责: 商品；商品与库存
可直接修改:
  src/catalog/**
其他路径可以在本地修改，推送时需要有权限的人评审。
```

在职责内修改，直接推送：

```bash
ait commit -a -m "商品改价"
ait push
```

```text
[main 4e270e1] 商品改价
   72ab6c7..4e270e1  main -> main  已更新分支
```

常用命令和 Git 一致：

```bash
ait status            # 状态，还会列出越权文件
ait diff              # 工作区对暂存区
ait diff --cached     # 暂存区对 HEAD
ait add src/catalog   # 加入暂存区；不带路径时加入全部更改
ait restore --staged src/catalog/items.rs   # 撤出暂存区
ait commit -m "说明"
ait log --oneline --graph -n 20
ait pull              # 拉取并合并，同时更新权限清单
ait push
```

`ait status` 会提前告诉你哪些文件推送时要评审：

```text
位于分支 main
与 origin/main 一致

尚未暂存的更改:
  修改: src/account/login.rs

这些路径不在你的写权限内，推送时会进入评审:
  src/account/login.rs
```

`pull` 默认合并；`ait pull --rebase` 改为变基，`ait pull --ff-only` 只接受快进。远端有新提交而本地没拉取时，`ait push` 会拒绝并提示“不是快进”，先 `ait pull` 再推。

## 越权改动与推送评审

bob 顺手改了账户模块的文件，提交后推送：

```bash
ait commit -a -m "顺手改登录"
ait push
```

```text
 ? [待评审]          main -> main (修改了写权限之外的路径，已提交评审 r-40641f08)
  需要评审的路径:
    src/account/login.rs
```

`main` 没有移动。alice（对 `src/account/**` 有写权限）查看并处理：

```bash
ait review list
```

```text
r-40641f08	pending	bob	refs/heads/main
  越权路径: src/account/login.rs
```

通过后分支更新：

```bash
ait review approve r-40641f08
```

```text
已通过 r-40641f08，分支已更新
```

或者拒绝并说明原因：

```bash
ait review reject r-40641f08 --reason "登录归账户模块"
```

被拒绝时，bob 本地的提交还在，要自己丢掉，否则下次推送还会带上它：

```bash
ait reset --hard origin/main
```

被通过时，bob 执行 `ait pull` 即可与远端一致。

禁止修改（`deny`）的路径不会生成评审，整次推送直接失败。

## 分支与合并请求

改动较大或跨模块时，在自己的分支上开发：

```bash
ait switch -c bob/stock
# 修改文件……
ait add
ait commit -m "库存与登录调整"
ait push -u origin bob/stock
ait pr create "库存功能"
```

```text
已创建合并请求 pr-9e94409f 库存功能
bob/stock -> main
```

`--base` 指定目标分支，默认 `main`。`-u` 记住上游，之后在这个分支上直接 `ait push`、`ait pull`。

```bash
ait pr list
```

```text
pr-9e94409f	open	bob	bob/stock -> main	库存功能
  需要评审: src/account/login.rs
```

谁能合并：

- 改动都在作者职责内时，作者可以自己 `ait pr merge`。
- 含有越权路径时，要先由对这些路径有写权限的人 `ait pr approve`，然后由**通过的人或仓库所有者**合并。作者此时执行 `pr merge` 会得到“需要有写权限的人先通过，才能合并到目标分支”。
- 配置了必过校验时，校验通过前不能合并。
- 目标分支已经改过同一文件时，合并会报冲突，先在分支上 `ait merge main` 解决再推。

```bash
ait pr approve pr-9e94409f   # alice
ait pr merge pr-9e94409f     # alice
ait pr close pr-9e94409f     # 不要了就关闭
```

分支规则：

- 不能推送到别人建的分支。
- 删除远端分支：`ait push origin --delete bob/stock`。建分支的人或仓库所有者可以删。
- 受保护分支（默认 `main`、`master`）不能删除。所有者用 `GET/PUT /api/repos/:name/protected`，正文 `{ "patterns": ["main", "release/*"] }` 修改。
- 标签只有所有者能推送、移动和删除：`ait tag -a v1.0 -m "说明"`，`ait push --tags`。移动已有标签要加 `--force`。

## 让 agent 接入

推荐做法：每个 agent 用独立账号和独立的 `AIT_HOME`，这样提交、评审和动态都能分清是谁。

```bash
AIT_HOME=~/.ait-agent-api AIT_PASSWORD=… ait register --server http://127.0.0.1:7878 --user api-agent
```

在仓库目录启动 MCP 服务：`ait mcp`（或 `ait mcp --repo 路径`）。Cursor 的配置示例：

```json
{
  "mcpServers": {
    "ait": {
      "command": "ait",
      "args": ["mcp"],
      "env": { "AIT_HOME": "/Users/me/.ait-agent-api" }
    }
  }
}
```

工具一览：

| 工具 | 用途 |
| --- | --- |
| `permissions`、`check_paths`、`owners` | 写之前查询权限和路径负责人 |
| `status`、`diff`、`log` | 查看状态和历史 |
| `commit` | 只提交写权限内的文件，含越权文件时失败 |
| `restore` | 把文件撤出暂存区 |
| `propose` | 提交并推送越权修改，生成评审 |
| `push`、`fetch`、`pull`、`branch`、`checkout`、`merge` | 同步与分支 |
| `pr`、`review` | 合并请求与评审 |
| `plan` | 查看规划 |
| `claim` | 登记、查看、结束工作意图 |
| `checks` | 查看、上报或请求执行校验 |
| `ai_review` | 请 AI 审查者看合并请求或评审 |

建议在 agent 的规则里写明这套流程：

1. 动手前调用 `check_paths` 和 `owners`，确认要改的路径归谁。
2. 较大的改动先 `claim`（`action: "start"`），看返回里有没有和别人重叠。
3. 写完用 `commit` 提交职责内的改动；越权的部分用 `propose`，不要绕过。
4. 跑完测试用 `checks` 上报结果（agent 的上报只作参考，不算必过校验通过）。
5. 开合并请求后用 `ai_review` 请审查者过一遍，完成后 `claim` 的 `finish`。

## 成员、限制与权限策略

规划之外，权限策略还可以设置成员类型和限制。所有者用 `PUT /api/repos/:name/policy` 保存，或者在 [ait-web](https://github.com/liubox123/ait-web) 的规划页里编辑：

```json
{
  "rules": [
    { "user": "bob", "pattern": "src/api/**", "access": "write" },
    { "user": "bob", "pattern": "migrations/**", "access": "deny" }
  ],
  "duties": [{ "user": "bob", "summary": "后端接口 agent", "paths": ["src/api/**"] }],
  "members": [
    { "user": "bob", "kind": "agent", "supervisor": "alice",
      "limits": { "max_files": 10, "max_lines": 400, "required_checks": ["test"], "human_review": false } },
    { "user": "ci-bot", "kind": "ci" }
  ]
}
```

- `access` 取 `write`、`review` 或 `deny`。
- `kind` 是 `human`（默认）、`agent` 或 `ci`；agent 可以指定 `supervisor`。
- `limits` 中 `max_files`、`max_lines` 限制单次改动规模，`required_checks` 追加必过校验，`human_review` 为 `true` 时即使在写权限内也要另一个人通过。超出限制的改动照样能提交，由别人通过后落地。

保存前可以先预览：`POST /api/repos/:name/policy/preview` 接受同样的正文，返回每个目录谁能写、谁被禁止、谁负责，以及每个成员的权限清单和问题提示。

## 校验与 AI 审查

在仓库里声明校验 `.ait/checks.toml`：

```toml
[[check]]
name = "test"
run = "cargo test"
image = "rust:1"
required = true
paths = ["src/**", "tests/**"]   # 只在改动命中这些路径时适用
runner = "any"                   # report、server 或 any
timeout = 600
```

- 结果来源：CI 用 `ci` 账号调用 `POST /api/repos/:name/checks` 上报；或者服务端在 Docker 沙箱里执行（启动服务时设 `AIT_CHECK_RUNNER=docker`，用 `AIT_CHECK_IMAGES` 限定镜像白名单）。
- 只有服务端执行器、仓库所有者和 `ci` 账号的结果能满足必过校验。
- 必过校验没通过时，合并请求不能合并，推送评审不能通过。

AI 审查者：

- 不配置模型时也会运行内置规则：越权、改动过大、疑似密钥、冲突标记、删除测试、调试输出、与别人的工作意图重叠。
- 仓库所有者用 `PUT /api/repos/:name/ai` 配置 OpenAI 兼容模型 `{ enabled, auto, base_url, model, api_key, instructions }`。开启 `auto` 后，新的合并请求和推送评审会自动审查。
- 审查只给意见，最后由有写权限的人决定。

接口细节见 README 的[校验](../README.zh-CN.md#校验)、[AI 审查者](../README.zh-CN.md#ai-审查者)和[协作](../README.zh-CN.md#协作)。

## 用系统 Git 访问

服务端支持 Git smart HTTP，不装 ait 也能协作：

```bash
git clone http://bob@127.0.0.1:7878/r/shop.git
git push origin main
git push origin --delete old-branch
```

用户名填 ait 账号，密码填登录密码或 ait 令牌。权限、受保护分支和评审规则与 `ait push` 相同；越权改动被转为评审时，`git push` 显示 `remote rejected` 和评审编号。用系统 Git 时本地拿不到权限清单，推送前可以在管理页面查看自己的职责和目录。

ait 管理的本地仓库也是标准 Git 仓库，`git log`、`git diff`、IDE 的 Git 面板都能直接用。

## 撤销与改写速查

| 想做的事 | 命令 |
| --- | --- |
| 丢掉工作区对某文件的修改 | `ait restore 文件` |
| 撤出暂存区 | `ait restore --staged 文件` |
| 修改上一个提交（未推送） | `ait commit --amend -m "新说明"` |
| 撤销已推送的提交 | `ait revert 提交` |
| 回到某个提交并丢弃之后的改动 | `ait reset --hard 提交` |
| 找回误删的提交 | `ait reflog`，再 `ait reset --hard HEAD@{n}` |
| 临时收起改动 | `ait stash`，之后 `ait stash pop` |
| 把别的分支的某个提交拿过来 | `ait cherry-pick 提交` |
| 整理本分支的提交 | `ait rebase -i main` |
| 合并冲突后放弃 | `ait merge --abort` 或 `ait rebase --abort` |
| 从某个版本取回文件 | `ait checkout 版本 -- 文件` |

切换分支时，本地改动会和 Git 一样带过去；只有改动所在的文件在两个分支间不同时才会拒绝。

## 环境变量

| 变量 | 作用于 | 说明 |
| --- | --- | --- |
| `AIT_PASSWORD` | 客户端 | 注册、登录时的密码 |
| `AIT_HOME` | 客户端 | 凭据目录，默认 `~/.ait` |
| `AIT_AUTHOR_NAME`、`AIT_AUTHOR_EMAIL` | 客户端 | 提交作者 |
| `AIT_ALLOW_REGISTER` | 服务端 | 设为 `false` 关闭注册 |
| `AIT_TOKEN_TTL_DAYS` | 服务端 | 令牌有效天数，默认 30，`0` 不过期 |
| `AIT_PUBLIC_URL` | 服务端 | 对外地址，用于生成克隆链接 |
| `AIT_WEB_DIR` | 服务端 | ait-web 构建产物目录，页面挂在 `/ui/` |
| `AIT_LLM_BASE_URL`、`AIT_LLM_API_KEY`、`AIT_LLM_MODEL` | 服务端 | 规划解析和 AI 审查的默认模型 |
| `AIT_CHECK_RUNNER` | 服务端 | 设为 `docker` 启用沙箱校验 |
| `AIT_CHECK_IMAGES`、`AIT_CHECK_IMAGE`、`AIT_DOCKER` | 服务端 | 镜像白名单、默认镜像、docker 命令 |

## 常见报错

| 报错 | 原因与处理 |
| --- | --- |
| `Address already in use` | 端口被占用，换 `--bind` 的端口，或停掉已有的服务 |
| `还没有登录` / `请先 ait login 登录这个服务` | 当前 `AIT_HOME` 下没有这个服务的令牌，执行 `ait login` |
| `登录已过期，请重新登录` | 令牌超过有效期，重新 `ait login` |
| `用户已存在` | 换个用户名，或者用 `ait login` |
| `不是快进，请先 ait pull 合并远端改动，或使用 --force` | 远端有你没拉取的提交，先 `ait pull` |
| `修改了写权限之外的路径，已提交评审 r-…` | 不是错误，等有权限的人处理评审 |
| `这些路径禁止修改` | 改动碰到了 `deny` 路径，把这些改动撤掉再推 |
| `需要有写权限的人先通过，才能合并到目标分支` | 合并请求含越权路径：先由有权限的人通过，再由通过者或所有者合并 |
| `必过校验还没有通过` | 等校验通过，或让 CI / 服务端执行器补上结果 |
| `您对下列文件的本地修改将被检出操作覆盖` | 切换分支前先提交或 `ait stash` |
| `不在 Git 仓库中` | 进入仓库目录，或给 `ait mcp --repo` 指定路径 |
