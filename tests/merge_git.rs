//! 用系统 Git 的合并结果对照 ait。覆盖行级合并、重命名、多个合并基、
//! 修改/删除、文件模式，以及 `--ff-only` / `--no-ff` / `--abort`。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ait::git::Repository;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn temp_dir() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("ait-merge-{}", ait::identity::random_token()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn git(dir: &Path, args: &[&str]) -> (bool, String) {
    let config = std::env::temp_dir().join("ait-empty-gitconfig");
    if !config.exists() {
        fs::write(&config, "").unwrap();
    }
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &config)
        .env("GIT_AUTHOR_NAME", "Git")
        .env("GIT_AUTHOR_EMAIL", "git@example.com")
        .env("GIT_COMMITTER_NAME", "Git")
        .env("GIT_COMMITTER_EMAIL", "git@example.com")
        .output()
        .expect("运行 git");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let (ok, text) = git(dir, args);
    assert!(ok, "git {args:?} 失败\n{text}");
    text
}

fn ait(bin: &str, home: &Path, dir: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(bin)
        .args(args)
        .current_dir(dir)
        .env("AIT_HOME", home)
        .env("AIT_AUTHOR_NAME", "Git")
        .env("AIT_AUTHOR_EMAIL", "git@example.com")
        .output()
        .expect("运行 ait");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

fn ait_ok(bin: &str, home: &Path, dir: &Path, args: &[&str]) -> String {
    let (ok, text) = ait(bin, home, dir, args);
    assert!(ok, "ait {args:?} 失败\n{text}");
    text
}

struct Pair {
    bin: String,
    home: PathBuf,
    git_repo: PathBuf,
    ait_repo: PathBuf,
}

impl Pair {
    fn new() -> Self {
        let root = temp_dir();
        let home = root.join("home");
        let git_repo = root.join("git");
        let ait_repo = root.join("ait");
        fs::create_dir_all(&home).unwrap();
        let bin = env!("CARGO_BIN_EXE_ait").to_string();
        git_ok(&root, &["init", "-b", "main", git_repo.to_str().unwrap()]);
        ait_ok(&bin, &home, &root, &["init", ait_repo.to_str().unwrap()]);
        Self {
            bin,
            home,
            git_repo,
            ait_repo,
        }
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        for repo in [&self.git_repo, &self.ait_repo] {
            let path = repo.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, bytes).unwrap();
        }
    }

    #[cfg(unix)]
    fn exec(&self, rel: &str) {
        use std::os::unix::fs::PermissionsExt;
        for repo in [&self.git_repo, &self.ait_repo] {
            let path = repo.join(rel);
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
    }

    fn remove(&self, rel: &str) {
        for repo in [&self.git_repo, &self.ait_repo] {
            let path = repo.join(rel);
            if path.is_dir() {
                fs::remove_dir_all(path).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
        }
    }

    fn commit(&self, message: &str) {
        git_ok(&self.git_repo, &["add", "-A"]);
        git_ok(&self.git_repo, &["commit", "-m", message]);
        ait_ok(&self.bin, &self.home, &self.ait_repo, &["add"]);
        ait_ok(
            &self.bin,
            &self.home,
            &self.ait_repo,
            &["commit", "-m", message],
        );
    }

    fn branch(&self, name: &str) {
        git_ok(&self.git_repo, &["branch", name]);
        ait_ok(&self.bin, &self.home, &self.ait_repo, &["branch", name]);
    }

    fn checkout_new(&self, name: &str) {
        git_ok(&self.git_repo, &["checkout", "-b", name]);
        ait_ok(
            &self.bin,
            &self.home,
            &self.ait_repo,
            &["checkout", "-b", name],
        );
    }

    fn checkout(&self, name: &str) {
        git_ok(&self.git_repo, &["checkout", name]);
        ait_ok(&self.bin, &self.home, &self.ait_repo, &["switch", name]);
    }

    fn merge(&self, branch: &str) -> (bool, String, bool, String) {
        let (git_ok, git_text) = git(&self.git_repo, &["merge", "--no-edit", branch]);
        let (ait_ok, ait_text) = ait(&self.bin, &self.home, &self.ait_repo, &["merge", branch]);
        (git_ok, git_text, ait_ok, ait_text)
    }

    fn files(&self, repo: &Path) -> Vec<(String, Vec<u8>, u32)> {
        let mut out = Vec::new();
        walk(repo, repo, &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn same_worktree(&self) {
        let git_files = self.files(&self.git_repo);
        let ait_files = self.files(&self.ait_repo);
        assert_eq!(
            git_files, ait_files,
            "工作区与 git 不一致\ngit={git_files:?}\nait={ait_files:?}"
        );
    }
}

fn exec_bit(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path).unwrap().permissions().mode() & 0o111 == 0 {
            0
        } else {
            1
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>, u32)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, out);
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        out.push((rel, fs::read(&path).unwrap(), exec_bit(&path)));
    }
}

#[test]
fn line_merge_matches_git() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    pair.write("note.txt", b"alpha\nkeep\nomega\n");
    pair.commit("base");
    pair.checkout_new("feature");
    pair.write("note.txt", b"ALPHA\nkeep\nomega\n");
    pair.commit("ours-top");
    pair.checkout("main");
    pair.write("note.txt", b"alpha\nkeep\nOMEGA\n");
    pair.commit("theirs-bottom");
    let (git_ok, git_text, ait_ok, ait_text) = pair.merge("feature");
    assert!(git_ok, "{git_text}");
    assert!(ait_ok, "{ait_text}");
    assert!(ait_text.contains("已合并"), "{ait_text}");
    pair.same_worktree();
    assert_eq!(
        fs::read(pair.ait_repo.join("note.txt")).unwrap(),
        b"ALPHA\nkeep\nOMEGA\n"
    );
}

#[test]
fn overlapping_and_adjacent_conflicts_match_git() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    pair.write("note.txt", b"a\nb\nc\n");
    pair.commit("base");
    pair.checkout_new("feature");
    pair.write("note.txt", b"A\nb\nc\n");
    pair.commit("feature");
    pair.checkout("main");
    pair.write("note.txt", b"a\nB\nc\n");
    pair.commit("main-edit");
    let (git_ok, _, ait_ok, ait_text) = pair.merge("feature");
    assert!(!git_ok);
    assert!(!ait_ok);
    assert!(ait_text.contains("有冲突"), "{ait_text}");
    pair.same_worktree();
}

#[test]
fn same_edit_and_modify_delete_match_git() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    pair.write("same.txt", b"base\n");
    pair.write("gone.txt", b"keep\n");
    pair.commit("base");
    pair.checkout_new("feature");
    pair.write("same.txt", b"BOTH\n");
    pair.write("gone.txt", b"edited\n");
    pair.commit("feature");
    pair.checkout("main");
    pair.write("same.txt", b"BOTH\n");
    pair.remove("gone.txt");
    pair.commit("main-edit");
    let (git_ok, git_text, ait_ok, ait_text) = pair.merge("feature");
    assert!(!git_ok, "{git_text}");
    assert!(!ait_ok, "{ait_text}");
    assert!(ait_text.contains("gone.txt"), "{ait_text}");
    pair.same_worktree();
    assert_eq!(fs::read(pair.ait_repo.join("same.txt")).unwrap(), b"BOTH\n");
    assert_eq!(
        fs::read(pair.ait_repo.join("gone.txt")).unwrap(),
        b"edited\n"
    );
}

#[test]
fn exact_rename_plus_edit_matches_git() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    pair.write("old.txt", b"hello\n");
    pair.commit("base");
    pair.checkout_new("renamed");
    pair.remove("old.txt");
    pair.write("new.txt", b"hello\n");
    pair.commit("rename");
    pair.checkout("main");
    pair.write("old.txt", b"hello\nworld\n");
    pair.commit("edit");
    let (git_ok, git_text, ait_ok, ait_text) = pair.merge("renamed");
    assert!(git_ok, "{git_text}");
    assert!(ait_ok, "{ait_text}");
    pair.same_worktree();
    assert!(!pair.ait_repo.join("old.txt").exists());
    assert_eq!(
        fs::read(pair.ait_repo.join("new.txt")).unwrap(),
        b"hello\nworld\n"
    );
}

#[test]
#[cfg(unix)]
fn executable_bit_survives_content_merge() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    pair.write("run.sh", b"#!/bin/sh\n");
    pair.commit("base");
    pair.checkout_new("mode");
    pair.exec("run.sh");
    pair.commit("mode");
    pair.checkout("main");
    pair.write("run.sh", b"#!/bin/sh\necho hi\n");
    pair.commit("body");
    let (git_ok, git_text, ait_ok, ait_text) = pair.merge("mode");
    assert!(git_ok, "{git_text}");
    assert!(ait_ok, "{ait_text}");
    pair.same_worktree();
}

#[test]
fn two_merge_bases_do_not_false_conflict() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    pair.write("a.txt", b"a\n");
    pair.write("b.txt", b"b\n");
    pair.commit("base");
    pair.checkout_new("left");
    pair.write("a.txt", b"aL\n");
    pair.commit("left");
    pair.checkout("main");
    pair.write("b.txt", b"bR\n");
    pair.commit("right");
    pair.checkout_new("m1");
    let (git_ok, git_text, ait_ok, ait_text) = pair.merge("left");
    assert!(git_ok, "{git_text}");
    assert!(ait_ok, "{ait_text}");
    pair.write("a.txt", b"aL1\n");
    pair.commit("m1-edit");
    pair.checkout("main");
    pair.checkout_new("m2");
    let (git_ok, git_text, ait_ok, ait_text) = pair.merge("left");
    assert!(git_ok, "{git_text}");
    assert!(ait_ok, "{ait_text}");
    pair.write("b.txt", b"bR2\n");
    pair.commit("m2-edit");
    pair.checkout("m1");
    let (git_ok, git_text, ait_ok, ait_text) = pair.merge("m2");
    assert!(git_ok, "git 在两个合并基上冲突了\n{git_text}");
    assert!(ait_ok, "ait 在两个合并基上冲突了\n{ait_text}");
    pair.same_worktree();
    assert_eq!(fs::read(pair.ait_repo.join("a.txt")).unwrap(), b"aL1\n");
    assert_eq!(fs::read(pair.ait_repo.join("b.txt")).unwrap(), b"bR2\n");
}

#[test]
fn ff_only_no_ff_and_abort() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    pair.write("note.txt", b"base\n");
    pair.commit("base");
    pair.branch("side");
    pair.checkout("side");
    pair.write("note.txt", b"side\n");
    pair.commit("side");
    pair.checkout("main");
    pair.write("note.txt", b"main\n");
    pair.commit("main");

    let (git_ff, _) = git(&pair.git_repo, &["merge", "--ff-only", "side"]);
    let (ait_ff, ait_text) = ait(
        &pair.bin,
        &pair.home,
        &pair.ait_repo,
        &["merge", "--ff-only", "side"],
    );
    assert!(!git_ff);
    assert!(!ait_ff, "{ait_text}");
    assert!(ait_text.contains("不是快进"), "{ait_text}");

    let (git_merged, git_text, ait_merged, ait_text) = pair.merge("side");
    assert!(!git_merged, "{git_text}");
    assert!(!ait_merged, "{ait_text}");
    let before = fs::read(pair.ait_repo.join("note.txt")).unwrap();
    let (git_abort, git_abort_text) = git(&pair.git_repo, &["merge", "--abort"]);
    let (ait_abort, ait_abort_text) =
        ait(&pair.bin, &pair.home, &pair.ait_repo, &["merge", "--abort"]);
    assert!(git_abort, "{git_abort_text}");
    assert!(ait_abort, "{ait_abort_text}");
    assert_eq!(fs::read(pair.ait_repo.join("note.txt")).unwrap(), b"main\n");
    assert_ne!(before, b"main\n");
    pair.same_worktree();

    let forward = Pair::new();
    forward.write("note.txt", b"base\n");
    forward.commit("base");
    forward.checkout_new("ahead");
    forward.write("note.txt", b"ahead\n");
    forward.commit("ahead");
    forward.checkout("main");
    git_ok(
        &forward.git_repo,
        &["merge", "--no-ff", "--no-edit", "ahead"],
    );
    let text = ait_ok(
        &forward.bin,
        &forward.home,
        &forward.ait_repo,
        &["merge", "--no-ff", "ahead"],
    );
    assert!(text.contains("已合并"), "{text}");
    forward.same_worktree();
    let repo = Repository::discover(&forward.ait_repo).unwrap();
    let head = repo.rev_parse("HEAD").unwrap();
    assert_eq!(repo.read_commit(head).unwrap().parents.len(), 2);
}

#[test]
fn binary_conflict_keeps_ours_like_git() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    pair.write("bin.dat", b"a\0b\n");
    pair.commit("base");
    pair.checkout_new("feature");
    pair.write("bin.dat", b"a\0B\n");
    pair.commit("feature");
    pair.checkout("main");
    pair.write("bin.dat", b"A\0b\n");
    pair.commit("main-edit");
    let (git_merged, git_text, ait_merged, ait_text) = pair.merge("feature");
    assert!(!git_merged, "{git_text}");
    assert!(!ait_merged, "{ait_text}");
    pair.same_worktree();
    assert_eq!(fs::read(pair.ait_repo.join("bin.dat")).unwrap(), b"A\0b\n");
}

#[test]
fn similar_rename_and_dissimilar_add_match_git() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    let base: String = (0..20)
        .map(|index| format!("line {index} stays in the file"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    pair.write("old.txt", base.as_bytes());
    pair.commit("base");
    pair.checkout_new("moved");
    pair.remove("old.txt");
    let mut renamed = base.clone();
    renamed = renamed.replacen("line 19 stays in the file", "line 19 was edited", 1);
    pair.write("new.txt", renamed.as_bytes());
    pair.commit("rename");
    pair.checkout("main");
    let edited = base.replacen("line 0 stays in the file", "line 0 from main", 1);
    pair.write("old.txt", edited.as_bytes());
    pair.commit("edit");
    let (git_merged, git_text, ait_merged, ait_text) = pair.merge("moved");
    assert!(git_merged, "{git_text}");
    assert!(ait_merged, "{ait_text}");
    pair.same_worktree();
    assert!(!pair.ait_repo.join("old.txt").exists());

    let low = Pair::new();
    low.write("old.txt", b"one\n");
    low.commit("base");
    low.checkout_new("side");
    low.remove("old.txt");
    low.write(
        "new.txt",
        b"totally different contents\nwith nothing in common\n",
    );
    low.commit("replace");
    low.checkout("main");
    low.write("old.txt", b"one!\n");
    low.commit("edit");
    let (git_merged, git_text, ait_merged, ait_text) = low.merge("side");
    assert_eq!(git_merged, ait_merged, "git:\n{git_text}\nait:\n{ait_text}");
    low.same_worktree();
}

#[test]
fn strategy_ours_favor_and_diff3_match_git() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    pair.write("note.txt", b"base\n");
    pair.commit("base");
    pair.checkout_new("side");
    pair.write("note.txt", b"side\n");
    pair.commit("side");
    pair.checkout("main");
    pair.write("note.txt", b"main\n");
    pair.commit("main");
    git_ok(
        &pair.git_repo,
        &["merge", "-s", "ours", "--no-edit", "side"],
    );
    let text = ait_ok(
        &pair.bin,
        &pair.home,
        &pair.ait_repo,
        &["merge", "-s", "ours", "side"],
    );
    assert!(text.contains("已合并"), "{text}");
    pair.same_worktree();
    assert_eq!(fs::read(pair.ait_repo.join("note.txt")).unwrap(), b"main\n");
    let repo = Repository::discover(&pair.ait_repo).unwrap();
    assert_eq!(
        repo.read_commit(repo.rev_parse("HEAD").unwrap())
            .unwrap()
            .parents
            .len(),
        2
    );

    let favor = Pair::new();
    favor.write("note.txt", b"one\n");
    favor.commit("base");
    favor.checkout_new("side");
    favor.write("note.txt", b"SIDE\n");
    favor.commit("side");
    favor.checkout("main");
    favor.write("note.txt", b"MAIN\n");
    favor.commit("main");
    git_ok(
        &favor.git_repo,
        &["merge", "-X", "ours", "--no-edit", "side"],
    );
    ait_ok(
        &favor.bin,
        &favor.home,
        &favor.ait_repo,
        &["merge", "-X", "ours", "side"],
    );
    favor.same_worktree();
    assert_eq!(
        fs::read(favor.ait_repo.join("note.txt")).unwrap(),
        b"MAIN\n"
    );

    let styled = Pair::new();
    styled.write("note.txt", b"one\n");
    styled.commit("base");
    styled.checkout_new("side");
    styled.write("note.txt", b"SIDE\n");
    styled.commit("side");
    styled.checkout("main");
    styled.write("note.txt", b"MAIN\n");
    styled.commit("main");
    git_ok(
        &styled.git_repo,
        &["config", "merge.conflictStyle", "diff3"],
    );
    Repository::discover(&styled.ait_repo)
        .unwrap()
        .config_set("merge.conflictStyle", "diff3")
        .unwrap();
    let (git_merged, _, ait_merged, ait_text) = styled.merge("side");
    assert!(!git_merged);
    assert!(!ait_merged, "{ait_text}");
    styled.same_worktree();
    let marked = fs::read(styled.ait_repo.join("note.txt")).unwrap();
    assert!(
        marked.windows(7).any(|item| item == b"|||||||"),
        "{marked:?}"
    );
}

#[test]
fn unmerged_index_matches_git() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    pair.write("note.txt", b"one\n");
    pair.commit("base");
    pair.checkout_new("side");
    pair.write("note.txt", b"SIDE\n");
    pair.commit("side");
    pair.checkout("main");
    pair.write("note.txt", b"MAIN\n");
    pair.commit("main");
    let (git_merged, _, ait_merged, ait_text) = pair.merge("side");
    assert!(!git_merged);
    assert!(!ait_merged, "{ait_text}");
    let (_, git_index) = git(&pair.git_repo, &["ls-files", "-u"]);
    let (_, ait_index) = git(&pair.ait_repo, &["ls-files", "-u"]);
    assert_eq!(git_index, ait_index, "git:\n{git_index}\nait:\n{ait_index}");
    git_ok(&pair.ait_repo, &["merge", "--abort"]);
    assert_eq!(fs::read(pair.ait_repo.join("note.txt")).unwrap(), b"MAIN\n");
    let status = ait_ok(&pair.bin, &pair.home, &pair.ait_repo, &["status"]);
    assert!(status.contains("干净"), "{status}");
}

#[test]
fn file_directory_conflict_is_reported() {
    if !git_available() {
        return;
    }
    let pair = Pair::new();
    pair.write("path", b"file\n");
    pair.commit("base");
    pair.checkout_new("dirside");
    pair.remove("path");
    pair.write("path/inside", b"nested\n");
    pair.commit("dir");
    pair.checkout("main");
    pair.write("path", b"file-v2\n");
    pair.commit("file");
    let (git_ok, _, ait_ok, ait_text) = pair.merge("dirside");
    assert!(!git_ok);
    assert!(!ait_ok, "{ait_text}");
    assert!(ait_text.contains("path"), "{ait_text}");
    assert_eq!(
        fs::read(pair.ait_repo.join("path/inside")).unwrap(),
        b"nested\n"
    );
    assert_eq!(
        fs::read(pair.ait_repo.join("path~HEAD")).unwrap(),
        b"file-v2\n"
    );
    let repo = Repository::discover(&pair.ait_repo).unwrap();
    let head = repo.rev_parse("HEAD").unwrap();
    assert_eq!(repo.read_commit(head).unwrap().parents.len(), 1);
}
