use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::bail;

use super::object::sha1;
use super::oid::Oid;

#[derive(Clone, Debug, Default)]
pub struct FileStat {
    pub ctime_s: u32,
    pub ctime_n: u32,
    pub mtime_s: u32,
    pub mtime_n: u32,
    pub dev: u32,
    pub ino: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
}

#[derive(Clone, Debug)]
pub struct IndexEntry {
    pub path: String,
    pub mode: u32,
    pub oid: Oid,
    pub stat: FileStat,
}

#[derive(Clone, Debug, Default)]
pub struct Index {
    pub entries: Vec<IndexEntry>,
}

impl Index {
    pub fn get(&self, path: &str) -> Option<&IndexEntry> {
        self.entries.iter().find(|e| e.path == path)
    }

    pub fn upsert(&mut self, entry: IndexEntry) {
        if let Some(pos) = self.entries.iter().position(|e| e.path == entry.path) {
            self.entries[pos] = entry;
        } else {
            self.entries.push(entry);
        }
        self.entries
            .sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    }

    pub fn remove(&mut self, path: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.path != path);
        before != self.entries.len()
    }

    pub fn remove_prefix(&mut self, prefix: &str) {
        let prefix = prefix.trim_matches('/');
        if prefix.is_empty() || prefix == "." {
            return;
        }
        let dir = format!("{prefix}/");
        self.entries
            .retain(|e| e.path != prefix && !e.path.starts_with(&dir));
    }
}

pub fn read_index(bytes: &[u8]) -> anyhow::Result<Index> {
    if bytes.len() < 12 + 20 {
        bail!("索引文件过短");
    }
    if &bytes[..4] != b"DIRC" {
        bail!("索引魔数不正确");
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
    if version != 2 && version != 3 {
        bail!("不支持的索引版本 {version}");
    }
    let count = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let body_end = bytes.len() - 20;
    let sum = sha1(&bytes[..body_end]);
    if sum.as_slice() != &bytes[body_end..] {
        bail!("索引校验和不匹配");
    }
    let mut entries = Vec::with_capacity(count);
    let mut offset = 12usize;
    for _ in 0..count {
        if offset + 62 > body_end {
            bail!("索引项被截断");
        }
        let stat = FileStat {
            ctime_s: u32be(bytes, offset),
            ctime_n: u32be(bytes, offset + 4),
            mtime_s: u32be(bytes, offset + 8),
            mtime_n: u32be(bytes, offset + 12),
            dev: u32be(bytes, offset + 16),
            ino: u32be(bytes, offset + 20),
            uid: u32be(bytes, offset + 28),
            gid: u32be(bytes, offset + 32),
            size: u32be(bytes, offset + 36),
        };
        let mode = u32be(bytes, offset + 24);
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&bytes[offset + 40..offset + 60]);
        let flags = u16::from_be_bytes(bytes[offset + 60..offset + 62].try_into().unwrap());
        let extended = flags & 0x4000 != 0;
        let mut name_off = offset + 62;
        if extended {
            name_off += 2;
        }
        let name_len_field = (flags & 0x0fff) as usize;
        let name = if name_len_field == 0x0fff {
            let end = bytes[name_off..body_end]
                .iter()
                .position(|b| *b == 0)
                .ok_or_else(|| anyhow::anyhow!("索引文件名没有结尾"))?
                + name_off;
            String::from_utf8(bytes[name_off..end].to_vec())?
        } else {
            let end = name_off + name_len_field;
            if end > body_end {
                bail!("索引文件名被截断");
            }
            String::from_utf8(bytes[name_off..end].to_vec())?
        };
        let raw_len = (name_off - offset) + name.len() + 1;
        let entry_len = (raw_len + 7) & !7;
        offset += entry_len;
        entries.push(IndexEntry {
            path: name,
            mode,
            oid: Oid(oid),
            stat,
        });
    }
    Ok(Index { entries })
}

pub fn write_index(index: &Index) -> Vec<u8> {
    let mut entries = index.entries.clone();
    entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DIRC");
    buf.extend_from_slice(&2u32.to_be_bytes());
    buf.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for entry in &entries {
        let name = entry.path.as_bytes();
        let entry_len = (62 + name.len() + 8) & !7;
        let start = buf.len();
        buf.resize(start + entry_len, 0);
        put_u32(&mut buf, start, entry.stat.ctime_s);
        put_u32(&mut buf, start + 4, entry.stat.ctime_n);
        put_u32(&mut buf, start + 8, entry.stat.mtime_s);
        put_u32(&mut buf, start + 12, entry.stat.mtime_n);
        put_u32(&mut buf, start + 16, entry.stat.dev);
        put_u32(&mut buf, start + 20, entry.stat.ino);
        put_u32(&mut buf, start + 24, entry.mode);
        put_u32(&mut buf, start + 28, entry.stat.uid);
        put_u32(&mut buf, start + 32, entry.stat.gid);
        put_u32(&mut buf, start + 36, entry.stat.size);
        buf[start + 40..start + 60].copy_from_slice(&entry.oid.0);
        let flag = name.len().min(0x0fff) as u16;
        buf[start + 60..start + 62].copy_from_slice(&flag.to_be_bytes());
        buf[start + 62..start + 62 + name.len()].copy_from_slice(name);
    }
    let sum = sha1(&buf);
    buf.extend_from_slice(&sum);
    buf
}

fn u32be(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

pub fn stat_path(path: &Path) -> anyhow::Result<(u32, FileStat)> {
    let meta = fs::symlink_metadata(path)?;
    let size = meta.len();
    if size > u32::MAX as u64 {
        bail!("{} 超过索引可以记录的 4GiB", path.display());
    }
    let (mtime_s, mtime_n) = system_time_parts(meta.modified().ok());
    #[cfg(unix)]
    let (mode, stat) = {
        use std::os::unix::fs::MetadataExt;
        let mode = if meta.file_type().is_symlink() {
            super::object::MODE_LINK
        } else if meta.mode() & 0o111 != 0 {
            super::object::MODE_EXEC
        } else {
            super::object::MODE_BLOB
        };
        let (ctime_s, ctime_n) = unix_ctime(&meta);
        (
            mode,
            FileStat {
                ctime_s,
                ctime_n,
                mtime_s,
                mtime_n,
                dev: meta.dev() as u32,
                ino: meta.ino() as u32,
                uid: meta.uid(),
                gid: meta.gid(),
                size: size as u32,
            },
        )
    };
    #[cfg(not(unix))]
    let (mode, stat) = (
        super::object::MODE_BLOB,
        FileStat {
            ctime_s: mtime_s,
            ctime_n: mtime_n,
            mtime_s,
            mtime_n,
            size: size as u32,
            ..FileStat::default()
        },
    );
    Ok((mode, stat))
}

fn system_time_parts(time: Option<SystemTime>) -> (u32, u32) {
    let Some(time) = time else {
        return (0, 0);
    };
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as u32, d.subsec_nanos()),
        Err(_) => (0, 0),
    }
}

#[cfg(unix)]
fn unix_ctime(meta: &fs::Metadata) -> (u32, u32) {
    use std::os::unix::fs::MetadataExt;
    (meta.ctime() as u32, meta.ctime_nsec() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_roundtrip() {
        let index = Index {
            entries: vec![IndexEntry {
                path: "src/main.rs".into(),
                mode: 0o100644,
                oid: Oid::parse("ce013625030ba8dba906f756967f9e9ca394464a").unwrap(),
                stat: FileStat {
                    size: 6,
                    mtime_s: 10,
                    ..FileStat::default()
                },
            }],
        };
        let bytes = write_index(&index);
        let back = read_index(&bytes).unwrap();
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].path, "src/main.rs");
        assert_eq!(back.entries[0].mode, 0o100644);
        assert_eq!(back.entries[0].stat.size, 6);
        assert_eq!(back.entries[0].oid, index.entries[0].oid);
    }
}
