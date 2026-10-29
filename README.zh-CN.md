# ait

[English](README.md) · [中文](README.zh-CN.md)

面向 AI 时代的 Git。本地仓库仍是标准 Git 对象、索引和引用，系统自带的 `git` 可以直接阅读。人和多个 agent 一起改代码时，服务端按项目规划决定谁能直接修改哪些目录。职责之内的改动进入分支；跨出职责的改动可以写在本地，但要经过评审或合并请求，才会进入目标分支。

AI 时代的开发和过去不一样：同一个人会在不同环境里驱动不同的 agent，短时间产生大量代码；需求和实现都在变，提示词也拦不住意料之外的编辑；多个 agent 各写各的，目录和结构会对不上。ait 把「谁负责哪一块」变成仓库规则，让生成、修改和合并都经过这条边界。

这些规则还很粗。欢迎在 [Discussions](https://github.com/liubox123/ait/discussions) 里讨论：权限该分到目录还是到符号，评审该由人做还是由模型做，agent 之间怎样对齐同一套结构。Issue 用来报具体问题。

## 为什么要做 ait

### 用 AI 写代码的痛苦

- **改得比看得快。** agent 几分钟能改几十个文件，人要逐行读完才敢合并。评审成了瓶颈，最后常常只看测试过没过。
- **改动越界。** 让它修一个接口，它顺手重命名了公共函数、改了配置、升了依赖。提示词里写了“只改这个目录”，它也不一定照做。
- **悄悄绕过问题。** 测试不过时删测试、加 `skip`、放宽断言；留下调试输出和冲突标记，偶尔把密钥写进代码。
- **上下文会丢。** 换一个会话或工具，agent 就忘了之前的约定。同一个问题被不同方式解决好几遍，结构越来越乱。
- **难以追溯。** 一次提交里混着人和 agent 的改动，事后说不清哪行是谁让改的、为什么改。
- **回退代价高。** 大块生成的改动缠在一起，想撤掉其中一部分，只能整体回退或者手工挑拣。

### AI 编码协作的痛苦

- **同时改同一处。** 几个 agent 并行干活，互相不知道对方在改什么。合并时才发现冲突；更糟的是合并成功了，逻辑却互相打架。
- **结构对不上。** 每个 agent 按自己的理解建目录、起名字、选写法，拼起来像几个不同的项目。
- **职责只写在文档里。** 谁负责哪块只存在于文档或聊天记录中，agent 看不到，Git 也不管：有推送权限就能改任何文件。
- **权限太粗。** 托管平台通常按仓库或分支授权，没法表达“这个 agent 只能改 `src/api/`”。为 agent 单独开仓库，又把代码拆散了。
- **评审压在少数人身上。** agent 的产出远超人的评审能力，跨模块的改动也不清楚该找谁看。
- **agent 自报的结果不可信。** agent 说“测试已通过”，没人能确认它真的跑过，跑的又是哪套测试。
- **分不清是哪个 agent。** 同一个人在编辑器、命令行和 CI 里各开一个 agent，共用一个账号，出了问题分不清是谁干的。

### ait 的做法

- **职责写进仓库规则。** 一份项目规划划分目录和职责，所有人和 agent 拿到同一份权限清单。agent 写之前可以通过 MCP 查询自己能改哪里。越权的改动不会直接进分支，而是变成评审或合并请求，由对那些路径有写权限的人决定。
- **限制单次改动的规模。** 成员可以设置单次最多改多少文件和行数，超出的改动要另一个人通过才能落地。
- **先让机器过一遍。** AI 审查者检查越权、改动过大、疑似密钥、冲突标记、删除测试和调试输出，帮人把注意力放在真正要判断的地方。
- **校验结果只认可信来源。** 必过校验只采信服务端沙箱、仓库所有者和 `ci` 账号给出的结果，agent 自己的上报只作参考。
- **开工前登记意图。** 人和 agent 登记要改的路径，范围重叠时提示双方，不加锁，不挡路。
- **每个 agent 一个身份。** agent 是独立的成员账号，可以指定监督人，提交、评审和动态都记在具体账号下。

上下文丢失、大块改动难以回退这类问题，ait 还解决不了，仍然要靠工作习惯和其他工具配合。

## 功能

- **Git 日常命令。** `init`、`add`、`status`、`diff`、`commit`、`log`、`branch`、`checkout`、`merge`、`push`、`pull`。对象格式与 Git 互通，`git fsck` 和 `git gc` 之后仍能读 pack。
- **目录写权限。** 克隆和拉取时收到修改权限清单，保存在 `.git/ait/manifest.json`。`write` 可以直接推送，没有写权限的路径进入评审，`deny` 会拒绝整次推送。所有者始终可以修改整个仓库。
- **项目规划。** 用一份规划划分用户、职责和目录。可以按标题解析，也可以让模型解析。
- **评审。** 直接推到 `main` 时，越权修改不会移动分支，而是生成一条评审，由对该路径有写权限的人通过或拒绝。
- **合并请求。** 其他分支可以先推上去，再用 `ait pr create` 请求合并。职责内的改动作者可以自己合并；越权路径要先由有写权限的人通过。不能推送到别人的分支。目标分支已经改过同一文件时，合并会报冲突。
- **MCP。** agent 在仓库目录运行 `ait mcp`，写之前可以查询职责和路径权限。`commit` 只接受写权限内的文件，跨目录修改用 `propose`。
- **成员与限制。** 每个成员是 `human`（开发者）、`agent` 或 `ci` 账号，agent 可以指定监督人。限制包括单次最多改动文件数和行数、额外的必过校验、是否所有改动都要另一个人通过。超出限制的改动照样可以提交，由别人通过后落地。
- **校验。** 在 `.ait/checks.toml` 里声明校验，结果可以通过接口上报，也可以由服务端在 Docker 沙箱里执行。必过校验会拦住合并和推送评审。只有服务端执行器、仓库所有者和 `ci` 账号给出的结果才算数，agent 的上报留作参考。
- **AI 审查者。** 内置规则会检查越权、改动过大、疑似密钥、冲突标记、删除测试、调试输出，以及和别人的工作意图重叠。仓库配置了 OpenAI 兼容模型时，再附上模型意见。审查者只给意见，由有写权限的人决定。
- **工作意图。** 人和 agent 开工前登记要改的路径。登记不加锁，范围重叠时提示双方，AI 审查者也会提到。
- **管理页面。** `ait serve` 后可以注册、建仓库、浏览代码和提交、处理合并请求与评审、编辑路径规则、粘贴规划。更完整的前端是 [ait-web](https://github.com/liubox123/ait-web)：页面改写自 GitLab，带目录 × 成员的规划页、协作看板，以及校验和 AI 审查面板。
- **多系统发布。** 推送到 `main` 会在 Linux、macOS、Windows 上测试并编译。推送 `v*` 标签时，把各平台可执行文件发布到 GitHub Release。

## 开始

安装 Rust 后：

```bash
cargo install --path .
ait serve --bind 127.0.0.1:7878 --data ./ait-data
```

发布页里也有各系统的可执行文件：[Releases](https://github.com/liubox123/ait/releases)。

另一个终端：

```bash
export AIT_PASSWORD=secret
ait register --server http://127.0.0.1:7878 --user alice
ait repo create shop
ait clone http://127.0.0.1:7878/r/shop
cd shop
ait plan apply --file ../examples/shop.plan.md
```

密码从 `AIT_PASSWORD` 读取，否则从终端读取。提交身份用 `ait config user.name` 和 `ait config user.email`，也可以用 `AIT_AUTHOR_NAME` / `AIT_AUTHOR_EMAIL`。第一个注册的用户是管理员。口令以 Argon2id 保存。管理员建好之后，把 `AIT_ALLOW_REGISTER=false` 设上，就不再接受新的注册。

## Docker

```bash
docker compose up -d --build
```

页面在 `http://127.0.0.1:7878`。仓库数据放在名为 `ait-data` 的卷里。管理员账号建好后，在 `docker-compose.yml` 里把 `AIT_ALLOW_REGISTER` 设为 `false`，再启动一次容器。

## 命令

```bash
ait init
ait add
ait restore --staged src/lib.rs
ait status
ait diff
ait commit -m "说明"
ait log
ait branch
ait checkout -b bob/price
ait merge main
ait pull
ait push
ait perm
ait review list
ait review approve r-1a2b3c4d
ait review reject r-1a2b3c4d --reason "先别改这里"
ait pr list
ait pr create "调整价格"
ait pr approve pr-1a2b3c4d
ait pr merge pr-1a2b3c4d
ait pr close pr-1a2b3c4d
```

### 与 Git 相同的命令

参数和行为按 Git 的习惯实现，仓库仍可直接交给系统 `git` 读写。

| 类别 | 命令 |
| --- | --- |
| 撤销与改写 | `reset --soft/--mixed/--hard`、`revert`、`cherry-pick [-x] [-n]`、`commit --amend`、`stash push/list/show/apply/pop/drop/clear`、`checkout <版本> -- <路径>`、`checkout --detach`、`switch -`、`restore [--staged] [--worktree] [--source <版本>]`、`rm [--cached] [-r]`、`mv`、`clean -n/-f/-d/-x`、`reflog` |
| 变基 | `rebase <上游> [--onto <基点>]`、`rebase -i`（pick/reword/edit/squash/fixup/drop/exec/break，`--todo <文件>` 可直接给出清单）、`--continue/--skip/--abort` |
| 查看 | `rev-parse`（`~n`、`^n`、`^{tree}`、`@{n}`、`@{-n}`、`@{u}`、`版本:路径`、`:/文字`）、`log`（`--oneline --graph --all -n --author --grep --since --until --first-parent --no-merges -p --stat --format`、`A..B`、`A...B`、`-- 路径`）、`show`、`diff A B`、`blame [-L]`、`grep`、`tag [-a] [-d]`、`describe`、`shortlog`、`cat-file`、`ls-files`、`ls-tree`、`merge-base` |
| 远端 | `remote add/remove/rename/set-url/get-url/-v`、`fetch [--all] [--prune]`、`pull [--rebase] [--ff-only]`、`push [-u] [-f] [--tags] [--delete] [<远端>] [<源>:<目标>]`、上游跟踪 `branch -u` |
| 维护 | `gc [--prune=now]`、`pack-refs`、`fsck`、`count-objects -v` |

`pull` 和 `push` 用 pack 传输对象；推送被拒绝时，对象只留在内存隔离区，不写入服务端仓库。

### 系统 Git 客户端

服务端支持 Git smart HTTP（拉取用协议 v2，也兼容 v0；推送用 receive-pack）。权限、受保护分支和评审规则与 `ait push` 相同：

```bash
git clone http://alice@127.0.0.1:7878/r/shop.git
git push origin main
git push origin v1.0
git push origin --delete old-branch
```

用户名填 ait 账号，口令可以是登录密码，也可以是 ait 令牌。越权改动被转为评审时，`git push` 会显示 `remote rejected` 和评审编号。

### 账号与分支保护

- `POST /api/password` `{ old_password, new_password }`：修改密码，注销该用户的全部会话并返回新令牌。
- `POST /api/logout-all`：注销当前用户的全部会话。
- 令牌默认 30 天过期，用 `AIT_TOKEN_TTL_DAYS` 调整，设为 `0` 表示不过期。
- `GET/PUT /api/repos/:name/protected` `{ patterns: ["main", "release/*"] }`：受保护分支，默认 `main`、`master`。受保护分支不能删除；推送到受保护分支时，越权或超出职责限制的改动转为评审。
- 删除分支：建分支的人或仓库所有者可以删。推送、移动、删除标签只限仓库所有者，移动已有标签需要 `--force`。

## 规划与模型

`examples/shop.plan.md`：

```text
## 账户 (@alice)
职责: 登录、注册与会话
目录: src/account/**, src/session/**

## 商品 (@bob)
职责: 商品与库存
目录: src/catalog/**
```

也支持 `@bob src/catalog/** 商品与库存`，以及：

```json
{"assignments":[{"user":"bob","duty":"商品与库存","paths":["src/catalog/**"]}]}
```

`ait plan apply --file examples/shop.plan.md --llm` 会把规划交给服务进程环境变量里配置的 OpenAI 兼容接口：

- `AIT_LLM_BASE_URL`，例如 `https://api.openai.com/v1`
- `AIT_LLM_API_KEY`
- `AIT_LLM_MODEL`，默认 `gpt-4o-mini`

对应的 HTTP 接口是 `PUT /api/repos/:name/plan`，正文为 `{ "content", "apply", "use_llm" }`。只有仓库所有者可以修改规划。

成员与限制和规则、职责放在同一份权限策略里：

```json
{
  "rules": [{ "user": "bob", "pattern": "src/api/**", "access": "write" }],
  "duties": [{ "user": "bob", "summary": "后端接口 agent", "paths": ["src/api/**"] }],
  "members": [
    { "user": "bob", "kind": "agent", "supervisor": "alice",
      "limits": { "max_files": 10, "max_lines": 400, "required_checks": ["test"], "human_review": false } },
    { "user": "ci-bot", "kind": "ci" }
  ]
}
```

`PUT /api/repos/:name/policy` 保存。`POST /api/repos/:name/policy/preview` 接受同样的正文，不保存，返回每个目录谁能写、谁被禁止、谁负责，以及每个成员的权限清单、提示和错误。

## 校验

```toml
# .ait/checks.toml
[[check]]
name = "test"
run = "cargo test"
image = "rust:1"
required = true
paths = ["src/**", "tests/**"]   # 只在改动命中这些路径时适用
runner = "any"                   # report、server 或 any
timeout = 600
```

- `GET /api/repos/:name/checks?reference=` 返回某个提交声明的校验、门禁和执行记录。
- `POST /api/repos/:name/checks` 上报 `{ reference, name, status, summary, url, log }`，状态为 `success`、`failure`、`error` 或 `skipped`。
- `POST /api/repos/:name/checks/run` 按 `{ reference, name? }` 在服务端执行。

服务端执行器默认关闭，设置 `AIT_CHECK_RUNNER=docker` 后启用。每项校验在无网络、去掉全部 capability、`no-new-privileges`、限制进程数、内存和 CPU 的容器里运行。`AIT_CHECK_IMAGES` 是逗号分隔的镜像白名单，`AIT_CHECK_IMAGE` 是默认镜像（`alpine:3`），`AIT_DOCKER` 指定 docker 命令。执行器开启时，新的合并请求和推送会自动开始服务端校验。

## AI 审查者

`GET/PUT /api/repos/:name/ai` 保存 `{ enabled, auto, base_url, model, api_key, instructions }`。密钥只写不读，读取时只返回 `has_api_key`。仓库没有配置模型时退回 `AIT_LLM_*` 环境变量，再没有就只运行内置规则。`POST /api/repos/:name/pulls/:id/ai-review` 和 `POST /api/repos/:name/reviews/:id/ai-review` 发起审查；开启 `auto` 后，合并请求和推送评审创建时自动审查。

## 协作

- `GET/POST /api/repos/:name/claims` 查看或登记工作意图 `{ paths, intent, branch?, ttl_minutes? }`，返回结果里列出重叠的意图。
- `DELETE /api/repos/:name/claims/:id` 结束一条意图。
- `GET /api/repos/:name/events` 返回最近的推送、评审、合并、校验、AI 审查和规划变更。

## Web 前端

构建 [ait-web](https://github.com/liubox123/ait-web) 后交给服务端：

```bash
AIT_WEB_DIR=/path/to/ait-web/dist ait serve
```

页面在 `/ui/` 下。

## MCP

在仓库目录启动 `ait mcp`。本仓库的 `.cursor/mcp.json` 会向 Cursor 注册这个服务。等价配置是：

```json
{
  "mcpServers": {
    "ait": {
      "command": "ait",
      "args": ["mcp"]
    }
  }
}
```

工具：`status`、`diff`、`log`、`permissions`、`check_paths`、`commit`、`restore`、`propose`、`push`、`fetch`、`pull`、`branch`、`checkout`、`merge`、`pr`、`review`、`plan`、`owners`、`claim`、`checks`、`ai_review`。写代码前先调用 `check_paths` 和 `owners`；较大的改动先用 `claim` 登记意图，用 `checks` 上报本地测试结果，用 `ai_review` 请 AI 审查者看一遍。`commit` 拒绝越权文件后，用 `restore` 把它撤出暂存区。合并进行中时，`commit` 只检查本人在对方分支之上的改动。

## 发布

`.github/workflows/build.yml` 在推送到 `main` 或打开 Pull Request 时，于 Ubuntu、macOS、Windows 上运行 `cargo test`，并编译：

- Linux x86_64 / arm64
- macOS arm64 / x86_64
- Windows x86_64 / arm64

推送 `v*` 标签后，这些文件会挂到对应的 GitHub Release 上。

```bash
git tag v0.1.1
git push origin v0.1.1
```

## 许可

Copyright (C) 2026 liubox123

ait 按照 [GNU 通用公共许可证第三版](LICENSE)（GNU GPL v3）授权。你可以运行、研究和再发布它；再发布时要提供对应的源代码，并且衍生作品继续使用同一许可证。

## 和 Git 的边界

已互通：上面表格里的命令，pack 与 idx v2 的读写（含增量对象），reflog，附注标签，系统 `git` 通过 smart HTTP 克隆、拉取和推送 ait 服务。

还没有：bisect、子模块、工作树（worktree）、浅克隆和部分克隆、标签与提交签名、SSH 传输、合并策略 octopus。推送到 GitHub 仍用系统 Git。ait 的权限、评审和合并请求发生在推送到 ait 服务的时候。
