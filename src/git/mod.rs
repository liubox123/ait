//! 与 Git 兼容的本地仓库实现：loose/pack 对象、索引、引用、工作区和三方合并。

mod diff;
mod ignore;
mod index;
mod merge;
mod object;
mod odb;
mod oid;
mod pack;
mod repo;

pub use diff::{unified_diff, ChangeKind, FileDiff, PathChange};
pub use index::Index;
pub use merge::{merge_maps, MergeAction};
pub use object::{
    Commit, Object, Signature, TreeEntry, MODE_BLOB, MODE_EXEC, MODE_LINK, MODE_TREE,
};
pub use oid::Oid;
pub use repo::{valid_branch, Head, MergeOptions, MergeOutcome, Repository, Status, StatusEntry};

pub fn empty_tree_oid() -> Oid {
    object::hash_payload("tree", &[]).0
}
