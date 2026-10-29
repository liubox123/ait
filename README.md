# ait

[English](README.md) · [中文](README.zh-CN.md)

Git for the AI era. A local repository is still standard Git objects, an index, and refs, so the system `git` can read it. When people and agents edit together, the server uses a project plan to decide who may change which directories. Work inside a duty lands on the branch. Work outside that duty can be written locally, and it reaches the target branch only after a review or a merge request.

Development in the AI era produces a lot of code quickly: one person may drive different agents in different environments, requirements and implementations keep changing, and prompts do not stop unexpected edits. Agents also drift apart in structure when each one writes on its own. ait turns “who owns this area” into repository rules, so generation, edits, and merges pass through that boundary.

The rules are still rough. Join the [Discussions](https://github.com/liubox123/ait/discussions) about whether permissions should stop at directories or reach symbols, whether review should be done by people or by models, and how agents should share one structure. Use Issues for concrete bugs.

## Why ait

### The pain of coding with AI

- **Changes arrive faster than anyone can read them.** An agent edits dozens of files in minutes, while a person has to read every line before merging. Review becomes the bottleneck, and often only the test result gets checked.
- **Edits leave their lane.** Asked to fix one endpoint, an agent renames a shared function, edits a config file, and bumps a dependency. A prompt that says “only touch this directory” is not always followed.
- **Problems get quietly bypassed.** When tests fail, tests get deleted, skipped, or loosened. Debug output and conflict markers stay behind, and now and then a secret lands in the code.
- **Context gets lost.** A new session or a different tool forgets earlier agreements. The same problem gets solved several different ways, and the structure drifts.
- **History is hard to trace.** One commit mixes human and agent edits, and later nobody can say who asked for a line or why.
- **Undoing is expensive.** Large generated changes are tangled together. Removing part of one means reverting all of it or picking pieces by hand.

### The pain of collaborating with AI

- **Several agents edit the same place.** Agents work in parallel without knowing what the others are changing. Conflicts show up only at merge time, or worse, the merge succeeds and the logic contradicts itself.
- **Structures don't line up.** Each agent invents its own directories, names, and style, and the result reads like several projects stitched together.
- **Ownership lives only in documents.** Who owns which area sits in docs or chat history. Agents can't see it and Git doesn't enforce it: anyone who can push can change any file.
- **Permissions are too coarse.** Hosting platforms grant access per repository or branch, and can't say “this agent may only change `src/api/`”. A separate repository per agent splits the code apart.
- **Review falls on a few people.** Agents produce far more than people can review, and it is unclear who should look at a change that crosses modules.
- **Agent reports can't be trusted.** An agent says “tests pass”, and nobody can confirm it ran them, or which ones.
- **Nobody knows which agent did it.** One person runs agents in the editor, on the command line, and in CI under a single account, and when something breaks nobody can tell which one did it.

### What ait does

- **Ownership becomes repository rules.** A project plan assigns directories and duties, and every person and agent receives the same permission manifest. Agents can ask over MCP what they may change before writing. Edits outside a duty don't land on the branch; they become a review or a merge request, decided by someone who can write those paths.
- **Change size is limited.** A member can be limited in files and lines per change. Anything larger lands only after a second person approves it.
- **A machine reads it first.** The AI reviewer flags boundary violations, oversized changes, likely secrets, conflict markers, deleted tests, and debug output, so people can focus on the judgment calls.
- **Check results come from trusted sources.** Required checks count only results from the server sandbox, a repository owner, or a `ci` account. Agent reports are kept for reference.
- **Intent is recorded before work starts.** People and agents record the paths they plan to change. Overlaps are shown to both sides; nothing is locked and nobody is blocked.
- **Each agent has its own identity.** An agent is a separate member account with an optional supervisor, and commits, reviews, and activity are recorded under that account.

ait does not solve lost context or hard-to-undo large changes. Those still depend on working habits and other tools.

## Features

- **Everyday Git commands.** `init`, `add`, `status`, `diff`, `commit`, `log`, `branch`, `checkout`, `merge`, `push`, and `pull`. The object format matches Git, including packs produced by `git gc`.
- **Directory write permissions.** Clone and pull deliver a modification manifest, stored at `.git/ait/manifest.json`. `write` can push directly. Paths without write permission become a review. `deny` rejects the whole push. The owner can always change the whole repository.
- **Project plan.** One plan assigns users, duties, and directories. It can be parsed from headings, or by a model.
- **Review.** A direct push to `main` that leaves the author’s write set does not move the branch. It opens a review for someone who may write those paths to approve or reject.
- **Merge requests.** Other branches can be pushed first, then opened with `ait pr create`. The author can merge changes inside their own duty. Paths outside that duty need approval from someone who can write them. A branch owned by someone else cannot be updated. Merging reports a conflict when the target branch has already changed the same file.
- **MCP.** An agent runs `ait mcp` in the repository and can read duties and path permissions before writing. `commit` accepts only files inside the write set. Cross-directory edits use `propose`.
- **Members and limits.** Each member is a `human`, an `agent`, or a `ci` account. An agent can have a supervisor. Limits set the most files and lines per change, extra required checks, and whether every change needs a second person. A change over its limits still lands, after someone else approves it.
- **Checks.** `.ait/checks.toml` declares checks. Results are reported over the API, or run by the server in a Docker sandbox. Required checks gate merges and push reviews. Only results from the server runner, an owner, or a `ci` account count. Agent reports are kept for reference.
- **AI reviewer.** Built-in rules look for boundary violations, oversized changes, likely secrets, conflict markers, deleted tests, debug output, and overlap with someone else's work intent. With an OpenAI-compatible model configured for the repository, its opinion is added too. The reviewer only comments; a person with write access decides.
- **Work intents.** People and agents record which paths they are about to change. Intents never lock; overlaps are shown to both sides and to the AI reviewer.
- **Management page.** After `ait serve`, people can register, create repositories, browse code and commits, handle merge requests and reviews, edit path rules, and paste a plan. The fuller web front end is [ait-web](https://github.com/liubox123/ait-web), with pages adapted from GitLab, a directory × member planner, a collaboration board, and checks and AI review panels.
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

`ait plan apply --file examples/shop.plan.md --llm` sends the plan to an OpenAI-compatible API configured on the server process:

- `AIT_LLM_BASE_URL`, for example `https://api.openai.com/v1`
- `AIT_LLM_API_KEY`
- `AIT_LLM_MODEL`, default `gpt-4o-mini`

The HTTP call is `PUT /api/repos/:name/plan` with `{ "content", "apply", "use_llm" }`. Only a repository owner can change the plan.

Members and limits live in the policy next to rules and duties:

```json
{
  "rules": [{ "user": "bob", "pattern": "src/api/**", "access": "write" }],
  "duties": [{ "user": "bob", "summary": "API agent", "paths": ["src/api/**"] }],
  "members": [
    { "user": "bob", "kind": "agent", "supervisor": "alice",
      "limits": { "max_files": 10, "max_lines": 400, "required_checks": ["test"], "human_review": false } },
    { "user": "ci-bot", "kind": "ci" }
  ]
}
```

`PUT /api/repos/:name/policy` saves it. `POST /api/repos/:name/policy/preview` takes the same body and returns, for every directory, who may write it, who is denied, and who holds the duty, plus each member's manifest, warnings, and errors, without saving.

## Checks

```toml
# .ait/checks.toml
[[check]]
name = "test"
run = "cargo test"
image = "rust:1"
required = true
paths = ["src/**", "tests/**"]   # applies only when the change touches these paths
runner = "any"                   # report, server, or any
timeout = 600
```

- `GET /api/repos/:name/checks?reference=` lists declared checks, gates, and runs for a commit.
- `POST /api/repos/:name/checks` reports `{ reference, name, status, summary, url, log }`. Status is `success`, `failure`, `error`, or `skipped`.
- `POST /api/repos/:name/checks/run` runs server checks for `{ reference, name? }`.

The server runner is off by default. `AIT_CHECK_RUNNER=docker` enables it. Each check runs in a container with no network, all capabilities dropped, `no-new-privileges`, and limits on processes, memory, and CPU. `AIT_CHECK_IMAGES` is a comma-separated image allowlist, `AIT_CHECK_IMAGE` the default image (`alpine:3`), and `AIT_DOCKER` the docker binary. When the runner is on, new merge requests and pushes start their server checks automatically.

## AI reviewer

`GET/PUT /api/repos/:name/ai` holds `{ enabled, auto, base_url, model, api_key, instructions }`. The key is write-only: reads return `has_api_key`. Without a repository model, the reviewer falls back to the `AIT_LLM_*` variables, and without those it runs the built-in rules alone. `POST /api/repos/:name/pulls/:id/ai-review` and `POST /api/repos/:name/reviews/:id/ai-review` request a review. With `auto` on, merge requests and push reviews are reviewed when they are created.

## Collaboration

- `GET/POST /api/repos/:name/claims` lists or records a work intent `{ paths, intent, branch?, ttl_minutes? }`. The answer lists overlapping intents.
- `DELETE /api/repos/:name/claims/:id` finishes one.
- `GET /api/repos/:name/events` returns recent pushes, reviews, merges, checks, AI reviews, and plan changes.

## Web front end

Build [ait-web](https://github.com/liubox123/ait-web) and point the server at it:

```bash
AIT_WEB_DIR=/path/to/ait-web/dist ait serve
```

The pages are served at `/ui/`.

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

Tools: `status`, `diff`, `log`, `permissions`, `check_paths`, `commit`, `restore`, `propose`, `push`, `fetch`, `pull`, `branch`, `checkout`, `merge`, `pr`, `review`, `plan`, `owners`, `claim`, `checks`, and `ai_review`. Record a work intent with `claim` before a larger change, report local test results with `checks`, and ask for a second opinion with `ai_review`. Call `check_paths` and `owners` before writing. When `commit` refuses a path outside the write set, `restore` takes it back out of the index. While a merge is in progress, `commit` checks only your own changes on top of the merged branch.

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
