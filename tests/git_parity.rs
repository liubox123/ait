//! 与系统 git 对照：撤销与改写、查看命令、多远端与标签、Git smart HTTP。
//! 系统没有 git 时跳过。

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Server {
    child: Child,
    origin: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_dir() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("ait-parity-{}", ait::identity::random_token()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn has_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn ait(home: &Path, cwd: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_ait"))
        .args(args)
        .current_dir(cwd)
        .env("AIT_HOME", home)
        .env("AIT_PASSWORD", "secret")
        .env("AIT_AUTHOR_NAME", "Tester")
        .env("AIT_AUTHOR_EMAIL", "tester@example.com")
        .env_remove("GIT_EDITOR")
        .env_remove("EDITOR")
        .env_remove("VISUAL")
        .output()
        .expect("启动 ait");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

fn must(home: &Path, cwd: &Path, args: &[&str]) -> String {
    let (ok, text) = ait(home, cwd, args);
    assert!(ok, "ait {args:?} 失败:\n{text}");
    text
}

fn git(cwd: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", cwd)
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .output()
        .expect("启动 git");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let (ok, text) = git(cwd, args);
    assert!(ok, "git {args:?} 失败:\n{text}");
    text
}

fn start_server(data: &Path) -> Server {
    let data_arg = data.to_string_lossy().to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ait"))
        .args(["serve", "--data", &data_arg, "--bind", "127.0.0.1:0"])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("启动服务");
    let stderr = child.stderr.take().expect("stderr");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let line = line.unwrap_or_default();
            if let Some(index) = line.find("http://") {
                let _ = tx.send(line[index..].trim().trim_end_matches('/').to_string());
            }
        }
    });
    let origin = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("服务没有打印地址");
    for _ in 0..50 {
        let host = origin.trim_start_matches("http://");
        if let Ok(mut stream) = std::net::TcpStream::connect(host) {
            let _ = write!(
                stream,
                "GET /api/health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
            );
            let mut body = String::new();
            let _ = std::io::Read::read_to_string(&mut stream, &mut body);
            if body.contains("\"ok\":true") {
                return Server { child, origin };
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("服务没有在 {origin} 就绪");
}

fn head(cwd: &Path) -> String {
    git_ok(cwd, &["rev-parse", "HEAD"]).trim().to_string()
}

#[test]
fn local_history_rewrite_matches_git() {
    if !has_git() {
        return;
    }
    let root = temp_dir();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    must(&home, &root, &["init", "repo"]);
    let repo = root.join("repo");

    fs::write(repo.join("a.txt"), "a\n").unwrap();
    must(&home, &repo, &["add"]);
    must(&home, &repo, &["commit", "-q", "-m", "one"]);
    fs::write(repo.join("a.txt"), "a\nb\n").unwrap();
    must(&home, &repo, &["commit", "-a", "-m", "two"]);
    fs::write(repo.join("c.txt"), "c\n").unwrap();
    must(&home, &repo, &["add", "c.txt"]);
    must(&home, &repo, &["commit", "-m", "three"]);

    let reflog = must(&home, &repo, &["reflog"]);
    assert!(reflog.contains("HEAD@{0}: commit: three"), "{reflog}");
    assert!(reflog.contains("commit (initial): one"), "{reflog}");
    assert_eq!(reflog, git_ok(&repo, &["reflog"]), "reflog 与 git 不一致");

    for spec in ["HEAD~1", "HEAD^{tree}", "HEAD:a.txt", "HEAD~2^0", "@{1}", ":/two"] {
        assert_eq!(
            must(&home, &repo, &["rev-parse", spec]).trim(),
            git_ok(&repo, &["rev-parse", spec]).trim(),
            "rev-parse {spec}"
        );
    }

    let three = head(&repo);
    must(&home, &repo, &["reset", "--hard", "HEAD~1"]);
    assert!(!repo.join("c.txt").exists());
    assert_eq!(git_ok(&repo, &["status", "--porcelain"]), "");
    must(&home, &repo, &["reset", "--hard", "HEAD@{1}"]);
    assert_eq!(head(&repo), three);

    must(&home, &repo, &["reset", "--soft", "HEAD~1"]);
    assert_eq!(git_ok(&repo, &["status", "--porcelain"]), "A  c.txt\n");
    must(&home, &repo, &["commit", "-m", "three again"]);

    must(&home, &repo, &["commit", "--amend", "-m", "three amended"]);
    assert_eq!(
        git_ok(&repo, &["log", "-1", "--format=%s"]).trim(),
        "three amended"
    );
    git_ok(&repo, &["fsck", "--strict"]);

    must(&home, &repo, &["revert", "--no-edit", "HEAD"]);
    assert!(!repo.join("c.txt").exists());
    assert!(git_ok(&repo, &["log", "-1", "--format=%s"]).starts_with("Revert \"three amended\""));

    must(&home, &repo, &["tag", "v1", "HEAD~1"]);
    must(&home, &repo, &["tag", "-a", "v2", "-m", "release"]);
    assert_eq!(must(&home, &repo, &["describe"]).trim(), "v2");
    assert_eq!(git_ok(&repo, &["describe"]).trim(), "v2");
    assert_eq!(
        git_ok(&repo, &["cat-file", "-t", "v2"]).trim(),
        "tag",
        "附注标签应是 tag 对象"
    );

    let blame = must(&home, &repo, &["blame", "a.txt"]);
    assert_eq!(blame, git_ok(&repo, &["blame", "a.txt"]), "blame 与 git 不一致");

    let grep = must(&home, &repo, &["grep", "-n", "b"]);
    assert!(grep.contains("a.txt:2:b"), "{grep}");

    // stash
    fs::write(repo.join("a.txt"), "a\nb\ndirty\n").unwrap();
    fs::write(repo.join("u.txt"), "untracked\n").unwrap();
    must(&home, &repo, &["stash", "push", "-u", "-m", "wip"]);
    assert_eq!(git_ok(&repo, &["status", "--porcelain"]), "");
    assert!(git_ok(&repo, &["stash", "list"]).contains("On main: wip"));
    must(&home, &repo, &["stash", "pop"]);
    assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "a\nb\ndirty\n");
    assert!(repo.join("u.txt").exists());

    // checkout 路径、restore --source、rm、mv、clean
    must(&home, &repo, &["restore", "a.txt"]);
    must(&home, &repo, &["checkout", "HEAD~3", "--", "a.txt"]);
    assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
    assert_eq!(git_ok(&repo, &["status", "--porcelain", "a.txt"]), "M  a.txt\n");
    must(&home, &repo, &["restore", "--staged", "--worktree", "a.txt"]);
    assert_eq!(git_ok(&repo, &["status", "--porcelain", "a.txt"]), "");
    must(&home, &repo, &["restore", "--source", "HEAD~3", "a.txt"]);
    assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "a\n");
    must(&home, &repo, &["restore", "a.txt"]);
    must(&home, &repo, &["mv", "a.txt", "moved.txt"]);
    assert_eq!(
        git_ok(&repo, &["status", "--porcelain", "--untracked-files=no"]),
        "R  a.txt -> moved.txt\n"
    );
    must(&home, &repo, &["commit", "-q", "-m", "move"]);
    must(&home, &repo, &["rm", "moved.txt"]);
    assert!(!repo.join("moved.txt").exists());
    must(&home, &repo, &["reset", "--hard"]);
    let dry = must(&home, &repo, &["clean", "-n"]);
    assert!(dry.contains("u.txt"), "{dry}");
    must(&home, &repo, &["clean", "-f"]);
    assert!(!repo.join("u.txt").exists());

    // 分离 HEAD 与 switch -
    must(&home, &repo, &["checkout", "--detach", "HEAD~1"]);
    assert!(git(&repo, &["symbolic-ref", "-q", "HEAD"]).0 == false);
    must(&home, &repo, &["switch", "-"]);
    assert_eq!(git_ok(&repo, &["branch", "--show-current"]).trim(), "main");

    // rebase、交互式 rebase、冲突后继续、cherry-pick
    must(&home, &repo, &["switch", "-c", "feat"]);
    fs::write(repo.join("g.txt"), "g1\n").unwrap();
    must(&home, &repo, &["add", "g.txt"]);
    must(&home, &repo, &["commit", "-m", "g1"]);
    fs::write(repo.join("g.txt"), "g1\ng2\n").unwrap();
    must(&home, &repo, &["commit", "-a", "-m", "g2"]);
    must(&home, &repo, &["switch", "main"]);
    fs::write(repo.join("m.txt"), "m\n").unwrap();
    must(&home, &repo, &["add", "m.txt"]);
    must(&home, &repo, &["commit", "-m", "m"]);
    must(&home, &repo, &["switch", "feat"]);
    must(&home, &repo, &["rebase", "main"]);
    let main = git_ok(&repo, &["rev-parse", "main"]);
    assert_eq!(git_ok(&repo, &["rev-parse", "HEAD~2"]), main);
    git_ok(&repo, &["fsck", "--strict"]);

    let todo = root.join("todo");
    fs::write(
        &todo,
        format!(
            "pick {} g1\nfixup {} g2\n",
            git_ok(&repo, &["rev-parse", "--short", "HEAD~1"]).trim(),
            git_ok(&repo, &["rev-parse", "--short", "HEAD"]).trim()
        ),
    )
    .unwrap();
    must(&home, &repo, &["rebase", "-i", "main", "--todo", todo.to_str().unwrap()]);
    assert_eq!(git_ok(&repo, &["rev-parse", "HEAD~1"]), main);
    assert_eq!(fs::read_to_string(repo.join("g.txt")).unwrap(), "g1\ng2\n");
    assert_eq!(git_ok(&repo, &["log", "-1", "--format=%s"]).trim(), "g1");

    must(&home, &repo, &["switch", "-c", "clash", "main"]);
    fs::write(repo.join("m.txt"), "clash\n").unwrap();
    must(&home, &repo, &["commit", "-a", "-m", "clash"]);
    must(&home, &repo, &["switch", "main"]);
    fs::write(repo.join("m.txt"), "main side\n").unwrap();
    must(&home, &repo, &["commit", "-a", "-m", "main side"]);
    must(&home, &repo, &["switch", "clash"]);
    let (ok, text) = ait(&home, &repo, &["rebase", "main"]);
    assert!(!ok, "应出现冲突: {text}");
    assert!(text.contains("m.txt"), "{text}");
    let status = must(&home, &repo, &["status"]);
    assert!(status.contains("rebase"), "{status}");
    fs::write(repo.join("m.txt"), "resolved\n").unwrap();
    must(&home, &repo, &["add", "m.txt"]);
    must(&home, &repo, &["rebase", "--continue"]);
    assert_eq!(git_ok(&repo, &["log", "-1", "--format=%s"]).trim(), "clash");
    assert_eq!(
        git_ok(&repo, &["rev-parse", "HEAD~1"]),
        git_ok(&repo, &["rev-parse", "main"])
    );

    must(&home, &repo, &["switch", "main"]);
    must(&home, &repo, &["cherry-pick", "feat"]);
    assert_eq!(fs::read_to_string(repo.join("g.txt")).unwrap(), "g1\ng2\n");

    // 查看命令
    let log = must(&home, &repo, &["log", "--oneline", "main..feat"]);
    assert_eq!(log.lines().count(), 1, "{log}");
    let shortlog = must(&home, &repo, &["shortlog", "-s"]);
    assert!(shortlog.contains("Tester"), "{shortlog}");
    let show = must(&home, &repo, &["show", "--stat", "HEAD"]);
    assert!(show.contains("g.txt"), "{show}");
    let diff = must(&home, &repo, &["diff", "--stat", "HEAD~1", "HEAD"]);
    assert!(diff.contains("g.txt"), "{diff}");

    // 打包与检查
    must(&home, &repo, &["gc", "--prune=now"]);
    git_ok(&repo, &["fsck", "--full", "--strict"]);
    let fsck = must(&home, &repo, &["fsck"]);
    assert!(fsck.contains("0 个问题"), "{fsck}");
    git_ok(&repo, &["log", "--all", "--oneline"]);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn remotes_tags_and_smart_http_interoperate_with_git() {
    if !has_git() {
        return;
    }
    let root = temp_dir();
    let server = start_server(&root.join("data"));
    let origin = server.origin.clone();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    must(&home, &root, &["register", "--server", &origin, "--user", "alice"]);
    must(&home, &root, &["repo", "create", "shop"]);
    must(&home, &root, &["init", "work"]);
    let work = root.join("work");
    fs::write(work.join("a.txt"), "one\n").unwrap();
    must(&home, &work, &["add"]);
    must(&home, &work, &["commit", "-q", "-m", "one"]);
    must(&home, &work, &["remote", "add", "origin", &format!("{origin}/r/shop")]);

    let pushed = must(&home, &work, &["push", "-u", "origin", "main"]);
    assert!(pushed.contains("main -> main"), "{pushed}");
    assert_eq!(git_ok(&work, &["config", "branch.main.merge"]).trim(), "refs/heads/main");
    must(&home, &work, &["tag", "-a", "v1", "-m", "版本一"]);
    let tags = must(&home, &work, &["push", "--tags"]);
    assert!(tags.contains("v1"), "{tags}");

    must(&home, &work, &["switch", "-c", "tmp"]);
    must(&home, &work, &["push", "-q", "origin", "tmp"]);
    let deleted = must(&home, &work, &["push", "origin", "--delete", "tmp"]);
    assert!(deleted.contains("已删除"), "{deleted}");
    must(&home, &work, &["switch", "main"]);

    // 系统 git 通过 smart HTTP 克隆（v2 与 v0）、推送、推标签、删分支
    let host = origin.trim_start_matches("http://");
    let url = format!("http://alice:secret@{host}/r/shop.git");
    git_ok(&root, &["clone", "-q", &url, "g"]);
    let g = root.join("g");
    assert_eq!(fs::read_to_string(g.join("a.txt")).unwrap(), "one\n");
    assert!(git_ok(&g, &["tag", "-n"]).contains("版本一"));
    git_ok(&g, &["fsck", "--strict"]);
    git_ok(&root, &["-c", "protocol.version=0", "clone", "-q", &url, "g0"]);
    assert_eq!(head(&root.join("g0")), head(&g));
    let (bad, text) = git(&root, &["clone", "-q", &format!("http://alice:wrong@{host}/r/shop.git"), "bad"]);
    assert!(!bad, "错误口令不应克隆成功: {text}");

    fs::write(g.join("a.txt"), "one\ntwo\n").unwrap();
    git_ok(&g, &["commit", "-qam", "two"]);
    git_ok(&g, &["push", "-q", "origin", "main"]);
    git_ok(&g, &["push", "-q", "origin", "main:refs/heads/feature"]);
    git_ok(&g, &["tag", "v2"]);
    git_ok(&g, &["push", "-q", "origin", "v2"]);
    git_ok(&g, &["push", "-q", "origin", "--delete", "feature"]);

    let pulled = must(&home, &work, &["pull", "--ff-only"]);
    assert!(pulled.contains("快进"), "{pulled}");
    assert_eq!(head(&work), head(&g));
    assert!(git_ok(&work, &["tag"]).contains("v2"));

    // fetch --prune
    must(&home, &work, &["switch", "-c", "gone"]);
    must(&home, &work, &["push", "-q", "origin", "gone"]);
    must(&home, &work, &["switch", "main"]);
    must(&home, &work, &["fetch", "-q"]);
    assert!(git_ok(&work, &["branch", "-r"]).contains("origin/gone"));
    git_ok(&g, &["push", "-q", "origin", "--delete", "gone"]);
    let fetched = must(&home, &work, &["fetch", "--prune"]);
    assert!(fetched.contains("origin/gone"), "{fetched}");
    assert!(!git_ok(&work, &["branch", "-r"]).contains("origin/gone"));

    // pull --rebase 与 git 的增量拉取
    fs::write(g.join("g.txt"), "g\n").unwrap();
    git_ok(&g, &["add", "g.txt"]);
    git_ok(&g, &["commit", "-qm", "from git"]);
    git_ok(&g, &["push", "-q", "origin", "main"]);
    fs::write(work.join("w.txt"), "w\n").unwrap();
    must(&home, &work, &["add", "w.txt"]);
    must(&home, &work, &["commit", "-q", "-m", "from ait"]);
    must(&home, &work, &["pull", "--rebase"]);
    assert_eq!(git_ok(&work, &["log", "-1", "--format=%s"]).trim(), "from ait");
    assert_eq!(git_ok(&work, &["rev-list", "--merges", "--count", "HEAD"]).trim(), "0");
    must(&home, &work, &["push", "-q"]);
    git_ok(&g, &["pull", "-q", "--rebase"]);
    assert_eq!(head(&g), head(&work));

    // 非快进被拒
    git_ok(&g, &["reset", "-q", "--hard", "HEAD~1"]);
    fs::write(g.join("x.txt"), "x\n").unwrap();
    git_ok(&g, &["add", "x.txt"]);
    git_ok(&g, &["commit", "-qm", "x"]);
    let (ok, text) = git(&g, &["push", "origin", "main"]);
    assert!(!ok, "非快进推送应被拒绝: {text}");
    let (ok, text) = ait(&home, &work, &["push", "origin", "HEAD~1:main"]);
    assert!(!ok, "{text}");
    assert!(text.contains("不是快进"), "{text}");

    // 多远端
    must(&home, &work, &["remote", "add", "second", &format!("{origin}/r/shop")]);
    must(&home, &work, &["remote", "rename", "second", "backup"]);
    must(&home, &work, &["fetch", "backup"]);
    assert!(git_ok(&work, &["branch", "-r"]).contains("backup/main"));
    must(&home, &work, &["remote", "remove", "backup"]);
    assert!(!git_ok(&work, &["branch", "-r"]).contains("backup/main"));

    // 服务端仓库仍可被 git 校验
    git_ok(&root, &["--git-dir", root.join("data/repos/shop/git").to_str().unwrap(), "fsck"]);
    let _ = fs::remove_dir_all(root);
}
