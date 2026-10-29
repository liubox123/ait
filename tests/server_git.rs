//! 服务端 Git 规则：删分支、推标签、隔离被拒推送、受保护分支、改密码、注销全部会话、令牌过期。

use std::fs;
use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use ait::git::{Oid, Repository, Signature, TagObject};
use ait::store::{PushRequest, Store};

fn temp_dir() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("ait-server-git-{}", ait::identity::random_token()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn who() -> Signature {
    ait::identity::signature_now("Tester", "tester@example.com")
}

fn commit(repo: &Repository, file: &str, text: &str, message: &str) -> Oid {
    fs::write(repo.work_tree().unwrap().join(file), text).unwrap();
    repo.add(&[file.to_string()]).unwrap();
    repo.commit(message, false, &who()).unwrap()
}

fn request(repo: &Repository, reference: &str, old: Oid, new: Oid, force: bool) -> PushRequest {
    let have: Vec<Oid> = if old.is_zero() { Vec::new() } else { vec![old] };
    let pack = if new.is_zero() {
        None
    } else {
        Some(STANDARD.encode(repo.pack_for(&[new], &have).unwrap()))
    };
    serde_json::from_value(serde_json::json!({
        "reference": reference,
        "old_oid": old,
        "new_oid": new,
        "force": force,
        "pack_b64": pack,
    }))
    .unwrap()
}

#[test]
fn branch_delete_tags_quarantine_and_protection() {
    let root = temp_dir();
    let store = Store::open(root.join("data")).unwrap();
    let alice = store.register("alice", "pw-a").unwrap();
    let bob = store.register("bob", "pw-b").unwrap();
    store.create_repo("alice", alice.admin, "shop", "").unwrap();
    let server_git = root.join("data/repos/shop/git");

    let local = Repository::init(&root.join("work"), false).unwrap();
    let one = commit(&local, "a.txt", "one\n", "one");
    let pushed = store
        .push("shop", "alice", true, request(&local, "refs/heads/main", Oid::ZERO, one, false))
        .unwrap();
    assert_eq!(pushed.status, "updated");

    // bob 建自己的分支，可以删；不能删 main，也不能删别人的分支
    local.create_branch("bob-work", one).unwrap();
    let two = {
        local.checkout("bob-work", false).unwrap();
        commit(&local, "b.txt", "bob\n", "bob")
    };
    store
        .push("shop", "bob", false, request(&local, "bob-work", Oid::ZERO, two, false))
        .unwrap();
    let err = store
        .push("shop", "bob", false, request(&local, "main", one, Oid::ZERO, false))
        .unwrap_err();
    assert!(err.message.contains("受保护"), "{}", err.message);
    store
        .push("shop", "alice", true, request(&local, "alice-work", Oid::ZERO, one, false))
        .unwrap();
    let err = store
        .push("shop", "bob", false, request(&local, "alice-work", one, Oid::ZERO, false))
        .unwrap_err();
    assert!(err.message.contains("自己创建"), "{}", err.message);
    let deleted = store
        .push("shop", "bob", false, request(&local, "bob-work", two, Oid::ZERO, false))
        .unwrap();
    assert_eq!(deleted.status, "deleted");
    let server = Repository::open(&server_git).unwrap();
    assert!(server.resolve_ref("refs/heads/bob-work").unwrap().is_none());

    // 被拒的推送不留下对象：bob 的非快进推送
    store
        .push("shop", "bob", false, request(&local, "bob-2", Oid::ZERO, two, false))
        .unwrap();
    local.checkout("main", false).unwrap();
    local.create_branch("side", one).unwrap();
    local.checkout("side", false).unwrap();
    let side = commit(&local, "side.txt", "side only\n", "side");
    let blob = Repository::open(&server_git).unwrap();
    let err = store
        .push("shop", "bob", false, request(&local, "bob-2", one, side, false))
        .unwrap_err();
    assert!(err.message.contains("请先拉取") || err.message.contains("快进"), "{}", err.message);
    assert!(!blob.has_object(side), "被拒推送的提交不应写入服务端");

    // 标签：只有所有者能推；移动需要 force；附注标签对象完整
    let tag = TagObject {
        object: one,
        kind: "commit".into(),
        name: "v1".into(),
        tagger: Some(who()),
        message: "release\n".into(),
    };
    let tag_oid = local.write_object("tag", &tag.encode()).unwrap();
    let err = store
        .push("shop", "bob", false, request(&local, "refs/tags/v1", Oid::ZERO, tag_oid, false))
        .unwrap_err();
    assert!(err.message.contains("所有者"), "{}", err.message);
    let pushed = store
        .push("shop", "alice", true, request(&local, "refs/tags/v1", Oid::ZERO, tag_oid, false))
        .unwrap();
    assert_eq!(pushed.status, "updated");
    let refs = store.refs("shop", "bob", false).unwrap();
    assert!(refs.tags.iter().any(|t| t.name == "refs/tags/v1" && t.oid == tag_oid));
    let err = store
        .push("shop", "alice", true, request(&local, "refs/tags/v1", tag_oid, two, false))
        .unwrap_err();
    assert!(err.message.contains("--force"), "{}", err.message);

    // 按 pack 取回：包含附注标签
    let pack = store.fetch_pack("shop", &[tag_oid], &[], true).unwrap();
    let fresh = Repository::init(&root.join("fresh"), false).unwrap();
    fresh.stage_pack(&pack).unwrap();
    fresh.persist_staged().unwrap();
    assert!(fresh.has_object(tag_oid) && fresh.has_object(one));
    let err = store.fetch_pack("shop", &[side], &[], false).unwrap_err();
    assert!(err.message.contains("不在任何分支"), "{}", err.message);

    // 受保护分支可配置
    let err = store
        .set_protected("shop", "bob", false, vec!["release/*".into()])
        .unwrap_err();
    assert_eq!(err.status.as_u16(), 403);
    let patterns = store
        .set_protected("shop", "alice", true, vec!["main".into(), "release/*".into()])
        .unwrap();
    assert_eq!(patterns, vec!["main".to_string(), "release/*".to_string()]);
    store
        .push("shop", "alice", true, request(&local, "release/1", Oid::ZERO, one, false))
        .unwrap();
    let err = store
        .push("shop", "alice", true, request(&local, "release/1", one, Oid::ZERO, false))
        .unwrap_err();
    assert!(err.message.contains("受保护"), "{}", err.message);
    let _ = bob;
    let _ = fs::remove_dir_all(root);
}

#[test]
fn password_change_logout_all_and_token_expiry() {
    let root = temp_dir();
    let store = Store::open(root.join("data")).unwrap();
    let first = store.register("carol", "old-pass").unwrap();
    let second = store.login("carol", "old-pass").unwrap();
    assert!(store.verify_basic("carol", "old-pass").is_ok());
    assert!(store.verify_basic("carol", &first.token).is_ok());
    assert!(store.verify_basic("carol", "nope").is_err());

    let err = store.change_password("carol", "wrong", "new-pass").unwrap_err();
    assert_eq!(err.status.as_u16(), 401);
    let fresh = store.change_password("carol", "old-pass", "new-pass").unwrap();
    assert!(store.authenticate(&first.token).is_err());
    assert!(store.authenticate(&second.token).is_err());
    assert!(store.authenticate(&fresh.token).is_ok());
    assert!(store.login("carol", "old-pass").is_err());
    let again = store.login("carol", "new-pass").unwrap();

    assert_eq!(store.logout_all("carol").unwrap(), 2);
    assert!(store.authenticate(&fresh.token).is_err());
    assert!(store.authenticate(&again.token).is_err());

    let session = store.login("carol", "new-pass").unwrap();
    let times_path = root.join("data/token_times.json");
    let mut times: std::collections::BTreeMap<String, i64> =
        serde_json::from_str(&fs::read_to_string(&times_path).unwrap()).unwrap();
    for value in times.values_mut() {
        *value = 0;
    }
    fs::write(&times_path, serde_json::to_string(&times).unwrap()).unwrap();
    let err = store.authenticate(&session.token).unwrap_err();
    assert!(err.message.contains("过期"), "{}", err.message);
    let _ = fs::remove_dir_all(root);
}
