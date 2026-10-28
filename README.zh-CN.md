# ait

[English](README.md) · [中文](README.zh-CN.md)

面向 AI 时代的 Git。本地仓库仍是标准 Git 对象、索引和引用，系统自带的 `git` 可以直接阅读。人和多个 agent 一起改代码时，服务端按项目规划决定谁能直接修改哪些目录。职责之内的改动进入分支；跨出职责的改动可以写在本地，但要经过评审或合并请求，才会进入目标分支。

AI 时代的开发和过去不一样：同一个人会在不同环境里驱动不同的 agent，短时间产生大量代码；需求和实现都在变，提示词也拦不住意料之外的编辑；多个 agent 各写各的，目录和结构会对不上。ait 把「谁负责哪一块」变成仓库规则，让生成、修改和合并都经过这条边界。

这些规则还很粗。欢迎在 [Discussions](https://github.com/liubox123/ait/discussions) 里讨论：权限该分到目录还是到符号，评审该由人做还是由模型做，agent 之间怎样对齐同一套结构。Issue 用来报具体问题。

## 功能

- **Git 日常命令。** `init`、`add`、`status`、`diff`、`commit`、`log`、`branch`、`checkout`、`merge`、`push`、`pull`。对象格式与 Git 互通，`git fsck` 和 `git gc` 之后仍能读 pack。
- **目录写权限。** 克隆和拉取时收到修改权限清单，保存在 `.git/ait/manifest.json`。`write` 可以直接推送，没有写权限的路径进入评审，`deny` 会拒绝整次推送。所有者始终可以修改整个仓库。
- **项目规划。** 用一份规划划分用户、职责和目录。可以按标题解析，也可以让模型解析。
- **评审。** 直接推到 `main` 时，越权修改不会移动分支，而是生成一条评审，由对该路径有写权限的人通过或拒绝。
- **合并请求。** 其他分支可以先推上去，再用 `ait pr create` 请求合并。职责内的改动作者可以自己合并；越权路径要先由有写权限的人通过。不能推送到别人的分支。目标分支已经改过同一文件时，合并会报冲突。
- **MCP。** agent 在仓库目录运行 `ait mcp`，写之前可以查询职责和路径权限。`commit` 只接受写权限内的文件，跨目录修改用 `propose`。
- **管理页面。** `ait serve` 后可以注册、建仓库、浏览代码和提交、处理合并请求与评审、编辑路径规则、粘贴规划。
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

密码从 `AIT_PASSWORD` 读取，否则从终端读取。提交身份用 `ait config user.name` 和 `ait config user.email`，也可以用 `AIT_AUTHOR_NAME` / `AIT_AUTHOR_EMAIL`。第一个注册的用户是管理员。

## 命令

```bash
ait init
ait add
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

`ait plan apply --file examples/shop.plan.md --llm` 会把规划交给 OpenAI 兼容接口。这是服务进程的环境变量，还没有按仓库保存模型配置的接口，也还没有模型审阅：

- `AIT_LLM_BASE_URL`，例如 `https://api.openai.com/v1`
- `AIT_LLM_API_KEY`
- `AIT_LLM_MODEL`，默认 `gpt-4o-mini`

对应的 HTTP 接口是 `PUT /api/repos/:name/plan`，正文为 `{ "content", "apply", "use_llm" }`。只有仓库所有者可以修改规划。

## MCP

在仓库目录启动 `ait mcp`。Cursor 配置：

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

工具：`status`、`diff`、`log`、`permissions`、`check_paths`、`commit`、`propose`、`push`、`plan`、`owners`。写代码前先调用 `check_paths` 和 `owners`。

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

已互通的日常能力：初始化、暂存、状态、差异、提交、分支、检出、快进、三方合并、日志，以及读取 `git gc` 生成的 pack。

还没有：rebase、stash、bisect、子模块、标签签名、Git 智能传输协议。推送到 GitHub 仍用系统 Git。ait 的权限、评审和合并请求发生在 `ait push` / `ait pr` 到 ait 服务的时候。对象以 loose 和已有 pack 保存，适合中小仓库。
