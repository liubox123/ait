# ait

[English](README.md) · [中文](README.zh-CN.md)

Git for the AI era. A local repository is still standard Git objects, an index, and refs, so the system `git` can read it. When people and agents edit together, the server uses a project plan to decide who may change which directories. Work inside a duty lands on the branch. Work outside that duty can be written locally, and it reaches the target branch only after a review or a merge request.

Development in the AI era produces a lot of code quickly: one person may drive different agents in different environments, requirements and implementations keep changing, and prompts do not stop unexpected edits. Agents also drift apart in structure when each one writes on its own. ait turns “who owns this area” into repository rules, so generation, edits, and merges pass through that boundary.

The rules are still rough. Join the [Discussions](https://github.com/liubox123/ait/discussions) about whether permissions should stop at directories or reach symbols, whether review should be done by people or by models, and how agents should share one structure. Use Issues for concrete bugs.

## Features

- **Everyday Git commands.** `init`, `add`, `status`, `diff`, `commit`, `log`, `branch`, `checkout`, `merge`, `push`, and `pull`. The object format matches Git, including packs produced by `git gc`.
- **Directory write permissions.** Clone and pull deliver a modification manifest, stored at `.git/ait/manifest.json`. `write` can push directly. Paths without write permission become a review. `deny` rejects the whole push. The owner can always change the whole repository.
- **Project plan.** One plan assigns users, duties, and directories. It can be parsed from headings, or by a model.
- **Review.** A direct push to `main` that leaves the author’s write set does not move the branch. It opens a review for someone who may write those paths to approve or reject.
- **Merge requests.** Other branches can be pushed first, then opened with `ait pr create`. The author can merge changes inside their own duty. Paths outside that duty need approval from someone who can write them. A branch owned by someone else cannot be updated. Merging reports a conflict when the target branch has already changed the same file.
- **MCP.** An agent runs `ait mcp` in the repository and can read duties and path permissions before writing. `commit` accepts only files inside the write set. Cross-directory edits use `propose`.
- **Management page.** After `ait serve`, people can register, create repositories, browse code and commits, handle merge requests and reviews, edit path rules, and paste a plan.
- **Multi-platform releases.** A push to `main` tests and builds on Linux, macOS, and Windows. A `v*` tag publishes the binaries on GitHub Releases.

## Start

With Rust installed:

```bash
cargo install --path .
ait serve --bind 127.0.0.1:7878 --data ./ait-data
```

Prebuilt binaries are on the [Releases](https://github.com/liubox123/ait/releases) page.

In another terminal:

```bash
export AIT_PASSWORD=secret
ait register --server http://127.0.0.1:7878 --user alice
ait repo create shop
ait clone http://127.0.0.1:7878/r/shop
cd shop
ait plan apply --file ../examples/shop.plan.md
```

The password is read from `AIT_PASSWORD`, or from the terminal. Set the commit identity with `ait config user.name` and `ait config user.email`, or with `AIT_AUTHOR_NAME` / `AIT_AUTHOR_EMAIL`. The first registered user is the administrator. Passwords are stored as Argon2id. Set `AIT_ALLOW_REGISTER=false` to stop later registrations.

## Docker

```bash
docker compose up -d --build
```

The page listens on `http://127.0.0.1:7878`. Repository data stays in the `ait-data` volume. After the administrator account exists, set `AIT_ALLOW_REGISTER=false` in `docker-compose.yml` and start the container again.

## Commands

```bash
ait init
ait add
ait restore --staged src/lib.rs
ait status
ait diff
ait commit -m "message"
ait log
ait branch
ait checkout -b bob/price
ait merge main
ait pull
ait push
ait perm
ait review list
ait review approve r-1a2b3c4d
ait review reject r-1a2b3c4d --reason "leave this alone for now"
ait pr list
ait pr create "adjust prices"
ait pr approve pr-1a2b3c4d
ait pr merge pr-1a2b3c4d
ait pr close pr-1a2b3c4d
```

### Git-compatible commands

Flags and behavior follow Git, and the repository stays readable and writable by system `git`.

| Area | Commands |
| --- | --- |
| Undo and rewrite | `reset --soft/--mixed/--hard`, `revert`, `cherry-pick [-x] [-n]`, `commit --amend`, `stash push/list/show/apply/pop/drop/clear`, `checkout <rev> -- <path>`, `checkout --detach`, `switch -`, `restore [--staged] [--worktree] [--source <rev>]`, `rm [--cached] [-r]`, `mv`, `clean -n/-f/-d/-x`, `reflog` |
| Rebase | `rebase <upstream> [--onto <base>]`, `rebase -i` (pick/reword/edit/squash/fixup/drop/exec/break; `--todo <file>` supplies the list directly), `--continue/--skip/--abort` |
| Inspect | `rev-parse` (`~n`, `^n`, `^{tree}`, `@{n}`, `@{-n}`, `@{u}`, `rev:path`, `:/text`), `log` (`--oneline --graph --all -n --author --grep --since --until --first-parent --no-merges -p --stat --format`, `A..B`, `A...B`, `-- path`), `show`, `diff A B`, `blame [-L]`, `grep`, `tag [-a] [-d]`, `describe`, `shortlog`, `cat-file`, `ls-files`, `ls-tree`, `merge-base` |
| Remotes | `remote add/remove/rename/set-url/get-url/-v`, `fetch [--all] [--prune]`, `pull [--rebase] [--ff-only]`, `push [-u] [-f] [--tags] [--delete] [<remote>] [<src>:<dst>]`, upstream tracking with `branch -u` |
| Maintenance | `gc [--prune=now]`, `pack-refs`, `fsck`, `count-objects -v` |

`pull` and `push` move objects as packs. When the server rejects a push, the objects stay in an in-memory quarantine and never reach the server repository.

### System Git clients

The server speaks Git smart HTTP (protocol v2 for fetch, with v0 fallback, and receive-pack for push). Permissions, protected branches, and review rules are the same as for `ait push`:

```bash
git clone http://alice@127.0.0.1:7878/r/shop.git
git push origin main
git push origin v1.0
git push origin --delete old-branch
```

Use the ait account name as the user name and either the password or an ait token as the password. When a push is turned into a review, `git push` reports `remote rejected` with the review id.

### Accounts and branch protection

- `POST /api/password` `{ old_password, new_password }` changes the password, signs out every session of that user, and returns a new token.
- `POST /api/logout-all` signs out every session of the current user.
- Tokens expire after 30 days by default. Set `AIT_TOKEN_TTL_DAYS` to change this, or `0` to disable expiry.
- `GET/PUT /api/repos/:name/protected` `{ patterns: ["main", "release/*"] }` lists protected branches, `main` and `master` by default. Protected branches cannot be deleted, and pushes to them that exceed the pusher's permissions or duty limits become reviews.
- A branch can be deleted by whoever created it or by a repository owner. Only owners can push, move, or delete tags, and moving an existing tag needs `--force`.

## Plans and models

`examples/shop.plan.md`:

```text
## Accounts (@alice)
Duty: sign-in, registration, and sessions
Paths: src/account/**, src/session/**

## Catalog (@bob)
Duty: products and inventory
Paths: src/catalog/**
```

The Chinese headings `职责:` and `目录:` are accepted too, as is `@bob src/catalog/** products and inventory`, and:

```json
{"assignments":[{"user":"bob","duty":"products and inventory","paths":["src/catalog/**"]}]}
```

`ait plan apply --file examples/shop.plan.md --llm` sends the plan to an OpenAI-compatible API. The endpoint is configured on the server process. There is no per-repository model setting yet, and no model review:

- `AIT_LLM_BASE_URL`, for example `https://api.openai.com/v1`
- `AIT_LLM_API_KEY`
- `AIT_LLM_MODEL`, default `gpt-4o-mini`

The HTTP call is `PUT /api/repos/:name/plan` with `{ "content", "apply", "use_llm" }`. Only a repository owner can change the plan.

## MCP

Run `ait mcp` in the repository. This repository's `.cursor/mcp.json` registers that server for Cursor. The same configuration is:

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

Tools: `status`, `diff`, `log`, `permissions`, `check_paths`, `commit`, `restore`, `propose`, `push`, `fetch`, `pull`, `branch`, `checkout`, `merge`, `pr`, `review`, `plan`, and `owners`. Call `check_paths` and `owners` before writing. When `commit` refuses a path outside the write set, `restore` takes it back out of the index. While a merge is in progress, `commit` checks only your own changes on top of the merged branch.

## Releases

`.github/workflows/build.yml` runs `cargo test` on Ubuntu, macOS, and Windows for pushes to `main` and for pull requests, and builds:

- Linux x86_64 / arm64
- macOS arm64 / x86_64
- Windows x86_64 / arm64

Pushing a `v*` tag attaches those binaries to the GitHub Release.

```bash
git tag v0.1.1
git push origin v0.1.1
```

## License

Copyright (C) 2026 liubox123

ait is licensed under the [GNU General Public License v3](LICENSE). You may run, study, and redistribute it. Redistribution includes the corresponding source, and derivative works stay under the same license.

## Relation to Git

Interoperable today: the commands in the table above, reading and writing pack and idx v2 files (including deltas), reflogs, annotated tags, and system `git` cloning, fetching, and pushing to an ait server over smart HTTP.

Still ahead: bisect, submodules, linked worktrees, shallow and partial clones, signed tags and commits, the SSH transport, and octopus merges. Pushing to GitHub still uses system Git. Permissions, reviews, and merge requests happen when you push to an ait server.
