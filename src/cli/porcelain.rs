//! 与 Git 对齐的日常命令：提交、查看历史、撤销与改写、储藏、标签、远端同步和对象维护。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::Subcommand;

use super::open_repo;
use crate::git::{
    render_graph, unified_diff, ChangeKind, CleanOptions, Commit, DiffSide, FileDiff, GrepOptions,
    LogFilter, Object, Oid, Repository, ResetMode, RestoreSource, SeqOutcome, Signature,
    TagObject,
};
use crate::identity;
use crate::remote;

#[derive(Subcommand)]
pub enum GitCommand {
    /// 提交。`--amend` 改写上一个提交
    Commit {
        #[arg(short, long)]
        message: Option<String>,
        #[arg(short = 'a', long)]
        all: bool,
        /// 改写当前提交：沿用原作者和父提交，不给 -m 时沿用原说明
        #[arg(long)]
        amend: bool,
        /// 与 --amend 一起使用时不改说明
        #[arg(long = "no-edit")]
        no_edit: bool,
        /// 不输出提交摘要
        #[arg(short, long)]
        quiet: bool,
    },
    /// 查看工作区状态和越权文件
    Status {
        #[arg(short, long)]
        short: bool,
    },
    /// 查看差异：默认工作区对暂存区；`--cached` 暂存区对 HEAD；也可给一个或两个版本、A..B、A...B
    Diff {
        #[arg(long, visible_alias = "staged")]
        cached: bool,
        #[arg(long)]
        stat: bool,
        #[arg(long = "name-only")]
        name_only: bool,
        #[arg(long = "name-status")]
        name_status: bool,
        revs: Vec<String>,
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// 查看提交历史
    Log {
        #[arg(short = 'n', long = "max-count")]
        max_count: Option<usize>,
        #[arg(long)]
        skip: Option<usize>,
        #[arg(long)]
        oneline: bool,
        #[arg(long)]
        graph: bool,
        /// 从所有引用出发
        #[arg(long)]
        all: bool,
        #[arg(short = 'p', long = "patch")]
        patch: bool,
        #[arg(long)]
        stat: bool,
        #[arg(long = "name-only")]
        name_only: bool,
        #[arg(long = "name-status")]
        name_status: bool,
        /// 作者（正则）
        #[arg(long)]
        author: Option<String>,
        /// 提交说明（正则）
        #[arg(long)]
        grep: Option<String>,
        #[arg(short = 'i', long = "regexp-ignore-case")]
        ignore_case: bool,
        #[arg(long, visible_alias = "after")]
        since: Option<String>,
        #[arg(long, visible_alias = "before")]
        until: Option<String>,
        #[arg(long = "first-parent")]
        first_parent: bool,
        #[arg(long = "no-merges")]
        no_merges: bool,
        #[arg(long)]
        merges: bool,
        #[arg(long)]
        reverse: bool,
        #[arg(long = "topo-order")]
        topo_order: bool,
        /// oneline、short、medium、full，或 format:<模板>（%H %h %s %an %ae %ad %ar %d %b 等）
        #[arg(long, visible_alias = "pretty")]
        format: Option<String>,
        #[arg(long)]
        decorate: bool,
        revs: Vec<String>,
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// 显示提交、标签、目录或文件内容
    Show {
        objects: Vec<String>,
        #[arg(long)]
        stat: bool,
        #[arg(long = "name-only")]
        name_only: bool,
        #[arg(short = 's', long = "no-patch")]
        no_patch: bool,
        #[arg(long, visible_alias = "pretty")]
        format: Option<String>,
        #[arg(long)]
        oneline: bool,
    },
    /// 把分支移到指定提交。`--soft` 只动分支，默认同时重置暂存区，`--hard` 连工作区一起重置。
    /// 给路径时只把这些路径的暂存区还原到指定版本
    Reset {
        #[arg(long)]
        soft: bool,
        #[arg(long)]
        mixed: bool,
        #[arg(long)]
        hard: bool,
        args: Vec<String>,
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// 还原文件。默认用暂存区还原工作区；`--staged` 用 HEAD 还原暂存区；`--source` 指定来源版本
    Restore {
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(short = 'S', long)]
        staged: bool,
        #[arg(short = 'W', long)]
        worktree: bool,
        #[arg(short = 's', long)]
        source: Option<String>,
    },
    /// 生成撤销指定提交的新提交
    Revert {
        commits: Vec<String>,
        #[arg(short = 'm', long)]
        mainline: Option<usize>,
        #[arg(short = 'n', long = "no-commit")]
        no_commit: bool,
        /// 与 Git 兼容；ait 总是直接使用默认说明
        #[arg(long = "no-edit")]
        _no_edit: bool,
        #[arg(long = "continue")]
        cont: bool,
        #[arg(long)]
        abort: bool,
        #[arg(long)]
        skip: bool,
    },
    /// 把指定提交的改动应用到当前分支
    #[command(name = "cherry-pick")]
    CherryPick {
        commits: Vec<String>,
        #[arg(short = 'm', long)]
        mainline: Option<usize>,
        #[arg(short = 'n', long = "no-commit")]
        no_commit: bool,
        /// 在说明末尾记下来源提交
        #[arg(short = 'x')]
        record_origin: bool,
        #[arg(long = "no-edit")]
        _no_edit: bool,
        #[arg(long = "continue")]
        cont: bool,
        #[arg(long)]
        abort: bool,
        #[arg(long)]
        skip: bool,
    },
    /// 把当前分支的提交重放到另一个基点上
    Rebase {
        upstream: Option<String>,
        branch: Option<String>,
        #[arg(long)]
        onto: Option<String>,
        /// 用编辑器调整执行清单（pick/reword/edit/squash/fixup/drop/exec/break）
        #[arg(short = 'i', long)]
        interactive: bool,
        /// 直接使用文件里的执行清单（适合 agent 调用）
        #[arg(long)]
        todo: Option<PathBuf>,
        #[arg(long = "continue")]
        cont: bool,
        #[arg(long)]
        abort: bool,
        #[arg(long)]
        skip: bool,
    },
    /// 储藏工作区和暂存区的修改
    Stash {
        #[command(subcommand)]
        command: Option<StashCommand>,
    },
    /// 删除文件并从暂存区移除
    Rm {
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(long)]
        cached: bool,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 'r')]
        recursive: bool,
    },
    /// 移动或改名已跟踪的文件
    Mv {
        from: String,
        to: String,
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// 删除未跟踪的文件。必须给 -n（只列出）或 -f（真正删除）
    Clean {
        #[arg(short = 'n', long = "dry-run")]
        dry_run: bool,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 'd')]
        dirs: bool,
        /// 连被忽略的文件一起删除
        #[arg(short = 'x')]
        ignored: bool,
    },
    /// 切换分支或提交；`checkout [<版本>] -- <路径>` 还原文件
    Checkout {
        target: Option<String>,
        start: Option<String>,
        #[arg(short = 'b')]
        create: bool,
        #[arg(long)]
        detach: bool,
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// 切换分支
    Switch {
        target: String,
        start: Option<String>,
        #[arg(short = 'c', long)]
        create: bool,
        #[arg(long)]
        detach: bool,
    },
    /// 列出、创建、删除、改名分支，或设置上游
    Branch {
        name: Option<String>,
        start: Option<String>,
        #[arg(short = 'd', long)]
        delete: bool,
        /// 强制删除未合并的分支
        #[arg(short = 'D')]
        force_delete: bool,
        #[arg(short = 'm', long = "move")]
        rename: bool,
        #[arg(short = 'a', long)]
        all: bool,
        #[arg(short = 'r', long)]
        remotes: bool,
        #[arg(short = 'v', long)]
        verbose: bool,
        #[arg(short = 'u', long = "set-upstream-to")]
        upstream: Option<String>,
        #[arg(long = "unset-upstream")]
        unset_upstream: bool,
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// 列出、创建或删除标签。给 -m 或 -a 时创建附注标签
    Tag {
        name: Option<String>,
        target: Option<String>,
        #[arg(short = 'a', long)]
        annotate: bool,
        #[arg(short = 'm', long)]
        message: Option<String>,
        #[arg(short = 'd', long)]
        delete: bool,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 'l', long)]
        list: bool,
        #[arg(short = 'n')]
        lines: Option<usize>,
    },
    /// 用最近的标签描述提交
    Describe {
        rev: Option<String>,
        #[arg(long)]
        tags: bool,
        #[arg(long)]
        always: bool,
        #[arg(long)]
        dirty: bool,
    },
    /// 逐行显示最后修改者
    Blame {
        args: Vec<String>,
        #[arg(short = 'L')]
        lines: Option<String>,
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// 在已跟踪文件（或指定版本）里搜索
    Grep {
        pattern: String,
        revs: Vec<String>,
        #[arg(short = 'i', long = "ignore-case")]
        ignore_case: bool,
        #[arg(short = 'n', long = "line-number")]
        line_number: bool,
        #[arg(short = 'l', long = "files-with-matches")]
        files: bool,
        #[arg(short = 'c', long)]
        count: bool,
        #[arg(short = 'w', long = "word-regexp")]
        word: bool,
        #[arg(short = 'F', long = "fixed-strings")]
        fixed: bool,
        #[arg(short = 'E', long = "extended-regexp")]
        extended: bool,
        #[arg(short = 'v', long = "invert-match")]
        invert: bool,
        #[arg(last = true)]
        paths: Vec<String>,
    },
    /// 按作者汇总提交
    Shortlog {
        revs: Vec<String>,
        #[arg(short = 's', long)]
        summary: bool,
        #[arg(short = 'n', long)]
        numbered: bool,
        #[arg(short = 'e', long)]
        email: bool,
    },
    /// 显示引用的移动记录
    Reflog {
        args: Vec<String>,
        #[arg(short = 'n', long = "max-count")]
        max_count: Option<usize>,
    },
    /// 把对象打成 pack，清理 loose 对象并打包引用
    Gc {
        /// `now` 时立即删除不可达对象
        #[arg(long)]
        prune: Option<String>,
        #[arg(long)]
        aggressive: bool,
    },
    /// 打包引用到 packed-refs
    #[command(name = "pack-refs")]
    PackRefs {
        #[arg(long)]
        all: bool,
    },
    /// 校验对象完整性与可达性
    Fsck,
    /// 统计对象数量
    #[command(name = "count-objects")]
    CountObjects {
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// 查看对象
    #[command(name = "cat-file")]
    CatFile {
        #[arg(short = 't')]
        kind: bool,
        #[arg(short = 's')]
        size: bool,
        #[arg(short = 'p')]
        pretty: bool,
        #[arg(short = 'e')]
        exists: bool,
        object: String,
    },
    /// 列出暂存区里的文件
    #[command(name = "ls-files")]
    LsFiles {
        #[arg(short = 's', long)]
        stage: bool,
        #[arg(short = 'u', long)]
        unmerged: bool,
        #[arg(short = 'o', long)]
        others: bool,
    },
    /// 列出目录树
    #[command(name = "ls-tree")]
    LsTree {
        tree: String,
        #[arg(short = 'r')]
        recursive: bool,
        #[arg(long = "name-only")]
        name_only: bool,
    },
    /// 求合并基
    #[command(name = "merge-base")]
    MergeBase {
        revs: Vec<String>,
        #[arg(long = "is-ancestor")]
        is_ancestor: bool,
        #[arg(long)]
        all: bool,
    },
    /// 解析版本表达式（支持 ~n、^n、@{n}、@{-n}、@{u}、^{commit}、rev:path 等）
    #[command(name = "rev-parse")]
    RevParse {
        #[arg(allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// 推送。默认推当前分支到它的上游远端；越权修改会变成评审
    Push {
        remote: Option<String>,
        refspecs: Vec<String>,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 'u', long = "set-upstream")]
        set_upstream: bool,
        #[arg(long)]
        tags: bool,
        #[arg(short = 'd', long)]
        delete: bool,
        /// 只输出失败的引用
        #[arg(short, long)]
        quiet: bool,
    },
    /// 拉取并合并（或变基），同时更新权限清单
    Pull {
        remote: Option<String>,
        branch: Option<String>,
        #[arg(short = 'r', long)]
        rebase: bool,
        #[arg(long = "no-rebase")]
        no_rebase: bool,
        #[arg(long = "ff-only")]
        ff_only: bool,
        #[arg(short, long)]
        quiet: bool,
    },
    /// 下载远端对象、分支、标签和权限清单
    Fetch {
        remote: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(short = 'p', long)]
        prune: bool,
        #[arg(short = 't', long)]
        tags: bool,
        #[arg(short, long)]
        quiet: bool,
    },
    /// 管理远端
    Remote {
        #[command(subcommand)]
        command: Option<RemoteCommand>,
        #[arg(short = 'v', long)]
        verbose: bool,
    },
}

#[derive(Subcommand)]
pub enum StashCommand {
    /// 储藏当前修改（默认动作）
    Push {
        #[arg(short = 'm', long)]
        message: Option<String>,
        #[arg(short = 'u', long = "include-untracked")]
        untracked: bool,
        #[arg(short = 'k', long = "keep-index")]
        keep_index: bool,
    },
    List,
    Show {
        stash: Option<String>,
        #[arg(short = 'p', long)]
        patch: bool,
    },
    Apply {
        stash: Option<String>,
        #[arg(long)]
        index: bool,
    },
    Pop {
        stash: Option<String>,
        #[arg(long)]
        index: bool,
    },
    Drop {
        stash: Option<String>,
    },
    Clear,
}

#[derive(Subcommand)]
pub enum RemoteCommand {
    /// 添加远端
    Add { name: String, url: String },
    /// 删除远端及其远端分支
    #[command(visible_alias = "rm")]
    Remove { name: String },
    Rename { old: String, new: String },
    #[command(name = "set-url")]
    SetUrl { name: String, url: String },
    #[command(name = "get-url")]
    GetUrl { name: String },
    /// 列出远端
    List,
}

fn me() -> Signature {
    let (name, email) = identity::load_identity();
    identity::signature_now(&name, &email)
}

pub fn run(command: GitCommand) -> anyhow::Result<()> {
    match command {
        GitCommand::Commit {
            message,
            all,
            amend,
            no_edit,
            quiet,
        } => {
            let repo = open_repo()?;
            let (name, email) = identity::load_identity();
            if name == "Ait" && email == "ait@local" {
                eprintln!("提示: 正在使用默认身份 Ait <ait@local>，可用 ait config user.name 修改");
            }
            let text = match (&message, amend, no_edit) {
                (Some(text), _, _) => text.clone(),
                (None, true, _) => String::new(),
                (None, false, _) => {
                    let pending = std::fs::read_to_string(repo.git_dir().join("MERGE_MSG")).unwrap_or_default();
                    if pending.trim().is_empty() {
                        match repo.run_editor("\n# 请输入提交说明，# 开头的行会被忽略\n", false, "COMMIT_EDITMSG") {
                            Ok(text) => text,
                            Err(_) => bail!("请用 -m 给出提交说明"),
                        }
                    } else {
                        String::new()
                    }
                }
            };
            let oid = repo.commit_with(&text, all, &me(), amend)?;
            if quiet {
                return Ok(());
            }
            let branch = repo.branch_name()?.unwrap_or_else(|| "HEAD".into());
            let summary = repo.read_commit(oid)?.summary().to_string();
            println!("[{branch} {}] {summary}", oid.short());
        }
        GitCommand::Status { short } => {
            let repo = open_repo()?;
            if short {
                print_short_status(&repo)?;
            } else {
                super::print_status(&repo)?;
            }
        }
        GitCommand::Diff {
            cached,
            stat,
            name_only,
            name_status,
            revs,
            paths,
        } => {
            let repo = open_repo()?;
            let (old, new) = diff_sides(&repo, cached, &revs)?;
            let files = repo.diff_sides(old, new, &paths)?;
            print_diffs(&files, stat, name_only, name_status, true);
        }
        GitCommand::Log {
            max_count,
            skip,
            oneline,
            graph,
            all,
            patch,
            stat,
            name_only,
            name_status,
            author,
            grep,
            ignore_case,
            since,
            until,
            first_parent,
            no_merges,
            merges,
            reverse,
            topo_order,
            format,
            decorate,
            revs,
            paths,
        } => {
            let repo = open_repo()?;
            let mut filter = LogFilter {
                author,
                grep,
                ignore_case,
                since: since.as_deref().map(parse_date).transpose()?,
                until: until.as_deref().map(parse_date).transpose()?,
                max_count,
                skip: skip.unwrap_or(0),
                first_parent,
                no_merges,
                merges,
                topo: topo_order || graph,
                reverse,
                ..LogFilter::default()
            };
            let (revs, extra_paths) = split_revs_paths(&repo, &revs);
            filter.paths = resolve_paths(&repo, &[paths, extra_paths].concat())?;
            fill_range(&repo, &revs, all, &mut filter)?;
            if filter.include.is_empty() {
                println!("还没有提交");
                return Ok(());
            }
            let commits = repo.walk(&filter)?;
            let style = if oneline {
                Style::Oneline
            } else {
                Style::parse(format.as_deref())
            };
            let decorations = if decorate || oneline || graph || format.as_deref().is_some_and(|f| f.contains("%d") || f.contains("%D")) {
                decorations(&repo)?
            } else {
                HashMap::new()
            };
            let rows = if graph {
                let shape: Vec<(Oid, Vec<Oid>)> =
                    commits.iter().map(|(oid, c)| (*oid, c.parents.clone())).collect();
                Some(render_graph(&shape))
            } else {
                None
            };
            let detail = Detail { patch, stat, name_only, name_status };
            for (index, (oid, commit)) in commits.iter().enumerate() {
                let text = render_commit(&repo, *oid, commit, &style, &decorations, decorate)?;
                let body = render_detail(&repo, commit, *oid, &detail)?;
                match &rows {
                    Some(rows) => {
                        let row = &rows[index];
                        let mut lines = text.lines();
                        if let Some(first) = lines.next() {
                            println!("{}{first}", row.commit);
                        }
                        for line in lines.chain(body.lines()) {
                            println!("{}{line}", row.body);
                        }
                        for line in &row.after {
                            println!("{line}");
                        }
                    }
                    None => {
                        print!("{text}");
                        if !text.ends_with('\n') {
                            println!();
                        }
                        if !body.is_empty() {
                            print!("{body}");
                        }
                        if matches!(style, Style::Medium | Style::Short | Style::Full) && index + 1 < commits.len() {
                            println!();
                        }
                    }
                }
            }
        }
        GitCommand::Show {
            objects,
            stat,
            name_only,
            no_patch,
            format,
            oneline,
        } => {
            let repo = open_repo()?;
            let objects = if objects.is_empty() {
                vec!["HEAD".to_string()]
            } else {
                objects
            };
            let style = if oneline {
                Style::Oneline
            } else {
                Style::parse(format.as_deref())
            };
            let decorations = decorations(&repo)?;
            for spec in objects {
                let oid = repo.rev_parse_object(&spec)?;
                show_object(&repo, oid, &spec, &style, &decorations, stat, name_only, no_patch)?;
            }
        }
        GitCommand::Reset {
            soft,
            mixed,
            hard,
            args,
            paths,
        } => {
            let repo = open_repo()?;
            let mut rev = None;
            let mut paths = paths;
            for (index, arg) in args.iter().enumerate() {
                if index == 0 && repo.rev_parse(arg).is_ok() {
                    rev = Some(arg.clone());
                } else {
                    paths.push(arg.clone());
                }
            }
            if !paths.is_empty() {
                if soft || hard {
                    bail!("按路径重置时不能用 --soft 或 --hard");
                }
                let source = match &rev {
                    Some(spec) => RestoreSource::Commit(repo.rev_parse(spec)?),
                    None => RestoreSource::Head,
                };
                let done = repo.restore_from(&paths, source, true, false)?;
                println!("已重置暂存区: {}", done.join(", "));
                return Ok(());
            }
            let mode = match (soft, mixed, hard) {
                (true, false, false) => ResetMode::Soft,
                (false, _, false) => ResetMode::Mixed,
                (false, false, true) => ResetMode::Hard,
                _ => bail!("--soft、--mixed、--hard 只能选一个"),
            };
            let label = rev.unwrap_or_else(|| "HEAD".into());
            let target = repo.rev_parse(&label)?;
            repo.reset(target, mode, &label)?;
            let summary = repo.read_commit(target)?.summary().to_string();
            println!("HEAD 现在位于 {} {summary}", target.short());
        }
        GitCommand::Restore {
            paths,
            staged,
            worktree,
            source,
        } => {
            let repo = open_repo()?;
            let worktree = worktree || !staged;
            let source = match source {
                Some(spec) => RestoreSource::Commit(repo.rev_parse(&spec)?),
                None if staged => RestoreSource::Head,
                None => RestoreSource::Index,
            };
            let done = repo.restore_from(&paths, source, staged, worktree)?;
            println!("已还原 {}", done.join(", "));
        }
        GitCommand::Revert {
            commits,
            mainline,
            no_commit,
            cont,
            abort,
            skip,
            ..
        } => {
            let repo = open_repo()?;
            if let Some(outcome) = sequence_control(&repo, cont, abort, skip)? {
                return report(outcome);
            }
            let oids = expand_commits(&repo, &commits)?;
            report(repo.revert(&oids, mainline, no_commit, &me())?)?;
        }
        GitCommand::CherryPick {
            commits,
            mainline,
            no_commit,
            record_origin,
            cont,
            abort,
            skip,
            ..
        } => {
            let repo = open_repo()?;
            if let Some(outcome) = sequence_control(&repo, cont, abort, skip)? {
                return report(outcome);
            }
            let oids = expand_commits(&repo, &commits)?;
            report(repo.cherry_pick(&oids, mainline, record_origin, no_commit, &me())?)?;
        }
        GitCommand::Rebase {
            upstream,
            branch,
            onto,
            interactive,
            todo,
            cont,
            abort,
            skip,
        } => {
            let repo = open_repo()?;
            if let Some(outcome) = sequence_control(&repo, cont, abort, skip)? {
                return report(outcome);
            }
            let upstream_oid = match &upstream {
                Some(spec) => repo.rev_parse(spec)?,
                None => {
                    let current = repo.branch_name()?.context("分离 HEAD 时请指定上游")?;
                    let refname = repo
                        .upstream_of(&current)?
                        .with_context(|| format!("分支 {current} 没有上游，请指定要变基到的版本"))?;
                    repo.resolve_ref(&refname)?
                        .with_context(|| format!("{refname} 还不存在，请先 ait fetch"))?
                }
            };
            let onto = onto.map(|spec| repo.rev_parse(&spec)).transpose()?;
            let todo_text = todo
                .map(|path| std::fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display())))
                .transpose()?;
            report(repo.rebase(
                upstream_oid,
                onto,
                branch.as_deref(),
                interactive,
                todo_text.as_deref(),
                &me(),
            )?)?;
        }
        GitCommand::Stash { command } => run_stash(command)?,
        GitCommand::Rm {
            paths,
            cached,
            force,
            recursive,
        } => {
            for path in open_repo()?.remove_paths(&paths, cached, force, recursive)? {
                println!("rm '{path}'");
            }
        }
        GitCommand::Mv { from, to, force } => {
            let (src, dst) = open_repo()?.move_path(&from, &to, force)?;
            println!("已移动 {src} -> {dst}");
        }
        GitCommand::Clean {
            dry_run,
            force,
            dirs,
            ignored,
        } => {
            if !dry_run && !force {
                bail!("为防误删，请加 -n 先查看，或 -f 真正删除");
            }
            let removed = open_repo()?.clean(&CleanOptions {
                dry_run: !force || dry_run,
                dirs,
                ignored,
            })?;
            let verb = if !force || dry_run { "将删除" } else { "已删除" };
            for path in removed {
                println!("{verb} {path}");
            }
        }
        GitCommand::Checkout {
            target,
            start,
            create,
            detach,
            paths,
        } => {
            let repo = open_repo()?;
            if !paths.is_empty() {
                let mut paths = paths;
                if let Some(extra) = start {
                    paths.insert(0, extra);
                }
                let done = match target {
                    Some(spec) => {
                        let oid = repo.rev_parse(&spec)?;
                        repo.restore_from(&paths, RestoreSource::Commit(oid), true, true)?
                    }
                    None => repo.restore_from(&paths, RestoreSource::Index, false, true)?,
                };
                println!("已还原 {}", done.join(", "));
                return Ok(());
            }
            let target = target.context("请指定分支、提交或 -- 路径")?;
            if detach {
                let oid = repo.rev_parse(&target)?;
                println!("{}", repo.checkout(&oid.to_hex(), false)?);
            } else {
                println!("{}", repo.checkout_from(&target, create, start.as_deref())?);
            }
        }
        GitCommand::Switch {
            target,
            start,
            create,
            detach,
        } => {
            let repo = open_repo()?;
            let target = if target == "-" {
                repo.previous_branch(1)?
            } else {
                target
            };
            if detach {
                let oid = repo.rev_parse(&target)?;
                println!("{}", repo.checkout(&oid.to_hex(), false)?);
            } else if create {
                println!("{}", repo.checkout_from(&target, true, start.as_deref())?);
            } else {
                if !crate::git::valid_branch(&target)
                    || (repo.resolve_ref(&format!("refs/heads/{target}"))?.is_none()
                        && repo.rev_parse(&target).is_ok()
                        && !repo.list_refs("refs/remotes/")?.iter().any(|(n, _)| n.ends_with(&format!("/{target}"))))
                {
                    bail!("{target} 不是分支；切到提交请加 --detach");
                }
                println!("{}", repo.checkout(&target, false)?);
            }
        }
        GitCommand::Branch {
            name,
            start,
            delete,
            force_delete,
            rename,
            all,
            remotes,
            verbose,
            upstream,
            unset_upstream,
            force,
        } => run_branch(
            name,
            start,
            delete || force_delete,
            force_delete,
            rename,
            all,
            remotes,
            verbose,
            upstream,
            unset_upstream,
            force,
        )?,
        GitCommand::Tag {
            name,
            target,
            annotate,
            message,
            delete,
            force,
            list,
            lines,
        } => {
            let repo = open_repo()?;
            if delete {
                let name = name.context("请指定要删除的标签")?;
                let oid = repo.delete_tag(&name)?;
                println!("已删除标签 '{name}'（曾是 {}）", oid.short());
                return Ok(());
            }
            match name {
                Some(pattern) if list => print_tags(&repo, Some(&pattern), lines)?,
                Some(name) => {
                    let spec = target.unwrap_or_else(|| "HEAD".into());
                    let oid = repo.rev_parse_object(&spec)?;
                    let message = match (message, annotate) {
                        (Some(text), _) => Some(text),
                        (None, true) => Some(repo.run_editor("\n# 请输入标签说明\n", false, "TAG_EDITMSG").context("附注标签需要 -m 说明")?),
                        (None, false) => None,
                    };
                    let created = repo.create_tag(&name, oid, message.as_deref(), &me(), force)?;
                    if message.is_some() {
                        println!("已创建附注标签 {name}（{}）", created.short());
                    } else {
                        println!("已创建标签 {name} -> {}", oid.short());
                    }
                }
                None => print_tags(&repo, None, lines)?,
            }
        }
        GitCommand::Describe {
            rev,
            tags,
            always,
            dirty,
        } => {
            let repo = open_repo()?;
            let oid = repo.rev_parse(rev.as_deref().unwrap_or("HEAD"))?;
            let mut text = repo.describe(oid, tags, always)?;
            if dirty && rev.is_none() {
                let status = repo.status()?;
                if !status.staged.is_empty() || !status.unstaged.is_empty() {
                    text.push_str("-dirty");
                }
            }
            println!("{text}");
        }
        GitCommand::Blame { args, lines, paths } => {
            let repo = open_repo()?;
            let (rev, path) = match (args.as_slice(), paths.as_slice()) {
                ([], [path]) => ("HEAD".to_string(), path.clone()),
                ([rev], [path]) => (rev.clone(), path.clone()),
                ([path], []) => ("HEAD".to_string(), path.clone()),
                ([rev, path], []) => (rev.clone(), path.clone()),
                _ => bail!("用法: ait blame [<版本>] [--] <文件>"),
            };
            let oid = repo.rev_parse(&rev)?;
            let rel = resolve_paths(&repo, &[path])?.remove(0);
            let range = lines.as_deref().map(parse_line_range).transpose()?;
            let blamed = repo.blame(oid, &rel)?;
            let width = blamed.iter().map(|l| l.author.chars().count()).max().unwrap_or(0);
            let digits = blamed.len().to_string().len();
            for line in blamed {
                if let Some((from, to)) = range {
                    if line.final_line < from || line.final_line > to {
                        continue;
                    }
                }
                let date = identity::format_time(line.time, &line.tz);
                let path_part = if line.path != rel {
                    format!(" {}", line.path)
                } else {
                    String::new()
                };
                let root = repo
                    .read_commit(line.oid)
                    .map(|commit| commit.parents.is_empty())
                    .unwrap_or(false);
                let id = if root {
                    format!("^{}", &line.oid.to_hex()[..7])
                } else {
                    line.oid.to_hex()[..8].to_string()
                };
                println!(
                    "{id}{path_part} ({:width$} {date} {:>digits$}) {}",
                    line.author,
                    line.final_line,
                    line.content,
                );
            }
        }
        GitCommand::Grep {
            pattern,
            revs,
            ignore_case,
            line_number,
            files,
            count,
            word,
            fixed,
            extended: _,
            invert,
            paths,
        } => {
            let repo = open_repo()?;
            let (revs, extra) = split_revs_paths(&repo, &revs);
            let paths = [paths, extra].concat();
            let sources: Vec<Option<(String, Oid)>> = if revs.is_empty() {
                vec![None]
            } else {
                revs.iter()
                    .map(|spec| repo.rev_parse(spec).map(|oid| Some((spec.clone(), oid))))
                    .collect::<anyhow::Result<_>>()?
            };
            let mut any = false;
            for source in sources {
                let matches = repo.grep(&GrepOptions {
                    pattern: pattern.clone(),
                    ignore_case,
                    word,
                    fixed,
                    invert,
                    source: source.as_ref().map(|(_, oid)| *oid),
                    paths: paths.clone(),
                })?;
                any |= !matches.is_empty();
                let prefix = source.as_ref().map(|(spec, _)| format!("{spec}:")).unwrap_or_default();
                if files || count {
                    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
                    for hit in &matches {
                        *counts.entry(hit.path.clone()).or_default() += 1;
                    }
                    for (path, n) in counts {
                        if count {
                            println!("{prefix}{path}:{n}");
                        } else {
                            println!("{prefix}{path}");
                        }
                    }
                } else {
                    for hit in matches {
                        if line_number {
                            println!("{prefix}{}:{}:{}", hit.path, hit.line_no, hit.line);
                        } else {
                            println!("{prefix}{}:{}", hit.path, hit.line);
                        }
                    }
                }
            }
            if !any {
                std::process::exit(1);
            }
        }
        GitCommand::Shortlog {
            revs,
            summary,
            numbered,
            email,
        } => {
            let repo = open_repo()?;
            let mut filter = LogFilter::default();
            fill_range(&repo, &revs, false, &mut filter)?;
            let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (_, commit) in repo.walk(&filter)? {
                let who = if email {
                    format!("{} <{}>", commit.author.name, commit.author.email)
                } else {
                    commit.author.name.clone()
                };
                groups.entry(who).or_default().push(commit.summary().to_string());
            }
            let mut list: Vec<(String, Vec<String>)> = groups.into_iter().collect();
            if numbered {
                list.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
            }
            for (who, mut subjects) in list {
                if summary {
                    println!("{:>6}\t{who}", subjects.len());
                } else {
                    println!("{who} ({}):", subjects.len());
                    subjects.reverse();
                    for subject in subjects {
                        println!("      {subject}");
                    }
                    println!();
                }
            }
        }
        GitCommand::Reflog { args, max_count } => {
            let repo = open_repo()?;
            let name = match args.as_slice() {
                [] => "HEAD".to_string(),
                [word] if word == "show" => "HEAD".to_string(),
                [word, name] if word == "show" => name.clone(),
                [name] => name.clone(),
                _ => bail!("用法: ait reflog [show] [<引用>]"),
            };
            let full = repo.reflog_name(&name)?;
            let label = if full == "HEAD" { "HEAD".to_string() } else { name.clone() };
            for (index, entry) in repo.reflog(&full)?.into_iter().enumerate() {
                if max_count.is_some_and(|max| index >= max) {
                    break;
                }
                println!("{} {label}@{{{index}}}: {}", entry.new.short(), entry.message);
            }
        }
        GitCommand::Gc { prune, aggressive: _ } => {
            let repo = open_repo()?;
            let report = repo.gc(prune.as_deref() == Some("now"))?;
            println!(
                "已打包 {} 个对象，删除 {} 个已打包的 loose 对象，清理 {} 个不可达对象，合并 {} 个旧 pack，打包 {} 个引用",
                report.packed, report.loose_removed, report.pruned, report.packs_removed, report.refs_packed
            );
        }
        GitCommand::PackRefs { all: _ } => {
            let count = open_repo()?.pack_refs()?;
            println!("已打包 {count} 个引用");
        }
        GitCommand::Fsck => {
            let repo = open_repo()?;
            let report = repo.fsck()?;
            for problem in &report.problems {
                println!("error: {problem}");
            }
            for oid in &report.dangling {
                println!("dangling commit {oid}");
            }
            println!("检查了 {} 个对象，发现 {} 个问题", report.checked, report.problems.len());
            if !report.problems.is_empty() {
                std::process::exit(1);
            }
        }
        GitCommand::CountObjects { verbose } => {
            let count = open_repo()?.count_objects()?;
            if verbose {
                println!("count: {}", count.loose);
                println!("size: {}", count.loose_bytes / 1024);
                println!("in-pack: {}", count.packed);
                println!("packs: {}", count.packs);
                println!("size-pack: {}", count.pack_bytes / 1024);
            } else {
                println!("{} objects, {} kilobytes", count.loose, count.loose_bytes / 1024);
            }
        }
        GitCommand::CatFile {
            kind,
            size,
            pretty,
            exists,
            object,
        } => {
            let repo = open_repo()?;
            let oid = match repo.rev_parse_object(&object) {
                Ok(oid) => oid,
                Err(err) if exists => {
                    let _ = err;
                    std::process::exit(1);
                }
                Err(err) => return Err(err),
            };
            if exists {
                if !repo.has_object(oid) {
                    std::process::exit(1);
                }
                return Ok(());
            }
            let (object_kind, payload) = repo.read_raw(oid)?;
            if kind {
                println!("{object_kind}");
            } else if size {
                println!("{}", payload.len());
            } else if pretty {
                match repo.read_object(oid)? {
                    Object::Tree(entries) => {
                        for entry in entries {
                            let kind = if entry.mode == crate::git::MODE_TREE { "tree" } else { "blob" };
                            println!("{:06o} {kind} {}\t{}", entry.mode, entry.oid, entry.name);
                        }
                    }
                    _ => {
                        use std::io::Write;
                        std::io::stdout().write_all(&payload)?;
                    }
                }
            } else {
                bail!("请指定 -t、-s、-p 或 -e");
            }
        }
        GitCommand::LsFiles {
            stage,
            unmerged,
            others,
        } => {
            let repo = open_repo()?;
            if others {
                for path in repo.status()?.untracked {
                    println!("{path}");
                }
                return Ok(());
            }
            for (path, mode, oid, entry_stage) in repo.index_entries()? {
                if unmerged && entry_stage == 0 {
                    continue;
                }
                if stage || unmerged {
                    println!("{mode:06o} {oid} {entry_stage}\t{path}");
                } else {
                    println!("{path}");
                }
            }
        }
        GitCommand::LsTree {
            tree,
            recursive,
            name_only,
        } => {
            let repo = open_repo()?;
            let mut oid = repo.rev_parse_object(&tree)?;
            oid = repo.peel_tags(oid)?;
            if let Object::Commit(commit) = repo.read_object(oid)? {
                oid = commit.tree;
            }
            for (mode, kind, child, path) in repo.ls_tree(oid, recursive)? {
                if name_only {
                    println!("{path}");
                } else {
                    println!("{mode:06o} {kind} {child}\t{path}");
                }
            }
        }
        GitCommand::MergeBase {
            revs,
            is_ancestor,
            all,
        } => {
            let repo = open_repo()?;
            if revs.len() != 2 {
                bail!("请给出两个版本");
            }
            let a = repo.rev_parse(&revs[0])?;
            let b = repo.rev_parse(&revs[1])?;
            if is_ancestor {
                if !repo.is_ancestor(a, b)? {
                    std::process::exit(1);
                }
                return Ok(());
            }
            let bases = repo.best_merge_bases(a, b)?;
            if bases.is_empty() {
                std::process::exit(1);
            }
            for base in bases.iter().take(if all { usize::MAX } else { 1 }) {
                println!("{base}");
            }
        }
        GitCommand::RevParse { args } => run_rev_parse(args)?,
        GitCommand::Push {
            remote,
            refspecs,
            force,
            set_upstream,
            tags,
            delete,
            quiet,
        } => {
            let repo = open_repo()?;
            let results = remote::push_refs(
                &repo,
                &remote::PushOptions {
                    remote,
                    refspecs,
                    force,
                    set_upstream,
                    tags,
                    delete,
                },
            )?;
            let mut failed = false;
            for result in &results {
                let rejected = result.status == "rejected";
                failed |= rejected;
                if quiet && !rejected {
                    continue;
                }
                println!("{}", result.line());
                if !result.blocked.is_empty() {
                    println!("  需要评审的路径:");
                    for path in &result.blocked {
                        println!("    {path}");
                    }
                }
            }
            if !quiet {
                if let Some(manifest) = results.iter().rev().find_map(|r| r.manifest.as_ref()) {
                    print!("{}", manifest.render());
                }
            }
            if failed {
                bail!("部分引用推送失败");
            }
        }
        GitCommand::Pull {
            remote,
            branch,
            rebase,
            no_rebase,
            ff_only,
            quiet,
        } => {
            let repo = open_repo()?;
            let rebase = rebase
                || (!no_rebase
                    && repo
                        .config_get("pull.rebase")?
                        .is_some_and(|v| v == "true"));
            let mode = if ff_only {
                remote::PullMode::FastForwardOnly
            } else if rebase {
                remote::PullMode::Rebase
            } else {
                remote::PullMode::Merge
            };
            let report = remote::pull_with(&repo, remote.as_deref(), branch.as_deref(), mode)?;
            if !quiet {
                println!("{}", report.message);
                print!("{}", report.manifest.render());
            }
        }
        GitCommand::Fetch {
            remote,
            all,
            prune,
            tags: _,
            quiet,
        } => {
            let repo = open_repo()?;
            let names = if all {
                remote::remote_names(&repo)?
            } else {
                vec![remote.unwrap_or_else(|| remote::default_remote(&repo))]
            };
            for name in names {
                let report = remote::fetch_remote(&repo, &name, prune)?;
                if quiet {
                    continue;
                }
                println!("从 {name} 获取完成");
                for line in &report.updated {
                    println!("  {line}");
                }
                for line in &report.pruned {
                    println!("  - [已删除] {line}");
                }
                print!("{}", report.manifest.render());
            }
        }
        GitCommand::Remote { command, verbose } => {
            let repo = open_repo()?;
            match command {
                Some(RemoteCommand::Add { name, url }) => {
                    remote::add_remote(&repo, &name, &url)?;
                    println!("已添加远端 {name} {url}");
                }
                Some(RemoteCommand::Remove { name }) => {
                    remote::remove_remote(&repo, &name)?;
                    println!("已删除远端 {name}");
                }
                Some(RemoteCommand::Rename { old, new }) => {
                    remote::rename_remote(&repo, &old, &new)?;
                    println!("已把远端 {old} 改名为 {new}");
                }
                Some(RemoteCommand::SetUrl { name, url }) => {
                    if repo.config_get(&format!("remote.{name}.url"))?.is_none() {
                        bail!("没有远端 {name}");
                    }
                    repo.config_set(&format!("remote.{name}.url"), &url)?;
                    println!("{name}\t{url}");
                }
                Some(RemoteCommand::GetUrl { name }) => {
                    let url = repo
                        .config_get(&format!("remote.{name}.url"))?
                        .with_context(|| format!("没有远端 {name}"))?;
                    println!("{url}");
                }
                Some(RemoteCommand::List) | None => {
                    let names = remote::remote_names(&repo)?;
                    if names.is_empty() {
                        println!("没有远端");
                    }
                    for name in names {
                        if verbose {
                            let url = repo.config_get(&format!("remote.{name}.url"))?.unwrap_or_default();
                            println!("{name}\t{url} (fetch)");
                            println!("{name}\t{url} (push)");
                        } else {
                            println!("{name}");
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn run_stash(command: Option<StashCommand>) -> anyhow::Result<()> {
    let repo = open_repo()?;
    let command = command.unwrap_or(StashCommand::Push {
        message: None,
        untracked: false,
        keep_index: false,
    });
    match command {
        StashCommand::Push {
            message,
            untracked,
            keep_index,
        } => match repo.stash_push(message.as_deref(), untracked, keep_index, &me())? {
            Some(_) => {
                let entry = &repo.stash_list()?[0];
                println!("已储藏: {}", entry.message);
            }
            None => println!("没有要储藏的本地修改"),
        },
        StashCommand::List => {
            for entry in repo.stash_list()? {
                println!("stash@{{{}}}: {}", entry.index, entry.message);
            }
        }
        StashCommand::Show { stash, patch } => {
            let files = repo.stash_show(stash_index(stash.as_deref())?)?;
            print_diffs(&files, !patch, false, false, false);
        }
        StashCommand::Apply { stash, index } => {
            let n = stash_index(stash.as_deref())?;
            let conflicts = repo.stash_apply(n, index)?;
            if conflicts.is_empty() {
                println!("已应用 stash@{{{n}}}");
            } else {
                print_conflicts(&conflicts);
                bail!("应用储藏时有冲突，解决后 ait add");
            }
        }
        StashCommand::Pop { stash, index } => {
            let n = stash_index(stash.as_deref())?;
            let conflicts = repo.stash_apply(n, index)?;
            if conflicts.is_empty() {
                let oid = repo.stash_drop(n)?;
                println!("已应用并丢弃 stash@{{{n}}}（{}）", oid.short());
            } else {
                print_conflicts(&conflicts);
                bail!("应用储藏时有冲突，储藏已保留；解决后 ait add，再 ait stash drop");
            }
        }
        StashCommand::Drop { stash } => {
            let n = stash_index(stash.as_deref())?;
            let oid = repo.stash_drop(n)?;
            println!("已丢弃 stash@{{{n}}}（{}）", oid.short());
        }
        StashCommand::Clear => {
            repo.stash_clear()?;
            println!("已清空储藏");
        }
    }
    Ok(())
}

fn stash_index(spec: Option<&str>) -> anyhow::Result<usize> {
    let Some(spec) = spec else { return Ok(0) };
    let inner = spec
        .strip_prefix("stash@{")
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or(spec);
    inner.parse().with_context(|| format!("无法识别储藏 {spec}"))
}

#[allow(clippy::too_many_arguments)]
fn run_branch(
    name: Option<String>,
    start: Option<String>,
    delete: bool,
    force_delete: bool,
    rename: bool,
    all: bool,
    remotes: bool,
    verbose: bool,
    upstream: Option<String>,
    unset_upstream: bool,
    force: bool,
) -> anyhow::Result<()> {
    let repo = open_repo()?;
    if let Some(upstream) = upstream {
        let branch = match name {
            Some(name) => name,
            None => repo.branch_name()?.context("分离 HEAD 不能设置上游")?,
        };
        let (remote_name, short) = upstream
            .split_once('/')
            .filter(|(r, _)| repo.config_get(&format!("remote.{r}.url")).ok().flatten().is_some())
            .map(|(r, b)| (r.to_string(), b.to_string()))
            .unwrap_or_else(|| (".".to_string(), upstream.clone()));
        repo.config_set(&format!("branch.{branch}.remote"), &remote_name)?;
        repo.config_set(&format!("branch.{branch}.merge"), &format!("refs/heads/{short}"))?;
        println!("分支 {branch} 已跟踪 {upstream}");
        return Ok(());
    }
    if unset_upstream {
        let branch = match name {
            Some(name) => name,
            None => repo.branch_name()?.context("分离 HEAD 没有上游")?,
        };
        repo.config_unset_section_key(&format!("branch.{branch}.remote"))?;
        repo.config_unset_section_key(&format!("branch.{branch}.merge"))?;
        println!("已取消 {branch} 的上游");
        return Ok(());
    }
    if delete {
        let name = name.context("请指定要删除的分支")?;
        let oid = repo
            .resolve_ref(&format!("refs/heads/{name}"))?
            .with_context(|| format!("分支不存在: {name}"))?;
        if !force_delete {
            let target = match repo.upstream_of(&name)? {
                Some(up) => repo.resolve_ref(&up)?,
                None => repo.head_oid()?,
            };
            if let Some(target) = target {
                if !repo.is_ancestor(oid, target)? {
                    bail!("分支 {name} 还没有合并，确认要删除请用 -D");
                }
            }
        }
        repo.delete_branch(&name)?;
        println!("已删除分支 {name}（曾是 {}）", oid.short());
        return Ok(());
    }
    if rename {
        let (old, new) = match (name, start) {
            (Some(old), Some(new)) => (old, new),
            (Some(new), None) => (repo.branch_name()?.context("分离 HEAD 不能改名")?, new),
            _ => bail!("用法: ait branch -m [<旧名>] <新名>"),
        };
        repo.rename_branch(&old, &new, force)?;
        println!("已把分支 {old} 改名为 {new}");
        return Ok(());
    }
    if let Some(name) = name {
        let oid = match &start {
            Some(spec) => repo.rev_parse(spec)?,
            None => repo.head_oid()?.context("还没有提交，不能创建分支")?,
        };
        let refname = format!("refs/heads/{name}");
        if force && repo.resolve_ref(&refname)?.is_some() {
            if repo.branch_name()?.as_deref() == Some(name.as_str()) {
                bail!("不能强制移动当前所在分支");
            }
            repo.set_ref_logged(&refname, oid, &format!("branch: Reset to {}", start.as_deref().unwrap_or("HEAD")))?;
        } else {
            repo.create_branch(&name, oid)?;
        }
        if let Some(spec) = &start {
            if let Some((remote_name, branch)) = spec.split_once('/') {
                if repo.resolve_ref(&format!("refs/remotes/{spec}"))?.is_some() {
                    repo.config_set(&format!("branch.{name}.remote"), remote_name)?;
                    repo.config_set(&format!("branch.{name}.merge"), &format!("refs/heads/{branch}"))?;
                }
            }
        }
        println!("已创建分支 {name}");
        return Ok(());
    }
    let current = repo.branch_name()?;
    let mut rows = Vec::new();
    if !remotes {
        for branch in repo.branches()? {
            rows.push((branch.current, branch.name.clone(), branch.oid, Some(branch.name)));
        }
    }
    if remotes || all {
        for (refname, oid) in repo.list_refs("refs/remotes/")? {
            let short = refname.trim_start_matches("refs/remotes/").to_string();
            let label = if all { format!("remotes/{short}") } else { short };
            rows.push((false, label, oid, None));
        }
    }
    if current.is_none() {
        if let Some(head) = repo.head_oid()? {
            println!("* (HEAD 分离于 {})", head.short());
        }
    }
    let width = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
    for (is_current, label, oid, local) in rows {
        let mark = if is_current { "* " } else { "  " };
        if verbose {
            let summary = repo.read_commit(oid).map(|c| c.summary().to_string()).unwrap_or_default();
            let track = match local.as_deref().map(|b| repo.upstream_of(b)) {
                Some(Ok(Some(up))) => tracking_note(&repo, oid, &up)?,
                _ => String::new(),
            };
            println!("{mark}{label:width$} {} {track}{summary}", oid.short());
        } else {
            println!("{mark}{label}");
        }
    }
    Ok(())
}

fn tracking_note(repo: &Repository, local: Oid, upstream: &str) -> anyhow::Result<String> {
    let short = upstream.trim_start_matches("refs/remotes/").trim_start_matches("refs/heads/");
    let Some(remote_oid) = repo.resolve_ref(upstream)? else {
        return Ok(format!("[{short}: 不存在] "));
    };
    let (ahead, behind) = ahead_behind(repo, local, remote_oid)?;
    Ok(match (ahead, behind) {
        (0, 0) => format!("[{short}] "),
        (a, 0) => format!("[{short}: 领先 {a}] "),
        (0, b) => format!("[{short}: 落后 {b}] "),
        (a, b) => format!("[{short}: 领先 {a}，落后 {b}] "),
    })
}

pub fn ahead_behind(repo: &Repository, local: Oid, remote: Oid) -> anyhow::Result<(usize, usize)> {
    let ahead = repo.walk(&LogFilter {
        include: vec![local],
        exclude: vec![remote],
        ..LogFilter::default()
    })?;
    let behind = repo.walk(&LogFilter {
        include: vec![remote],
        exclude: vec![local],
        ..LogFilter::default()
    })?;
    Ok((ahead.len(), behind.len()))
}

fn print_tags(repo: &Repository, pattern: Option<&str>, lines: Option<usize>) -> anyhow::Result<()> {
    for tag in repo.tags()? {
        if let Some(pattern) = pattern {
            if !glob_match(pattern, &tag.name) {
                continue;
            }
        }
        match lines {
            Some(n) => {
                let text = if tag.annotated {
                    tag.message.lines().take(n.max(1)).collect::<Vec<_>>().join("\n    ")
                } else {
                    tag.commit
                        .and_then(|c| repo.read_commit(c).ok())
                        .map(|c| c.summary().to_string())
                        .unwrap_or_default()
                };
                println!("{:15} {text}", tag.name);
            }
            None => println!("{}", tag.name),
        }
    }
    Ok(())
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let regex = format!(
        "^{}$",
        regex::escape(pattern).replace(r"\*", ".*").replace(r"\?", ".")
    );
    regex::Regex::new(&regex).map(|re| re.is_match(text)).unwrap_or(false)
}

fn sequence_control(
    repo: &Repository,
    cont: bool,
    abort: bool,
    skip: bool,
) -> anyhow::Result<Option<SeqOutcome>> {
    if abort {
        return Ok(Some(SeqOutcome::Done(repo.sequence_abort()?)));
    }
    if cont {
        return Ok(Some(repo.sequence_continue(&me())?));
    }
    if skip {
        return Ok(Some(repo.sequence_skip(&me())?));
    }
    Ok(None)
}

fn report(outcome: SeqOutcome) -> anyhow::Result<()> {
    match outcome {
        SeqOutcome::Done(message) => {
            println!("{message}");
            Ok(())
        }
        SeqOutcome::Stopped { message, conflicts } => {
            if conflicts.is_empty() {
                println!("{message}");
                Ok(())
            } else {
                print_conflicts(&conflicts);
                bail!("{message}")
            }
        }
    }
}

fn print_conflicts(paths: &[String]) {
    println!("有冲突:");
    for path in paths {
        println!("  {path}");
    }
}

/// 把 `A..B` 展开成提交列表（从旧到新），其余参数逐个解析。
fn expand_commits(repo: &Repository, specs: &[String]) -> anyhow::Result<Vec<Oid>> {
    if specs.is_empty() {
        bail!("请指定提交");
    }
    let mut out = Vec::new();
    for spec in specs {
        if spec.contains("..") {
            let mut filter = LogFilter {
                reverse: true,
                no_merges: true,
                ..LogFilter::default()
            };
            fill_range(repo, std::slice::from_ref(spec), false, &mut filter)?;
            out.extend(repo.walk(&filter)?.into_iter().map(|(oid, _)| oid));
        } else {
            out.push(repo.rev_parse(spec)?);
        }
    }
    Ok(out)
}

/// 把 `A`、`^A`、`A..B`、`A...B` 填进遍历的起点和排除集合。没有给版本时从 HEAD 开始。
fn fill_range(repo: &Repository, revs: &[String], all: bool, filter: &mut LogFilter) -> anyhow::Result<()> {
    for spec in revs {
        if let Some((left, right)) = spec.split_once("...") {
            let a = repo.rev_parse(if left.is_empty() { "HEAD" } else { left })?;
            let b = repo.rev_parse(if right.is_empty() { "HEAD" } else { right })?;
            filter.include.extend([a, b]);
            filter.exclude.extend(repo.best_merge_bases(a, b)?);
        } else if let Some((left, right)) = spec.split_once("..") {
            filter.exclude.push(repo.rev_parse(if left.is_empty() { "HEAD" } else { left })?);
            filter.include.push(repo.rev_parse(if right.is_empty() { "HEAD" } else { right })?);
        } else if let Some(rest) = spec.strip_prefix('^') {
            filter.exclude.push(repo.rev_parse(rest)?);
        } else {
            filter.include.push(repo.rev_parse(spec)?);
        }
    }
    if all {
        for (_, oid) in repo.list_refs("refs/")? {
            if let Ok(commit) = repo.peel_to_commit(oid) {
                filter.include.push(commit);
            }
        }
        if let Some(head) = repo.head_oid()? {
            filter.include.push(head);
        }
    }
    if filter.include.is_empty() {
        if let Some(head) = repo.head_oid()? {
            filter.include.push(head);
        }
    }
    Ok(())
}

/// `ait log foo` 里 foo 不是版本而是现存路径时当作路径。
fn split_revs_paths(repo: &Repository, args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut revs = Vec::new();
    let mut paths = Vec::new();
    for arg in args {
        let is_rev = arg.contains("..") || arg.starts_with('^') || repo.rev_parse(arg).is_ok();
        if is_rev && paths.is_empty() {
            revs.push(arg.clone());
        } else {
            paths.push(arg.clone());
        }
    }
    (revs, paths)
}

fn resolve_paths(repo: &Repository, paths: &[String]) -> anyhow::Result<Vec<String>> {
    let Some(work) = repo.work_tree() else {
        return Ok(paths.to_vec());
    };
    let work = std::fs::canonicalize(work).unwrap_or_else(|_| work.to_path_buf());
    let cwd = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let prefix = cwd
        .strip_prefix(&work)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    Ok(paths
        .iter()
        .map(|p| {
            let joined = if prefix.is_empty() || p.starts_with('/') {
                p.trim_start_matches("./").to_string()
            } else {
                format!("{prefix}/{}", p.trim_start_matches("./"))
            };
            normalize_rel(&joined)
        })
        .collect())
}

fn normalize_rel(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn diff_sides(repo: &Repository, cached: bool, revs: &[String]) -> anyhow::Result<(DiffSide, DiffSide)> {
    let head_or_empty = || -> anyhow::Result<DiffSide> {
        Ok(match repo.head_oid()? {
            Some(oid) => DiffSide::Commit(oid),
            None => DiffSide::Commit(empty_commit_tree(repo)?),
        })
    };
    match revs {
        [] if cached => Ok((head_or_empty()?, DiffSide::Index)),
        [] => Ok((DiffSide::Index, DiffSide::Worktree)),
        [one] if one.contains("...") => {
            let (left, right) = one.split_once("...").unwrap();
            let a = repo.rev_parse(if left.is_empty() { "HEAD" } else { left })?;
            let b = repo.rev_parse(if right.is_empty() { "HEAD" } else { right })?;
            let base = repo.merge_base(a, b)?.context("两个版本没有共同祖先")?;
            Ok((DiffSide::Commit(base), DiffSide::Commit(b)))
        }
        [one] if one.contains("..") => {
            let (left, right) = one.split_once("..").unwrap();
            let a = repo.rev_parse(if left.is_empty() { "HEAD" } else { left })?;
            let b = repo.rev_parse(if right.is_empty() { "HEAD" } else { right })?;
            Ok((DiffSide::Commit(a), DiffSide::Commit(b)))
        }
        [one] if cached => Ok((DiffSide::Commit(repo.rev_parse(one)?), DiffSide::Index)),
        [one] => Ok((DiffSide::Commit(repo.rev_parse(one)?), DiffSide::Worktree)),
        [a, b] => Ok((
            DiffSide::Commit(repo.rev_parse(a)?),
            DiffSide::Commit(repo.rev_parse(b)?),
        )),
        _ => bail!("最多给两个版本"),
    }
}

/// 还没有提交时用来比较的“空提交”：只写入一个空树提交对象，不移动任何引用。
fn empty_commit_tree(repo: &Repository) -> anyhow::Result<Oid> {
    let tree = repo.write_object("tree", &[])?;
    let who = Signature {
        name: "ait".into(),
        email: "ait@local".into(),
        time: 0,
        tz: "+0000".into(),
    };
    let commit = Commit {
        tree,
        parents: Vec::new(),
        author: who.clone(),
        committer: who,
        message: "empty\n".into(),
    };
    repo.write_object("commit", &commit.encode())
}

fn print_diffs(files: &[FileDiff], stat: bool, name_only: bool, name_status: bool, say_empty: bool) {
    if files.is_empty() {
        if say_empty {
            println!("没有差异");
        }
        return;
    }
    if name_only {
        for file in files {
            println!("{}", file.path);
        }
        return;
    }
    if name_status {
        for file in files {
            println!("{}\t{}", status_letter(file.kind), file.path);
        }
        return;
    }
    if stat {
        print!("{}", stat_text(files));
        return;
    }
    print!("{}", unified_diff(files));
}

fn status_letter(kind: ChangeKind) -> char {
    match kind {
        ChangeKind::Added => 'A',
        ChangeKind::Deleted => 'D',
        ChangeKind::Modified => 'M',
        ChangeKind::Unmerged => 'U',
    }
}

fn stat_text(files: &[FileDiff]) -> String {
    let mut rows = Vec::new();
    let (mut total_add, mut total_del) = (0usize, 0usize);
    for file in files {
        let (add, del) = count_lines(&file.patch);
        total_add += add;
        total_del += del;
        rows.push((file.path.clone(), add, del, file.binary));
    }
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let most = rows.iter().map(|r| r.1 + r.2).max().unwrap_or(0).max(1);
    let scale = |n: usize| if most <= 50 { n } else { (n * 50).div_ceil(most) };
    let mut out = String::new();
    for (path, add, del, binary) in rows {
        if binary {
            out.push_str(&format!(" {path:width$} | Bin\n"));
        } else {
            out.push_str(&format!(
                " {path:width$} | {:>4} {}{}\n",
                add + del,
                "+".repeat(scale(add)),
                "-".repeat(scale(del))
            ));
        }
    }
    out.push_str(&format!(
        " {} file{} changed, {total_add} insertion{}(+), {total_del} deletion{}(-)\n",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        if total_add == 1 { "" } else { "s" },
        if total_del == 1 { "" } else { "s" },
    ));
    out
}

fn count_lines(patch: &str) -> (usize, usize) {
    let mut add = 0;
    let mut del = 0;
    for line in patch.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            add += 1;
        } else if line.starts_with('-') {
            del += 1;
        }
    }
    (add, del)
}

enum Style {
    Oneline,
    Short,
    Medium,
    Full,
    Format(String),
}

impl Style {
    fn parse(format: Option<&str>) -> Style {
        match format {
            None | Some("medium") => Style::Medium,
            Some("oneline") => Style::Oneline,
            Some("short") => Style::Short,
            Some("full") | Some("fuller") => Style::Full,
            Some(other) => {
                let template = other
                    .strip_prefix("format:")
                    .or_else(|| other.strip_prefix("tformat:"))
                    .unwrap_or(other);
                Style::Format(template.to_string())
            }
        }
    }
}

struct Detail {
    patch: bool,
    stat: bool,
    name_only: bool,
    name_status: bool,
}

fn render_detail(repo: &Repository, commit: &Commit, oid: Oid, detail: &Detail) -> anyhow::Result<String> {
    if !(detail.patch || detail.stat || detail.name_only || detail.name_status) {
        return Ok(String::new());
    }
    if commit.parents.len() > 1 {
        return Ok(String::new());
    }
    let files = repo.diff_range(commit.parents.first().copied(), oid)?;
    let mut out = String::new();
    if detail.name_only {
        for file in &files {
            out.push_str(&format!("{}\n", file.path));
        }
    } else if detail.name_status {
        for file in &files {
            out.push_str(&format!("{}\t{}\n", status_letter(file.kind), file.path));
        }
    } else {
        out.push('\n');
        if detail.stat {
            out.push_str(&stat_text(&files));
        }
        if detail.patch {
            if detail.stat {
                out.push('\n');
            }
            out.push_str(&unified_diff(&files));
        }
    }
    Ok(out)
}

fn decorations(repo: &Repository) -> anyhow::Result<HashMap<Oid, Vec<String>>> {
    let mut map: HashMap<Oid, Vec<String>> = HashMap::new();
    let current = repo.branch_name()?;
    if let Some(head) = repo.head_oid()? {
        let label = match &current {
            Some(branch) => format!("HEAD -> {branch}"),
            None => "HEAD".to_string(),
        };
        map.entry(head).or_default().push(label);
    }
    for (name, oid) in repo.list_refs("refs/")? {
        let label = if let Some(short) = name.strip_prefix("refs/heads/") {
            if current.as_deref() == Some(short) {
                continue;
            }
            short.to_string()
        } else if let Some(short) = name.strip_prefix("refs/remotes/") {
            short.to_string()
        } else if let Some(short) = name.strip_prefix("refs/tags/") {
            format!("tag: {short}")
        } else {
            continue;
        };
        let target = repo.peel_to_commit(oid).unwrap_or(oid);
        map.entry(target).or_default().push(label);
    }
    Ok(map)
}

fn render_commit(
    repo: &Repository,
    oid: Oid,
    commit: &Commit,
    style: &Style,
    decorations: &HashMap<Oid, Vec<String>>,
    decorate: bool,
) -> anyhow::Result<String> {
    let deco = decorations
        .get(&oid)
        .map(|names| format!(" ({})", names.join(", ")))
        .unwrap_or_default();
    let _ = repo;
    Ok(match style {
        Style::Oneline => format!("{}{deco} {}\n", oid.short(), commit.summary()),
        Style::Short | Style::Medium | Style::Full => {
            let mut out = format!("commit {oid}{}\n", if decorate { deco.as_str() } else { "" });
            if commit.parents.len() > 1 {
                let parents: Vec<String> = commit.parents.iter().map(|p| p.short()).collect();
                out.push_str(&format!("Merge: {}\n", parents.join(" ")));
            }
            out.push_str(&format!("Author: {} <{}>\n", commit.author.name, commit.author.email));
            if matches!(style, Style::Full) {
                out.push_str(&format!("Commit: {} <{}>\n", commit.committer.name, commit.committer.email));
            }
            if !matches!(style, Style::Short) {
                out.push_str(&format!(
                    "Date:   {}\n",
                    identity::format_time(commit.author.time, &commit.author.tz)
                ));
            }
            out.push('\n');
            if matches!(style, Style::Short) {
                out.push_str(&format!("    {}\n", commit.summary()));
            } else {
                for line in commit.message.trim_end().lines() {
                    out.push_str(&format!("    {line}\n"));
                }
            }
            out
        }
        Style::Format(template) => format!("{}\n", expand_format(template, oid, commit, &deco)),
    })
}

fn expand_format(template: &str, oid: Oid, commit: &Commit, deco: &str) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    let body = commit
        .message
        .split_once('\n')
        .map(|(_, rest)| rest.trim_start_matches('\n').trim_end().to_string())
        .unwrap_or_default();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let Some(code) = chars.next() else {
            out.push('%');
            break;
        };
        match code {
            '%' => out.push('%'),
            'n' => out.push('\n'),
            'H' => out.push_str(&oid.to_hex()),
            'h' => out.push_str(&oid.short()),
            'T' => out.push_str(&commit.tree.to_hex()),
            't' => out.push_str(&commit.tree.short()),
            'P' => out.push_str(&commit.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>().join(" ")),
            'p' => out.push_str(&commit.parents.iter().map(|p| p.short()).collect::<Vec<_>>().join(" ")),
            's' => out.push_str(commit.summary()),
            'b' => out.push_str(&body),
            'B' => out.push_str(commit.message.trim_end()),
            'd' => out.push_str(deco),
            'D' => out.push_str(deco.trim().trim_start_matches('(').trim_end_matches(')')),
            'a' | 'c' => {
                let who = if code == 'a' { &commit.author } else { &commit.committer };
                let Some(field) = chars.next() else { break };
                match field {
                    'n' => out.push_str(&who.name),
                    'e' => out.push_str(&who.email),
                    'd' | 'D' => out.push_str(&identity::format_time(who.time, &who.tz)),
                    't' => out.push_str(&who.time.to_string()),
                    'r' => out.push_str(&relative_time(who.time)),
                    'I' | 'i' => out.push_str(&identity::format_time(who.time, &who.tz)),
                    other => {
                        out.push('%');
                        out.push(code);
                        out.push(other);
                    }
                }
            }
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

fn relative_time(time: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let diff = (now - time).max(0);
    let (n, unit) = match diff {
        d if d < 90 => (d, "second"),
        d if d < 90 * 60 => (d / 60, "minute"),
        d if d < 36 * 3600 => (d / 3600, "hour"),
        d if d < 14 * 86400 => (d / 86400, "day"),
        d if d < 70 * 86400 => (d / (7 * 86400), "week"),
        d if d < 365 * 86400 => (d / (30 * 86400), "month"),
        d => (d / (365 * 86400), "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

#[allow(clippy::too_many_arguments)]
fn show_object(
    repo: &Repository,
    oid: Oid,
    spec: &str,
    style: &Style,
    decorations: &HashMap<Oid, Vec<String>>,
    stat: bool,
    name_only: bool,
    no_patch: bool,
) -> anyhow::Result<()> {
    match repo.read_object(oid)? {
        Object::Commit(commit) => {
            print!("{}", render_commit(repo, oid, &commit, style, decorations, true)?);
            if no_patch || commit.parents.len() > 1 {
                return Ok(());
            }
            let files = repo.diff_range(commit.parents.first().copied(), oid)?;
            if files.is_empty() {
                return Ok(());
            }
            if name_only {
                for file in &files {
                    println!("{}", file.path);
                }
            } else if stat {
                println!();
                print!("{}", stat_text(&files));
            } else {
                println!();
                print!("{}", unified_diff(&files));
            }
        }
        Object::Tag(payload) => {
            let tag = TagObject::parse(&payload)?;
            println!("tag {}", tag.name);
            if let Some(tagger) = &tag.tagger {
                println!("Tagger: {} <{}>", tagger.name, tagger.email);
                println!("Date:   {}", identity::format_time(tagger.time, &tagger.tz));
            }
            println!();
            println!("{}", tag.message.trim_end());
            println!();
            show_object(repo, tag.object, spec, style, decorations, stat, name_only, no_patch)?;
        }
        Object::Tree(entries) => {
            println!("tree {spec}\n");
            for entry in entries {
                let slash = if entry.mode == crate::git::MODE_TREE { "/" } else { "" };
                println!("{}{slash}", entry.name);
            }
        }
        Object::Blob(bytes) => {
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
        }
    }
    Ok(())
}

fn print_short_status(repo: &Repository) -> anyhow::Result<()> {
    let status = repo.status()?;
    let mut rows: BTreeMap<String, (char, char)> = BTreeMap::new();
    for entry in &status.staged {
        let letter = if entry.kind == ChangeKind::Unmerged { 'U' } else { status_letter(entry.kind) };
        rows.entry(entry.path.clone()).or_insert((' ', ' ')).0 = letter;
    }
    for entry in &status.unstaged {
        rows.entry(entry.path.clone()).or_insert((' ', ' ')).1 = status_letter(entry.kind);
    }
    for (path, (x, y)) in rows {
        println!("{x}{y} {path}");
    }
    for path in status.untracked {
        println!("?? {path}");
    }
    Ok(())
}

fn run_rev_parse(args: Vec<String>) -> anyhow::Result<()> {
    let repo = open_repo()?;
    let mut short = false;
    let mut abbrev_ref = false;
    let mut verify = false;
    for arg in args {
        match arg.as_str() {
            "--short" => short = true,
            "--abbrev-ref" => abbrev_ref = true,
            "--verify" | "-q" | "--quiet" => verify = true,
            "--git-dir" => println!("{}", repo.git_dir().display()),
            "--show-toplevel" => println!(
                "{}",
                repo.work_tree().context("裸仓库没有工作区")?.display()
            ),
            "--is-inside-work-tree" => println!("{}", repo.work_tree().is_some()),
            "--is-bare-repository" => println!("{}", repo.is_bare()),
            spec => {
                if abbrev_ref {
                    let name = if spec == "HEAD" || spec == "@" {
                        repo.branch_name()?.unwrap_or_else(|| "HEAD".into())
                    } else if let Some(base) = spec.strip_suffix("@{u}").or_else(|| spec.strip_suffix("@{upstream}")) {
                        let branch = if base.is_empty() {
                            repo.branch_name()?.context("分离 HEAD 没有上游")?
                        } else {
                            base.to_string()
                        };
                        repo.upstream_of(&branch)?
                            .with_context(|| format!("分支 {branch} 没有上游"))?
                            .trim_start_matches("refs/remotes/")
                            .trim_start_matches("refs/heads/")
                            .to_string()
                    } else {
                        spec.to_string()
                    };
                    println!("{name}");
                    continue;
                }
                let oid = match repo.rev_parse_object(spec) {
                    Ok(oid) => oid,
                    Err(err) if verify => {
                        let _ = err;
                        std::process::exit(1);
                    }
                    Err(err) => return Err(err),
                };
                if short {
                    println!("{}", oid.short());
                } else {
                    println!("{oid}");
                }
            }
        }
    }
    Ok(())
}

fn parse_line_range(text: &str) -> anyhow::Result<(usize, usize)> {
    let (from, to) = text.split_once(',').context("-L 需要 起始,结束")?;
    let from: usize = from.trim().parse()?;
    let to = to.trim();
    let to = if let Some(n) = to.strip_prefix('+') {
        from + n.parse::<usize>()?.saturating_sub(1)
    } else {
        to.parse()?
    };
    Ok((from, to))
}

/// 支持 `YYYY-MM-DD`、`YYYY-MM-DD HH:MM:SS`、Unix 时间戳、`yesterday`、`N days ago`、`N.weeks.ago` 等。
pub fn parse_date(text: &str) -> anyhow::Result<i64> {
    let text = text.trim();
    let now = chrono::Utc::now().timestamp();
    if let Ok(n) = text.parse::<i64>() {
        return Ok(n);
    }
    if text == "now" {
        return Ok(now);
    }
    if text == "yesterday" {
        return Ok(now - 86400);
    }
    let words: Vec<&str> = text.split(|c: char| c == '.' || c.is_whitespace()).filter(|w| !w.is_empty()).collect();
    if words.len() == 3 && words[2] == "ago" {
        let n: i64 = words[0].parse().context("无法识别日期")?;
        let unit = words[1].trim_end_matches('s');
        let seconds = match unit {
            "second" => 1,
            "minute" => 60,
            "hour" => 3600,
            "day" => 86400,
            "week" => 7 * 86400,
            "month" => 30 * 86400,
            "year" => 365 * 86400,
            other => bail!("无法识别时间单位 {other}"),
        };
        return Ok(now - n * seconds);
    }
    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Ok(local_timestamp(dt));
        }
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Ok(local_timestamp(date.and_hms_opt(0, 0, 0).expect("合法时间")));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(dt.timestamp());
    }
    bail!("无法识别日期 {text}")
}

fn local_timestamp(dt: chrono::NaiveDateTime) -> i64 {
    use chrono::TimeZone;
    chrono::Local
        .from_local_datetime(&dt)
        .single()
        .map(|t| t.timestamp())
        .unwrap_or_else(|| dt.and_utc().timestamp())
}
