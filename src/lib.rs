//! ait：面向 AI 协作的 Git。
//!
//! 本地仓库使用标准 Git 对象、索引和引用，因此系统自带的 `git` 可以阅读同一份仓库。
//! 多人与多个 agent 的写权限、评审和职责规划由 ait 服务端执行。

pub mod cli;
pub mod git;
pub mod identity;
pub mod mcp;
pub mod perm;
pub mod plan;
pub mod remote;
pub mod server;
pub mod smart;
pub mod store;

pub use git::Repository;
