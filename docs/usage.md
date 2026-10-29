# ait usage guide

[English](usage.md) · [中文](usage.zh-CN.md) · [Back to README](../README.md)

This guide starts from a running server and walks a two-person example through the daily flow: creating a repository, assigning duties, committing, reviewing out-of-scope edits, branches and merge requests. It then covers connecting agents, checks and AI review, system Git clients, and common errors. The sample output comes from real runs. ait prints its messages in Chinese, so each sample is followed by a short English note where it helps. Commit ids will differ on your machine.

## Contents

1. [Concepts](#concepts)
2. [Install and start the server](#install-and-start-the-server)
3. [Accounts](#accounts)
4. [Create a repository and assign duties](#create-a-repository-and-assign-duties)
5. [Daily work](#daily-work)
6. [Out-of-scope edits and push reviews](#out-of-scope-edits-and-push-reviews)
7. [Branches and merge requests](#branches-and-merge-requests)
8. [Connecting agents](#connecting-agents)
9. [Members, limits, and the permission policy](#members-limits-and-the-permission-policy)
10. [Checks and AI review](#checks-and-ai-review)
11. [Using system Git](#using-system-git)
12. [Undo and rewrite cheat sheet](#undo-and-rewrite-cheat-sheet)
13. [Environment variables](#environment-variables)
14. [Common errors](#common-errors)

## Concepts

- **Repository owner.** Whoever creates a repository owns it. Owners can change the whole repository, edit the plan, and manage protected branches and tags. The first user to register on a server is the administrator and is an owner of every repository.
- **Plan and duties.** A plan assigns directories to people and agents, for example “bob owns `src/catalog/**`”. Applying the plan gives each person a write set.
- **Permission manifest.** On clone, pull, and push the server sends the current user's manifest, stored in `.git/ait/manifest.json`. `ait perm` shows it at any time.
- **Three access levels.** `write` pushes directly. Paths without write access go to review (`review`) on push. `deny` rejects the whole push.
- **Push review.** When a push to a branch touches paths outside the author's write set, the branch does not move. The server opens a review (`r-…`) for someone who can write those paths to approve or reject.
- **Merge request.** Work on your own branch, push it, and open a merge request with `ait pr create` (`pr-…`).
- **No limits locally.** Any file can be edited and committed locally. The boundary applies only when pushing to the server.

## Install and start the server

With Rust installed, install from source:

```bash
cargo install --path .
```

Binaries for each platform are also on [Releases](https://github.com/liubox123/ait/releases).

Start the server:

```bash
ait serve --bind 127.0.0.1:7878 --data ./ait-data
```

- `--bind` is the listen address, `127.0.0.1:7878` by default. Use `0.0.0.0:7878` to serve your network.
- `--data` is the data directory, `./ait-data` by default. It holds a bare Git repository per project, plus accounts and tokens.
- `http://127.0.0.1:7878` in a browser opens the built-in management page.

With Docker:

```bash
docker compose up -d --build
```

Data lives in a volume named `ait-data`. Once the administrator has registered, set `AIT_ALLOW_REGISTER` to `false` in `docker-compose.yml` and start the container again.

Behind a reverse proxy, set `AIT_PUBLIC_URL=https://git.example.com` so clone URLs are correct.

## Accounts

```bash
export AIT_PASSWORD=secret
ait register --server http://127.0.0.1:7878 --user alice
```

```text
已注册 alice（首位用户，管理员）
```

(“Registered alice, first user, administrator.”)

- The first user to register is the administrator. Others register with the same command.
- Existing accounts sign in with `ait login --server … --user …`.
- The password is read from `AIT_PASSWORD`, or from the terminal when it is unset. Passwords are stored with Argon2id.
- Tokens are saved in `~/.ait/credentials.json`, one per server. `AIT_HOME` changes the directory; that is how several identities on one machine, such as a person and an agent, stay apart.
- Once the administrator exists, start the server with `AIT_ALLOW_REGISTER=false` to stop new registrations.

Commit author:

```bash
ait config user.name "Alice"
ait config user.email alice@example.com
```

`AIT_AUTHOR_NAME` and `AIT_AUTHOR_EMAIL` work too.

Tokens and passwords:

- Tokens expire after 30 days by default. Commands then report “登录已过期，请重新登录” (session expired); run `ait login` again. The server's `AIT_TOKEN_TTL_DAYS` changes the lifetime, and `0` disables expiry.
- Change a password with `POST /api/password` and body `{ "old_password", "new_password" }`. This signs out every session of that user and returns a new token.
- `POST /api/logout-all` signs out every session. Use it when a token leaks.

## Create a repository and assign duties

alice creates and clones the repository:

```bash
ait repo create shop -d "Sample shop"
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

(The repository is created with alice as owner, and her manifest says she may change `**`.)

Add the initial code and push:

```bash
cd shop
ait add
ait commit -m "init"
ait push
```

```text
[main 72ab6c7] init
 * [新引用]          main -> main  已更新分支
```

Write a plan, `plan.md`:

```text
## Accounts (@alice)
Duty: sign-in, sign-up, and sessions
Paths: src/account/**

## Catalog (@bob)
Duty: products and stock
Paths: src/catalog/**
```

Apply it (owners only):

```bash
ait plan apply --file plan.md
```

```text
已根据规划文本更新权限
- alice：Accounts；sign-in, sign-up, and sessions
    src/account/**
- bob：Catalog；products and stock
    src/catalog/**
```

`ait plan show` prints the plan stored on the server. Two more formats are accepted:

```text
@bob src/catalog/** products and stock
```

```json
{"assignments":[{"user":"bob","duty":"products and stock","paths":["src/catalog/**"]}]}
```

For a plan in free-form prose, add `--llm` to have a model parse it: `ait plan apply --file plan.md --llm`. The server process needs `AIT_LLM_BASE_URL`, `AIT_LLM_API_KEY`, and optionally `AIT_LLM_MODEL` (default `gpt-4o-mini`).

Path patterns: `src/catalog/**` means every file under that directory. `*` matches any characters within one directory or file name, and `?` matches one character. `**` on its own means the whole repository.

## Daily work

bob registers, clones, and checks his permissions first:

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

(bob may change `src/catalog/**` directly. Other paths can be edited locally and go to review on push.)

Edits inside the duty push directly:

```bash
ait commit -a -m "update prices"
ait push
```

```text
[main 4e270e1] update prices
   72ab6c7..4e270e1  main -> main  已更新分支
```

Everyday commands match Git:

```bash
ait status            # status, plus files outside your write set
ait diff              # worktree against index
ait diff --cached     # index against HEAD
ait add src/catalog   # stage; with no path, stage every change
ait restore --staged src/catalog/items.rs   # unstage
ait commit -m "message"
ait log --oneline --graph -n 20
ait pull              # fetch and merge, and refresh the manifest
ait push
```

`ait status` tells you in advance which files will need review:

```text
位于分支 main
与 origin/main 一致

尚未暂存的更改:
  修改: src/account/login.rs

这些路径不在你的写权限内，推送时会进入评审:
  src/account/login.rs
```

(The last block lists paths outside the write set that will go to review.)

`pull` merges by default. `ait pull --rebase` rebases instead, and `ait pull --ff-only` accepts only fast-forwards. When the remote has commits you have not pulled, `ait push` refuses with “不是快进” (not a fast-forward); run `ait pull` first.

## Out-of-scope edits and push reviews

bob also edits a file in the accounts module, commits, and pushes:

```bash
ait commit -a -m "tweak login"
ait push
```

```text
 ? [待评审]          main -> main (修改了写权限之外的路径，已提交评审 r-40641f08)
  需要评审的路径:
    src/account/login.rs
```

(“Pending review: changed paths outside the write set, review r-40641f08 opened.”)

`main` did not move. alice, who can write `src/account/**`, looks at it:

```bash
ait review list
```

```text
r-40641f08	pending	bob	refs/heads/main
  越权路径: src/account/login.rs
```

Approving moves the branch:

```bash
ait review approve r-40641f08
```

```text
已通过 r-40641f08，分支已更新
```

Or reject it with a reason:

```bash
ait review reject r-40641f08 --reason "login belongs to accounts"
```

After a rejection bob's commit is still in his local history. He has to drop it, or the next push will carry it again:

```bash
ait reset --hard origin/main
```

After an approval, `ait pull` brings bob back in line with the server.

Paths marked `deny` never become a review; the whole push fails.

## Branches and merge requests

For larger or cross-module work, use your own branch:

```bash
ait switch -c bob/stock
# edit files...
ait add
ait commit -m "stock and login changes"
ait push -u origin bob/stock
ait pr create "Stock feature"
```

```text
已创建合并请求 pr-9e94409f Stock feature
bob/stock -> main
```

`--base` picks the target branch, `main` by default. `-u` records the upstream, so plain `ait push` and `ait pull` work on that branch afterwards.

```bash
ait pr list
```

```text
pr-9e94409f	open	bob	bob/stock -> main	Stock feature
  需要评审: src/account/login.rs
```

(“Needs review: src/account/login.rs.”)

Who can merge:

- When every change is inside the author's duty, the author can run `ait pr merge`.
- When it includes out-of-scope paths, someone who can write those paths must run `ait pr approve` first, and then **the approver or a repository owner** merges. If the author runs `pr merge` at that point, the answer is “需要有写权限的人先通过，才能合并到目标分支”.
- With required checks configured, merging waits until they pass.
- When the target branch has changed the same file, the merge reports a conflict. Run `ait merge main` on the branch, resolve, and push again.

```bash
ait pr approve pr-9e94409f   # alice
ait pr merge pr-9e94409f     # alice
ait pr close pr-9e94409f     # close it if it is no longer wanted
```

Branch rules:

- You cannot push to a branch someone else created.
- Delete a remote branch with `ait push origin --delete bob/stock`. The branch creator or a repository owner may delete it.
- Protected branches (`main` and `master` by default) cannot be deleted. Owners change the list with `GET/PUT /api/repos/:name/protected` and body `{ "patterns": ["main", "release/*"] }`.
- Only owners can push, move, or delete tags: `ait tag -a v1.0 -m "message"`, then `ait push --tags`. Moving an existing tag needs `--force`.

## Connecting agents

Give every agent its own account and its own `AIT_HOME`, so commits, reviews, and activity show who did what.

```bash
AIT_HOME=~/.ait-agent-api AIT_PASSWORD=… ait register --server http://127.0.0.1:7878 --user api-agent
```

Start the MCP server in the repository directory with `ait mcp` (or `ait mcp --repo path`). A Cursor configuration:

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

Tools:

| Tool | Purpose |
| --- | --- |
| `permissions`, `check_paths`, `owners` | Check permissions and path owners before writing |
| `status`, `diff`, `log` | Inspect state and history |
| `commit` | Commit files inside the write set only; fails on out-of-scope files |
| `restore` | Unstage files |
| `propose` | Commit and push out-of-scope edits as a review |
| `push`, `fetch`, `pull`, `branch`, `checkout`, `merge` | Sync and branches |
| `pr`, `review` | Merge requests and reviews |
| `plan` | Read the plan |
| `claim` | Start, list, and finish work intents |
| `checks` | Read, report, or request checks |
| `ai_review` | Ask the AI reviewer about a merge request or review |

A workflow worth putting in the agent's rules:

1. Before editing, call `check_paths` and `owners` to see who owns the paths.
2. For larger work, `claim` first (`action: "start"`) and look for overlaps in the reply.
3. Commit in-duty work with `commit`. Send out-of-scope edits through `propose` instead of working around it.
4. After running tests, report them with `checks`. Agent reports are kept for reference and do not satisfy required checks.
5. After opening a merge request, ask `ai_review` for a pass, and finish the `claim` when done.

## Members, limits, and the permission policy

Beyond the plan, the permission policy sets member kinds and limits. Owners save it with `PUT /api/repos/:name/policy`, or edit it on the planner page of [ait-web](https://github.com/liubox123/ait-web):

```json
{
  "rules": [
    { "user": "bob", "pattern": "src/api/**", "access": "write" },
    { "user": "bob", "pattern": "migrations/**", "access": "deny" }
  ],
  "duties": [{ "user": "bob", "summary": "API agent", "paths": ["src/api/**"] }],
  "members": [
    { "user": "bob", "kind": "agent", "supervisor": "alice",
      "limits": { "max_files": 10, "max_lines": 400, "required_checks": ["test"], "human_review": false } },
    { "user": "ci-bot", "kind": "ci" }
  ]
}
```

- `access` is `write`, `review`, or `deny`.
- `kind` is `human` (the default), `agent`, or `ci`. An agent can have a `supervisor`.
- In `limits`, `max_files` and `max_lines` cap the size of one change, `required_checks` adds required checks, and `human_review: true` makes even in-scope changes wait for a second person. A change over its limits still lands, after someone else approves it.

Preview before saving: `POST /api/repos/:name/policy/preview` takes the same body and returns, for every directory, who may write, who is denied, and who owns it, plus each member's manifest and any warnings.

## Checks and AI review

Declare checks in `.ait/checks.toml`:

```toml
[[check]]
name = "test"
run = "cargo test"
image = "rust:1"
required = true
paths = ["src/**", "tests/**"]   # applies only when the change touches these paths
runner = "any"                   # report, server, or any
timeout = 600
```

- Results come from CI, reporting as a `ci` account through `POST /api/repos/:name/checks`, or from the server running them in a Docker sandbox (start the server with `AIT_CHECK_RUNNER=docker`, and limit images with `AIT_CHECK_IMAGES`).
- Only results from the server runner, a repository owner, or a `ci` account satisfy required checks.
- Until required checks pass, merge requests cannot merge and push reviews cannot be approved.

AI reviewer:

- Without a model, built-in rules still run: boundary violations, oversized changes, likely secrets, conflict markers, deleted tests, debug output, and overlap with someone else's work intent.
- Owners configure an OpenAI-compatible model with `PUT /api/repos/:name/ai` and `{ enabled, auto, base_url, model, api_key, instructions }`. With `auto` on, new merge requests and push reviews are reviewed automatically.
- The reviewer only comments. A person with write access decides.

API details are in the README sections [Checks](../README.md#checks), [AI reviewer](../README.md#ai-reviewer), and [Collaboration](../README.md#collaboration).

## Using system Git

The server speaks Git smart HTTP, so people can collaborate without installing ait:

```bash
git clone http://bob@127.0.0.1:7878/r/shop.git
git push origin main
git push origin --delete old-branch
```

Use the ait account as the user name, and the password or an ait token as the password. Permissions, protected branches, and review rules are the same as for `ait push`. When an out-of-scope push becomes a review, `git push` shows `remote rejected` with the review id. System Git does not receive the manifest, so check your duties and paths on the management page before pushing.

A repository managed by ait is also a standard Git repository; `git log`, `git diff`, and your IDE's Git panel work on it directly.

## Undo and rewrite cheat sheet

| To do this | Run |
| --- | --- |
| Discard worktree edits to a file | `ait restore file` |
| Unstage a file | `ait restore --staged file` |
| Fix the last commit (not yet pushed) | `ait commit --amend -m "new message"` |
| Undo a pushed commit | `ait revert commit` |
| Go back to a commit and drop later work | `ait reset --hard commit` |
| Recover a lost commit | `ait reflog`, then `ait reset --hard HEAD@{n}` |
| Put edits aside for now | `ait stash`, later `ait stash pop` |
| Take one commit from another branch | `ait cherry-pick commit` |
| Clean up this branch's commits | `ait rebase -i main` |
| Give up after a conflict | `ait merge --abort` or `ait rebase --abort` |
| Get a file from another version | `ait checkout version -- file` |

Switching branches carries local edits along, as in Git. It refuses only when an edited file differs between the two branches.

## Environment variables

| Variable | Side | Meaning |
| --- | --- | --- |
| `AIT_PASSWORD` | client | Password for register and login |
| `AIT_HOME` | client | Credentials directory, `~/.ait` by default |
| `AIT_AUTHOR_NAME`, `AIT_AUTHOR_EMAIL` | client | Commit author |
| `AIT_ALLOW_REGISTER` | server | `false` disables registration |
| `AIT_TOKEN_TTL_DAYS` | server | Token lifetime in days, 30 by default, `0` never expires |
| `AIT_PUBLIC_URL` | server | Public address used in clone URLs |
| `AIT_WEB_DIR` | server | ait-web build output, served under `/ui/` |
| `AIT_LLM_BASE_URL`, `AIT_LLM_API_KEY`, `AIT_LLM_MODEL` | server | Default model for plan parsing and AI review |
| `AIT_CHECK_RUNNER` | server | `docker` enables sandboxed checks |
| `AIT_CHECK_IMAGES`, `AIT_CHECK_IMAGE`, `AIT_DOCKER` | server | Image allowlist, default image, docker command |

## Common errors

| Message | Cause and fix |
| --- | --- |
| `Address already in use` | The port is taken. Pick another `--bind` port or stop the running server |
| `还没有登录` / `请先 ait login 登录这个服务` (not signed in) | No token for this server under the current `AIT_HOME`. Run `ait login` |
| `登录已过期，请重新登录` (session expired) | The token is past its lifetime. Run `ait login` again |
| `用户已存在` (user exists) | Pick another name, or use `ait login` |
| `不是快进，请先 ait pull 合并远端改动，或使用 --force` (not a fast-forward) | The remote has commits you have not pulled. Run `ait pull` |
| `修改了写权限之外的路径，已提交评审 r-…` (review opened) | Not an error. Wait for someone with access to handle the review |
| `这些路径禁止修改` (paths denied) | The change touches `deny` paths. Drop those edits and push again |
| `需要有写权限的人先通过，才能合并到目标分支` (approval needed) | The merge request has out-of-scope paths. Someone with access approves, then the approver or an owner merges |
| `必过校验还没有通过` (required checks pending) | Wait for checks to pass, or have CI or the server runner report them |
| `您对下列文件的本地修改将被检出操作覆盖` (local changes would be overwritten) | Commit or `ait stash` before switching |
| `不在 Git 仓库中` (not in a repository) | Change into the repository, or give `ait mcp --repo` a path |
