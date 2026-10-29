use anyhow::{bail, Context};
use sha1::{Digest, Sha1};

use super::oid::Oid;

pub const MODE_TREE: u32 = 0o40000;
pub const MODE_BLOB: u32 = 0o100644;
pub const MODE_EXEC: u32 = 0o100755;
pub const MODE_LINK: u32 = 0o120000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: i64,
    pub tz: String,
}

impl Signature {
    pub fn format_line(&self) -> String {
        format!("{} <{}> {} {}", self.name, self.email, self.time, self.tz)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        let mut parts = text.rsplitn(3, ' ');
        let tz = parts.next().context("缺少时区")?.to_string();
        let time: i64 = parts.next().context("缺少时间")?.parse().context("时间")?;
        let ident = parts.next().context("缺少作者")?.trim().to_string();
        let start = ident.rfind('<').context("缺少邮箱")?;
        let end = ident.rfind('>').context("缺少邮箱")?;
        if end < start {
            bail!("邮箱格式错误");
        }
        Ok(Signature {
            name: ident[..start].trim().to_string(),
            email: ident[start + 1..end].to_string(),
            time,
            tz,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: String,
    pub oid: Oid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
}

impl Commit {
    pub fn summary(&self) -> &str {
        self.message.lines().next().unwrap_or("").trim()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut body = String::new();
        body.push_str(&format!("tree {}\n", self.tree));
        for parent in &self.parents {
            body.push_str(&format!("parent {parent}\n"));
        }
        body.push_str(&format!("author {}\n", self.author.format_line()));
        body.push_str(&format!("committer {}\n", self.committer.format_line()));
        body.push('\n');
        body.push_str(&self.message);
        if !self.message.ends_with('\n') {
            body.push('\n');
        }
        body.into_bytes()
    }

    pub fn parse(payload: &[u8]) -> anyhow::Result<Self> {
        let text = std::str::from_utf8(payload).context("提交不是 UTF-8")?;
        let (header, message) = text.split_once("\n\n").unwrap_or((text, ""));
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        for line in header.lines() {
            if line.starts_with(' ') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("tree ") {
                tree = Some(Oid::parse(rest)?);
            } else if let Some(rest) = line.strip_prefix("parent ") {
                parents.push(Oid::parse(rest)?);
            } else if let Some(rest) = line.strip_prefix("author ") {
                author = Some(Signature::parse(rest)?);
            } else if let Some(rest) = line.strip_prefix("committer ") {
                committer = Some(Signature::parse(rest)?);
            }
        }
        Ok(Commit {
            tree: tree.context("提交缺少 tree")?,
            parents,
            author: author.context("提交缺少 author")?,
            committer: committer.context("提交缺少 committer")?,
            message: message.to_string(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagObject {
    pub object: Oid,
    pub kind: String,
    pub name: String,
    pub tagger: Option<Signature>,
    pub message: String,
}

impl TagObject {
    pub fn encode(&self) -> Vec<u8> {
        let mut body = format!(
            "object {}\ntype {}\ntag {}\n",
            self.object, self.kind, self.name
        );
        if let Some(tagger) = &self.tagger {
            body.push_str(&format!("tagger {}\n", tagger.format_line()));
        }
        body.push('\n');
        body.push_str(&self.message);
        if !self.message.is_empty() && !self.message.ends_with('\n') {
            body.push('\n');
        }
        body.into_bytes()
    }

    pub fn parse(payload: &[u8]) -> anyhow::Result<Self> {
        let text = String::from_utf8_lossy(payload);
        let (header, message) = text.split_once("\n\n").unwrap_or((&text, ""));
        let mut object = None;
        let mut kind = None;
        let mut name = String::new();
        let mut tagger = None;
        for line in header.lines() {
            if let Some(rest) = line.strip_prefix("object ") {
                object = Some(Oid::parse(rest.trim())?);
            } else if let Some(rest) = line.strip_prefix("type ") {
                kind = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("tag ") {
                name = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("tagger ") {
                tagger = Signature::parse(rest).ok();
            }
        }
        Ok(TagObject {
            object: object.context("标签缺少 object")?,
            kind: kind.context("标签缺少 type")?,
            name,
            tagger,
            message: message.to_string(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Object {
    Blob(Vec<u8>),
    Tree(Vec<TreeEntry>),
    Commit(Commit),
    Tag(Vec<u8>),
}

impl Object {
    pub fn kind(&self) -> &'static str {
        match self {
            Object::Blob(_) => "blob",
            Object::Tree(_) => "tree",
            Object::Commit(_) => "commit",
            Object::Tag(_) => "tag",
        }
    }

    pub fn payload(&self) -> Vec<u8> {
        match self {
            Object::Blob(b) => b.clone(),
            Object::Tree(entries) => encode_tree(entries),
            Object::Commit(c) => c.encode(),
            Object::Tag(b) => b.clone(),
        }
    }

    pub fn parse(kind: &str, payload: &[u8]) -> anyhow::Result<Self> {
        match kind {
            "blob" => Ok(Object::Blob(payload.to_vec())),
            "tree" => Ok(Object::Tree(parse_tree(payload)?)),
            "commit" => Ok(Object::Commit(Commit::parse(payload)?)),
            "tag" => Ok(Object::Tag(payload.to_vec())),
            other => bail!("未知对象类型: {other}"),
        }
    }
}

pub fn sha1(data: &[u8]) -> [u8; 20] {
    let digest = Sha1::digest(data);
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest);
    out
}

/// 返回 `(oid, 含 Git 头的原始字节)`。
pub fn hash_payload(kind: &str, payload: &[u8]) -> (Oid, Vec<u8>) {
    let header = format!("{kind} {}\0", payload.len());
    let mut raw = Vec::with_capacity(header.len() + payload.len());
    raw.extend_from_slice(header.as_bytes());
    raw.extend_from_slice(payload);
    (Oid(sha1(&raw)), raw)
}

pub fn split_raw(raw: &[u8]) -> anyhow::Result<(&str, &[u8])> {
    let nul = raw.iter().position(|b| *b == 0).context("对象缺少头")?;
    let header = std::str::from_utf8(&raw[..nul]).context("对象头")?;
    let (kind, size) = header.split_once(' ').context("对象头格式")?;
    let size: usize = size.parse().context("对象大小")?;
    let payload = &raw[nul + 1..];
    if payload.len() != size {
        bail!("对象大小不符，头声明 {size}，实际 {}", payload.len());
    }
    Ok((kind, payload))
}

/// Git 的树项排序：目录名按「名字后面带 /」比较。
pub fn git_name_cmp(a: &str, a_dir: bool, b: &str, b_dir: bool) -> std::cmp::Ordering {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let n = ab.len().min(bb.len());
    let cmp = ab[..n].cmp(&bb[..n]);
    if cmp != std::cmp::Ordering::Equal {
        return cmp;
    }
    let c1 = if ab.len() == n {
        if a_dir {
            b'/'
        } else {
            0
        }
    } else {
        ab[n]
    };
    let c2 = if bb.len() == n {
        if b_dir {
            b'/'
        } else {
            0
        }
    } else {
        bb[n]
    };
    c1.cmp(&c2)
}

pub fn encode_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut items = entries.to_vec();
    items.sort_by(|a, b| git_name_cmp(&a.name, a.mode == MODE_TREE, &b.name, b.mode == MODE_TREE));
    let mut out = Vec::new();
    for entry in items {
        out.extend_from_slice(format!("{:o} {}\0", entry.mode, entry.name).as_bytes());
        out.extend_from_slice(&entry.oid.0);
    }
    out
}

pub fn parse_tree(payload: &[u8]) -> anyhow::Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut i = 0;
    while i < payload.len() {
        let space = payload[i..]
            .iter()
            .position(|b| *b == b' ')
            .context("树项缺少空格")?
            + i;
        let mode = std::str::from_utf8(&payload[i..space])?;
        let mode = u32::from_str_radix(mode, 8).context("树项模式")?;
        let nul = payload[space + 1..]
            .iter()
            .position(|b| *b == 0)
            .context("树项缺少结尾")?
            + space
            + 1;
        let name = std::str::from_utf8(&payload[space + 1..nul])?.to_string();
        let sha_end = nul + 1 + 20;
        if sha_end > payload.len() {
            bail!("树项对象名被截断");
        }
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&payload[nul + 1..sha_end]);
        entries.push(TreeEntry {
            mode,
            name,
            oid: Oid(oid),
        });
        i = sha_end;
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree_matches_git() {
        let (oid, _) = hash_payload("tree", &[]);
        assert_eq!(oid.to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    }

    #[test]
    fn blob_hello_matches_git() {
        let (oid, _) = hash_payload("blob", b"hello\n");
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
    }

    #[test]
    fn tree_sorts_directory_as_if_slash() {
        let mut entries = vec![
            TreeEntry {
                mode: MODE_TREE,
                name: "foo".into(),
                oid: Oid::ZERO,
            },
            TreeEntry {
                mode: MODE_BLOB,
                name: "foo.txt".into(),
                oid: Oid::ZERO,
            },
        ];
        let encoded = encode_tree(&entries);
        let parsed = parse_tree(&encoded).unwrap();
        assert_eq!(parsed[0].name, "foo.txt");
        assert_eq!(parsed[1].name, "foo");
        entries.reverse();
        assert_eq!(
            parse_tree(&encode_tree(&entries)).unwrap()[0].name,
            "foo.txt"
        );
    }
}
