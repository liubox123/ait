use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ait::git::{Repository, Signature};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn temp_dir() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("ait-compat-{}", ait::identity::random_token()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    let config = std::env::temp_dir().join("ait-empty-gitconfig");
    if !config.exists() {
        fs::write(&config, "").unwrap();
    }
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &config)
        .env("GIT_AUTHOR_NAME", "Git")
        .env("GIT_AUTHOR_EMAIL", "git@example.com")
        .env("GIT_COMMITTER_NAME", "Git")
        .env("GIT_COMMITTER_EMAIL", "git@example.com")
        .output()
        .expect("运行 git")
}

fn author() -> Signature {
    Signature {
        name: "Ait".into(),
        email: "ait@example.com".into(),
        time: 1_700_000_000,
        tz: "+0000".into(),
    }
}

#[test]
fn system_git_reads_and_packs_an_ait_repository() {
    if !git_available() {
        eprintln!("跳过：没有找到 git");
        return;
    }
    let dir = temp_dir();
    let repo = Repository::init(&dir, false).unwrap();
    fs::create_dir_all(dir.join("foo")).unwrap();
    fs::write(dir.join("hello.txt"), "hello\n").unwrap();
    fs::write(dir.join("foo.txt"), "file\n").unwrap();
    fs::write(dir.join("foo/bar.txt"), "dir\n").unwrap();
    repo.add(&[".".into()]).unwrap();
    let oid = repo.commit("first", false, &author()).unwrap();

    let fsck = git(&dir, &["fsck", "--full"]);
    assert!(
        fsck.status.success(),
        "git fsck 失败\nstdout:{}\nstderr:{}",
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr)
    );

    let status = git(&dir, &["status", "--porcelain"]);
    assert!(
        status.status.success(),
        "git status 失败: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&status.stdout).trim(),
        "",
        "git 认为工作区不干净:\n{}",
        String::from_utf8_lossy(&status.stdout)
    );

    let log = git(&dir, &["log", "-1", "--format=%s"]);
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "first");
    let listed = git(&dir, &["rev-parse", "HEAD"]);
    assert_eq!(
        String::from_utf8_lossy(&listed.stdout).trim(),
        oid.to_string()
    );

    let hash = git(&dir, &["hash-object", "hello.txt"]);
    assert_eq!(
        String::from_utf8_lossy(&hash.stdout).trim(),
        "ce013625030ba8dba906f756967f9e9ca394464a"
    );

    fs::write(dir.join("hello.txt"), "hello\nworld\n").unwrap();
    let added = git(&dir, &["add", "hello.txt"]);
    assert!(
        added.status.success(),
        "git add 失败: {}",
        String::from_utf8_lossy(&added.stderr)
    );
    let again = Repository::open(&dir).unwrap();
    let status = again.status().unwrap();
    assert!(
        status.staged.iter().any(|entry| entry.path == "hello.txt"),
        "ait 没有读到 git 写入的索引: {status:?}"
    );
    again.commit("second", false, &author()).unwrap();
    let log = git(&dir, &["log", "--format=%s"]);
    let messages = String::from_utf8_lossy(&log.stdout);
    assert!(messages.contains("second"), "{messages}");
    assert!(messages.contains("first"), "{messages}");

    let gc = git(&dir, &["gc"]);
    assert!(
        gc.status.success(),
        "git gc 失败: {}",
        String::from_utf8_lossy(&gc.stderr)
    );
    let packed = Repository::open(&dir).unwrap();
    let head = packed.head_oid().unwrap().unwrap();
    let history = packed.log(head, 10).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(packed.read_commit(head).unwrap().summary(), "second");
    let files = packed.file_at(head, "foo/bar.txt").unwrap();
    assert_eq!(files.2, b"dir\n");

    let _ = fs::remove_dir_all(dir);
}
