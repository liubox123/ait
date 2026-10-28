use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};

use crate::git::{unified_diff, ChangeKind, Repository};
use crate::identity::{self, load_manifest};
use crate::perm::Access;
use crate::remote;

pub fn execute() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { path } => {
            let path = PathBuf::from(&path);
            let repo = Repository::init(&path, false)?;
            println!("已初始化空仓库 {}", repo.git_dir().display());
        }
        Command::Clone { url, dir } => {
            let dir = dir.unwrap_or_else(|| {
                url.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("repo")
                    .to_string()
            });
            let report = remote::clone_repo(&url, PathBuf::from(&dir).as_path())?;
            println!("{}", report.message);
            print!("{}", report.manifest.render());
        }
        Command::Add { paths } => {
            let repo = open_repo()?;
            repo.add(&paths)?;
            println!("已更新索引");
        }
        Command::Status => print_status(&open_repo()?)?,
        Command::Diff { cached } => {
            let repo = open_repo()?;
            let files = if cached {
                repo.diff_staged()?
            } else {
                repo.diff_unstaged()?
            };
            let text = unified_diff(&files);
            if text.is_empty() {
                println!("没有差异");
            } else {
                print!("{text}");
            }
        }
        Command::Commit { message, all } => {
            let repo = open_repo()?;
            let (name, email) = identity::load_identity();
            if name == "Ait" && email == "ait@local" {
                eprintln!("提示: 正在使用默认身份 Ait <ait@local>，可用 ait config user.name 修改");
            }
            let oid = repo.commit(&message, all, &identity::signature_now(&name, &email))?;
            let branch = repo.branch_name()?.unwrap_or_else(|| "HEAD".into());
            println!("[{branch} {}] {message}", oid.short());
        }
        Command::Log { max_count } => {
            let repo = open_repo()?;
            let Some(head) = repo.head_oid()? else {
                println!("还没有提交");
                return Ok(());
            };
            for commit in repo.log(head, max_count)? {
                println!("commit {}", commit.oid);
                println!("Author: {} <{}>", commit.author_name, commit.author_email);
                println!("Date:   {}", identity::format_time(commit.time, &commit.tz));
                println!();
                for line in commit.message.lines() {
                    println!("    {line}");
                }
                println!();
            }
        }
        Command::Branch { name, delete } => {
            let repo = open_repo()?;
            if delete {
                let name = name.context("请指定要删除的分支")?;
                repo.delete_branch(&name)?;
                println!("已删除分支 {name}");
            } else if let Some(name) = name {
                let oid = repo.head_oid()?.context("还没有提交，不能创建分支")?;
                repo.create_branch(&name, oid)?;
                println!("已创建分支 {name}");
            } else {
                for branch in repo.branches()? {
                    let mark = if branch.current { "* " } else { "  " };
                    println!("{mark}{} {}", branch.name, branch.oid.short());
                }
            }
        }
        Command::Checkout { target, create } => {
            println!("{}", open_repo()?.checkout(&target, create)?);
        }
        Command::Switch { target } => {
            println!("{}", open_repo()?.checkout(&target, false)?);
        }
        Command::Merge { branch } => {
            let repo = open_repo()?;
            let oid = repo.rev_parse(&branch)?;
            let (name, email) = identity::load_identity();
            match repo.merge_oid(oid, &branch, &identity::signature_now(&name, &email))? {
                crate::git::MergeOutcome::UpToDate => println!("已经是最新"),
                crate::git::MergeOutcome::FastForward(oid) => println!("快进到 {}", oid.short()),
                crate::git::MergeOutcome::Merged(oid) => println!("已合并为 {}", oid.short()),
                crate::git::MergeOutcome::Conflicts(paths) => {
                    println!("有冲突:");
                    for path in paths {
                        println!("  {path}");
                    }
                    bail!("请解决冲突后 ait add，再 ait commit");
                }
            }
        }
        Command::Push { force } => {
            let response = remote::push_repo(&open_repo()?, force)?;
            println!("{}", response.message);
            if !response.blocked.is_empty() {
                println!("需要评审的路径:");
                for path in &response.blocked {
                    println!("  {path}");
                }
            }
            print!("{}", response.manifest.render());
        }
        Command::Pull => {
            let report = remote::pull_repo(&open_repo()?)?;
            println!("{}", report.message);
            print!("{}", report.manifest.render());
        }
        Command::Fetch => {
            let repo = open_repo()?;
            let client = remote::Client::from_repo(&repo)?;
            let manifest = remote::fetch_repo(&repo, &client)?;
            println!("已获取远端引用");
            print!("{}", manifest.render());
        }
        Command::Remote { command } => match command {
            Some(RemoteCommand::Add { name, url }) => {
                if name != "origin" {
                    bail!("当前只使用 origin 这个远端名字");
                }
                let repo = open_repo()?;
                repo.config_set("remote.origin.url", &url)?;
                repo.config_set("branch.main.remote", "origin")?;
                repo.config_set("branch.main.merge", "refs/heads/main")?;
                println!("已添加 origin {url}");
            }
            Some(RemoteCommand::List) | None => {
                let repo = open_repo()?;
                match repo.config_get("remote.origin.url")? {
                    Some(url) => println!("origin\t{url}"),
                    None => println!("没有远端"),
                }
            }
        },
        Command::Login { server, user } => {
            let password = identity::read_password("密码: ")?;
            let session = remote::login(&server, &user, &password)?;
            println!("已登录 {} @ {}", session.user, server.trim_end_matches('/'));
        }
        Command::Register { server, user } => {
            let password = identity::read_password("密码: ")?;
            let session = remote::register(&server, &user, &password)?;
            println!(
                "已注册 {}{}",
                session.user,
                if session.admin {
                    "（首位用户，管理员）"
                } else {
                    ""
                }
            );
        }
        Command::Repo { command } => match command {
            RepoCommand::Create {
                name,
                server,
                description,
            } => {
                let origin = remote::resolve_origin(server.as_deref())?;
                let meta = remote::create_repo(&origin, &name, &description)?;
                println!("已创建仓库 {}，所有者 {}", meta.name, meta.owner);
                println!("克隆地址 {origin}/r/{}", meta.name);
            }
            RepoCommand::List { server } => {
                let origin = remote::resolve_origin(server.as_deref())?;
                let repos = remote::list_repos(&origin)?;
                if repos.is_empty() {
                    println!("还没有仓库");
                }
                for repo in repos {
                    println!("{}\t{}\t{}", repo.name, repo.owner, repo.clone_url);
                }
            }
        },
        Command::Config { key, value } => match key.as_str() {
            "user.name" => {
                let (_, email) = identity::load_identity();
                identity::save_identity(&value, &email)?;
                println!("user.name = {value}");
            }
            "user.email" => {
                let (name, _) = identity::load_identity();
                identity::save_identity(&name, &value)?;
                println!("user.email = {value}");
            }
            other => bail!("只支持 user.name 和 user.email，收到 {other}"),
        },
        Command::Perm => {
            let repo = open_repo()?;
            match load_manifest(repo.git_dir())? {
                Some(manifest) => print!("{}", manifest.render()),
                None => println!("还没有权限清单。克隆或拉取后，服务端会下发可修改的目录。"),
            }
        }
        Command::Plan { command } => match command {
            PlanCommand::Show => {
                let text = remote::plan_text(&open_repo()?)?;
                if text.trim().is_empty() {
                    println!("服务端还没有规划");
                } else {
                    println!("{text}");
                }
            }
            PlanCommand::Apply { file, llm } => {
                let content = fs::read_to_string(&file)
                    .with_context(|| format!("读取 {}", file.display()))?;
                let view = remote::apply_plan(&open_repo()?, &content, llm)?;
                println!(
                    "已根据{}更新权限",
                    if view.source == "llm" {
                        "模型"
                    } else {
                        "规划文本"
                    }
                );
                for item in &view.assignments {
                    println!("- {}：{}", item.user, item.duty);
                    for path in &item.paths {
                        println!("    {path}");
                    }
                }
            }
        },
        Command::Review { command } => {
            let repo = open_repo()?;
            match command {
                ReviewCommand::List => {
                    let reviews = remote::reviews(&repo)?;
                    if reviews.is_empty() {
                        println!("没有评审");
                    }
                    for item in reviews {
                        println!(
                            "{}\t{}\t{}\t{}",
                            item.review.id,
                            item.review.status,
                            item.review.author,
                            item.review.reference
                        );
                        if !item.review.blocked.is_empty() {
                            println!("  越权路径: {}", item.review.blocked.join(", "));
                        }
                    }
                }
                ReviewCommand::Approve { id } => {
                    let review = remote::approve(&repo, &id)?;
                    println!("已通过 {}，分支已更新", review.id);
                }
                ReviewCommand::Reject { id, reason } => {
                    let review = remote::reject(&repo, &id, &reason)?;
                    println!("已拒绝 {}", review.id);
                }
            }
        }
        Command::Pr { command } => {
            let repo = open_repo()?;
            match command {
                PrCommand::List => {
                    let pulls = remote::proposals(&repo)?;
                    if pulls.is_empty() {
                        println!("没有合并请求");
                    }
                    for item in pulls {
                        println!(
                            "{}\t{}\t{}\t{} -> {}\t{}",
                            item.proposal.id,
                            item.proposal.status,
                            item.proposal.author,
                            short_ref(&item.proposal.head_ref),
                            short_ref(&item.proposal.base_ref),
                            item.proposal.title
                        );
                        if !item.blocked.is_empty() {
                            println!("  需要评审: {}", item.blocked.join(", "));
                        }
                    }
                }
                PrCommand::Create { title, base } => {
                    let pull = remote::create_proposal(&repo, &title, &base)?;
                    println!(
                        "已创建合并请求 {} {}",
                        pull.proposal.id, pull.proposal.title
                    );
                    println!(
                        "{} -> {}",
                        short_ref(&pull.proposal.head_ref),
                        short_ref(&pull.proposal.base_ref)
                    );
                }
                PrCommand::Approve { id } => {
                    let pull = remote::approve_proposal(&repo, &id)?;
                    println!("已通过 {}", pull.proposal.id);
                }
                PrCommand::Merge { id } => {
                    let pull = remote::merge_proposal(&repo, &id)?;
                    println!(
                        "已合并 {} 到 {}",
                        pull.proposal.id,
                        short_ref(&pull.proposal.base_ref)
                    );
                }
                PrCommand::Close { id } => {
                    let pull = remote::close_proposal(&repo, &id)?;
                    println!("已关闭 {}", pull.proposal.id);
                }
            }
        }
        Command::Serve { bind, data } => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(crate::server::serve(data, &bind))?;
        }
        Command::Mcp { repo } => crate::mcp::serve(repo)?,
        Command::RevParse { name } => {
            println!("{}", open_repo()?.rev_parse(&name)?);
        }
    }
    Ok(())
}

fn open_repo() -> anyhow::Result<Repository> {
    Repository::discover(&std::env::current_dir()?)
}

fn short_ref(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

fn print_status(repo: &Repository) -> anyhow::Result<()> {
    let status = repo.status()?;
    match (&status.branch, status.head) {
        (Some(branch), _) => println!("位于分支 {branch}"),
        (None, Some(oid)) => println!("HEAD 分离在 {}", oid.short()),
        _ => println!("还没有提交"),
    }
    if let Some(branch) = &status.branch {
        if let (Some(local), Some(remote)) = (
            status.head,
            repo.resolve_ref(&format!("refs/remotes/origin/{branch}"))?,
        ) {
            if local == remote {
                println!("与 origin/{branch} 一致");
            } else if repo.is_ancestor(remote, local)? {
                println!("领先于 origin/{branch}");
            } else if repo.is_ancestor(local, remote)? {
                println!("落后于 origin/{branch}，可以 ait pull");
            } else {
                println!("与 origin/{branch} 已经分叉，ait pull 会尝试合并");
            }
        }
    }
    let manifest = load_manifest(repo.git_dir())?;
    print_group("要提交的更改", &status.staged);
    print_group("尚未暂存的更改", &status.unstaged);
    if !status.untracked.is_empty() {
        println!("\n未跟踪的文件:");
        for path in &status.untracked {
            println!("  {path}");
        }
    }
    if let Some(manifest) = &manifest {
        let mut locked = Vec::new();
        let mut denied = Vec::new();
        for entry in status.staged.iter().chain(status.unstaged.iter()) {
            match manifest.decide(&entry.path) {
                Access::Review => locked.push(entry.path.as_str()),
                Access::Deny => denied.push(entry.path.as_str()),
                Access::Write => {}
            }
        }
        if !locked.is_empty() {
            println!("\n这些路径不在你的写权限内，推送时会进入评审:");
            for path in locked {
                println!("  {path}");
            }
        }
        if !denied.is_empty() {
            println!("\n这些路径被禁止修改，推送会被拒绝:");
            for path in denied {
                println!("  {path}");
            }
        }
    }
    if status.staged.is_empty() && status.unstaged.is_empty() && status.untracked.is_empty() {
        println!("没有需要提交的内容，工作区是干净的");
    }
    Ok(())
}

fn print_group(title: &str, entries: &[crate::git::StatusEntry]) {
    if entries.is_empty() {
        return;
    }
    println!("\n{title}:");
    for entry in entries {
        let label = match entry.kind {
            ChangeKind::Added => "新文件",
            ChangeKind::Modified => "修改",
            ChangeKind::Deleted => "删除",
        };
        println!("  {label}: {}", entry.path);
    }
}

#[derive(Parser)]
#[command(
    name = "ait",
    version,
    about = "面向 AI 协作的 Git，本地仓库与 Git 对象格式兼容"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 初始化仓库
    Init {
        #[arg(default_value = ".")]
        path: String,
    },
    /// 从 ait 服务克隆
    Clone { url: String, dir: Option<String> },
    /// 把文件加入索引。不带路径时加入当前目录下的全部更改
    Add {
        #[arg(num_args = 0..)]
        paths: Vec<String>,
    },
    /// 查看工作区状态和越权文件
    Status,
    /// 查看差异。默认是未暂存的修改
    Diff {
        #[arg(long)]
        cached: bool,
    },
    /// 提交
    Commit {
        #[arg(short, long)]
        message: String,
        #[arg(short = 'a', long)]
        all: bool,
    },
    /// 查看提交历史
    Log {
        #[arg(short = 'n', long, default_value_t = 20)]
        max_count: usize,
    },
    /// 列出、创建或删除分支
    Branch {
        name: Option<String>,
        #[arg(short = 'd', long)]
        delete: bool,
    },
    /// 切换分支或提交
    Checkout {
        target: String,
        #[arg(short = 'b')]
        create: bool,
    },
    /// 切换分支
    Switch { target: String },
    /// 合并分支或提交
    Merge { branch: String },
    /// 推送到 origin。越权修改会变成评审，不会直接进入分支
    Push {
        #[arg(long)]
        force: bool,
    },
    /// 拉取并合并，同时更新权限清单
    Pull,
    /// 只下载远端对象和权限清单
    Fetch,
    /// 查看或添加 origin
    Remote {
        #[command(subcommand)]
        command: Option<RemoteCommand>,
    },
    /// 登录 ait 服务
    Login {
        #[arg(long)]
        server: String,
        #[arg(long)]
        user: String,
    },
    /// 注册并登录。第一个用户是管理员
    Register {
        #[arg(long)]
        server: String,
        #[arg(long)]
        user: String,
    },
    /// 服务端仓库
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// 设置 user.name 或 user.email
    Config { key: String, value: String },
    /// 显示最近一次拉取得到的修改权限清单
    Perm,
    /// 查看或应用服务端规划
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// 查看、通过或拒绝越权推送
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    /// 提交、查看、通过或合并分支上的合并请求
    Pr {
        #[command(subcommand)]
        command: PrCommand,
    },
    /// 启动服务和管理页面
    Serve {
        #[arg(long, default_value = "127.0.0.1:7878")]
        bind: String,
        #[arg(long, default_value = "ait-data")]
        data: PathBuf,
    },
    /// 以 MCP 协议服务当前仓库，供 agent 调用
    Mcp {
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// 解析提交或分支
    #[command(name = "rev-parse")]
    RevParse { name: String },
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// 添加 origin
    Add { name: String, url: String },
    /// 列出远端
    List,
}

#[derive(Subcommand)]
enum RepoCommand {
    /// 在服务端创建仓库
    Create {
        name: String,
        #[arg(long)]
        server: Option<String>,
        #[arg(short, long, default_value = "")]
        description: String,
    },
    /// 列出服务端仓库
    List {
        #[arg(long)]
        server: Option<String>,
    },
}

#[derive(Subcommand)]
enum PlanCommand {
    /// 显示服务端保存的规划
    Show,
    /// 解析规划并写成目录权限。加 --llm 时调用 OpenAI 兼容接口
    Apply {
        #[arg(short, long)]
        file: PathBuf,
        #[arg(long)]
        llm: bool,
    },
}

#[derive(Subcommand)]
enum ReviewCommand {
    List,
    Approve {
        id: String,
    },
    Reject {
        id: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
}

#[derive(Subcommand)]
enum PrCommand {
    /// 列出合并请求
    List,
    /// 用当前分支向目标分支提交合并请求
    Create {
        title: String,
        #[arg(long, default_value = "main")]
        base: String,
    },
    /// 通过越权合并请求
    Approve { id: String },
    /// 合并到目标分支
    Merge { id: String },
    /// 关闭合并请求
    Close { id: String },
}
