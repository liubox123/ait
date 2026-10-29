use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};

use crate::git::{ChangeKind, Repository};
use crate::identity::{self, load_manifest};
use crate::perm::Access;
use crate::remote;

mod porcelain;

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
        Command::Merge {
            branch,
            abort,
            ff_only,
            no_ff,
            strategy,
            option,
        } => {
            let repo = open_repo()?;
            if abort {
                if branch.is_some() {
                    bail!("--abort 不能同时指定分支");
                }
                repo.merge_abort()?;
                println!("已取消合并");
                return Ok(());
            }
            if ff_only && no_ff {
                bail!("不能同时使用 --ff-only 和 --no-ff");
            }
            let Some(branch) = branch else {
                bail!("请指定要合并的分支");
            };
            let mut favor = 0i8;
            for item in &option {
                match item.as_str() {
                    "ours" => favor = 1,
                    "theirs" => favor = -1,
                    other => bail!("不支持的合并选项: {other}"),
                }
            }
            let strategy_ours = match strategy.as_deref() {
                None | Some("ort") | Some("recursive") => false,
                Some("ours") => true,
                Some(other) => bail!("不支持的合并策略: {other}"),
            };
            let oid = repo.rev_parse(&branch)?;
            let (name, email) = identity::load_identity();
            match repo.merge_with(
                oid,
                &branch,
                &identity::signature_now(&name, &email),
                &crate::git::MergeOptions {
                    no_ff,
                    ff_only,
                    strategy_ours,
                    favor,
                    diff3: false,
                },
            )? {
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
                let (_, email) = identity::saved_identity();
                identity::save_identity(&value, &email)?;
                println!("user.name = {value}");
            }
            "user.email" => {
                let (name, _) = identity::saved_identity();
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
        Command::Git(command) => porcelain::run(command)?,
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
    if let Some(kind) = repo.sequence_kind() {
        let todo = repo.sequence_todo()?;
        println!(
            "正在进行 {kind}，还剩 {} 步；ait {kind} --continue / --skip / --abort",
            todo.len()
        );
    } else if repo.merge_head()?.is_some() {
        println!("正在合并；解决冲突后 ait add 再 ait commit，或 ait merge --abort");
    }
    if let Some(branch) = &status.branch {
        let upstream = repo
            .upstream_of(branch)?
            .unwrap_or_else(|| format!("refs/remotes/origin/{branch}"));
        let label = upstream
            .trim_start_matches("refs/remotes/")
            .trim_start_matches("refs/heads/")
            .to_string();
        if let (Some(local), Some(remote)) = (status.head, repo.resolve_ref(&upstream)?) {
            let (ahead, behind) = porcelain::ahead_behind(repo, local, remote)?;
            match (ahead, behind) {
                (0, 0) => println!("与 {label} 一致"),
                (a, 0) => println!("领先于 {label} {a} 个提交"),
                (0, b) => println!("落后于 {label} {b} 个提交，可以 ait pull"),
                (a, b) => println!("与 {label} 已经分叉（领先 {a}，落后 {b}），ait pull 会尝试合并"),
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
            ChangeKind::Unmerged => "冲突",
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
    /// 合并分支或提交。默认行级三方合并
    Merge {
        branch: Option<String>,
        /// 放弃进行中的合并，工作区回到合并前
        #[arg(long)]
        abort: bool,
        /// 只能快进
        #[arg(long)]
        ff_only: bool,
        /// 即使可以快进也生成合并提交
        #[arg(long)]
        no_ff: bool,
        /// 合并策略。`ours` 保留当前内容并记录这次合并
        #[arg(short = 's', long = "strategy")]
        strategy: Option<String>,
        /// 冲突处理。`ours` 或 `theirs` 采用对应一侧的冲突块
        #[arg(short = 'X')]
        option: Vec<String>,
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
    #[command(flatten)]
    Git(porcelain::GitCommand),
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
