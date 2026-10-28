//! 读取 Git pack v2。`git gc` 会把 loose 对象打进 pack，这里扫描并解开 delta，
//! 这样同一仓库在 gc 之后仍然能被 ait 读取。

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context};
use flate2::read::ZlibDecoder;

use super::object::{hash_payload, Object};
use super::odb;
use super::oid::Oid;

const MAX_PACK: u64 = 512 * 1024 * 1024;

#[derive(Clone)]
struct Resolved {
    kind: &'static str,
    payload: Vec<u8>,
}

#[derive(Default)]
pub struct PackCache {
    objects: HashMap<Oid, Resolved>,
}

impl PackCache {
    pub fn load(git_dir: &Path) -> anyhow::Result<Self> {
        let dir = git_dir.join("objects").join("pack");
        let mut cache = PackCache::default();
        if !dir.exists() {
            return Ok(cache);
        }
        let mut raw_packs = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("pack") {
                continue;
            }
            let meta = entry.metadata()?;
            if meta.len() > MAX_PACK {
                bail!(
                    "pack 过大（{} 字节），超过 ait 当前 {} 字节上限",
                    meta.len(),
                    MAX_PACK
                );
            }
            raw_packs.push(parse_pack(&fs::read(&path)?)?);
        }
        resolve_all(git_dir, raw_packs, &mut cache)?;
        Ok(cache)
    }

    pub fn get(&self, oid: Oid) -> Option<anyhow::Result<Object>> {
        self.objects
            .get(&oid)
            .map(|item| Object::parse(item.kind, &item.payload))
    }

    pub fn oids(&self) -> Vec<Oid> {
        self.objects.keys().copied().collect()
    }
}

enum PackKind {
    Base(&'static str),
    Ofs(u64),
    Ref(Oid),
}

struct RawEntry {
    offset: u64,
    kind: PackKind,
    data: Vec<u8>,
}

fn parse_pack(data: &[u8]) -> anyhow::Result<Vec<RawEntry>> {
    if data.len() < 32 || &data[..4] != b"PACK" {
        bail!("不是 pack 文件");
    }
    let version = u32::from_be_bytes(data[4..8].try_into().unwrap());
    if version != 2 && version != 3 {
        bail!("不支持的 pack 版本 {version}");
    }
    let count = u32::from_be_bytes(data[8..12].try_into().unwrap()) as usize;
    let mut i = 12usize;
    let end = data.len() - 20;
    let mut entries = Vec::with_capacity(count);
    while entries.len() < count {
        if i >= end {
            bail!("pack 在读完对象前结束");
        }
        let offset = i as u64;
        let (kind_n, size, next) = read_type_size(data, i)?;
        i = next;
        let kind = match kind_n {
            1 => PackKind::Base("commit"),
            2 => PackKind::Base("tree"),
            3 => PackKind::Base("blob"),
            4 => PackKind::Base("tag"),
            6 => {
                let (ofs, next) = read_ofs(data, i)?;
                if ofs > offset {
                    bail!("ofs delta 偏移无效");
                }
                i = next;
                PackKind::Ofs(offset - ofs)
            }
            7 => {
                if i + 20 > data.len() {
                    bail!("ref delta 被截断");
                }
                let mut oid = [0u8; 20];
                oid.copy_from_slice(&data[i..i + 20]);
                i += 20;
                PackKind::Ref(Oid(oid))
            }
            other => bail!("未知 pack 对象类型 {other}"),
        };
        let mut dec = ZlibDecoder::new(&data[i..]);
        let mut inflated = Vec::new();
        dec.read_to_end(&mut inflated).context("解压 pack 对象")?;
        i += dec.total_in() as usize;
        if let PackKind::Base(_) = kind {
            if inflated.len() != size {
                bail!("pack 对象大小不符");
            }
        }
        entries.push(RawEntry {
            offset,
            kind,
            data: inflated,
        });
    }
    Ok(entries)
}

fn read_type_size(data: &[u8], mut i: usize) -> anyhow::Result<(u8, usize, usize)> {
    if i >= data.len() {
        bail!("pack 头被截断");
    }
    let c = data[i];
    i += 1;
    let kind = (c >> 4) & 7;
    let mut size = (c & 15) as usize;
    let mut shift = 4;
    let mut cur = c;
    while cur & 0x80 != 0 {
        if i >= data.len() {
            bail!("pack 大小被截断");
        }
        cur = data[i];
        i += 1;
        size |= ((cur & 0x7f) as usize) << shift;
        shift += 7;
    }
    Ok((kind, size, i))
}

fn read_ofs(data: &[u8], mut i: usize) -> anyhow::Result<(u64, usize)> {
    if i >= data.len() {
        bail!("ofs delta 被截断");
    }
    let mut c = data[i];
    i += 1;
    let mut ofs = (c & 0x7f) as u64;
    while c & 0x80 != 0 {
        if i >= data.len() {
            bail!("ofs delta 被截断");
        }
        c = data[i];
        i += 1;
        ofs = ((ofs + 1) << 7) | (c & 0x7f) as u64;
    }
    Ok((ofs, i))
}

fn resolve_all(
    git_dir: &Path,
    packs: Vec<Vec<RawEntry>>,
    cache: &mut PackCache,
) -> anyhow::Result<()> {
    struct Pending {
        offset_key: u64,
        kind: PackKind,
        data: Vec<u8>,
    }
    let mut pending = Vec::new();
    let mut by_offset: HashMap<u64, Oid> = HashMap::new();
    for (pi, pack) in packs.into_iter().enumerate() {
        for entry in pack {
            let key = ((pi as u64) << 48) | entry.offset;
            let kind = match entry.kind {
                PackKind::Base(k) => PackKind::Base(k),
                PackKind::Ofs(abs) => PackKind::Ofs(((pi as u64) << 48) | abs),
                PackKind::Ref(oid) => PackKind::Ref(oid),
            };
            pending.push(Pending {
                offset_key: key,
                kind,
                data: entry.data,
            });
        }
    }

    let mut guard = 0;
    while !pending.is_empty() {
        guard += 1;
        if guard > pending.len() + 2 {
            bail!("pack 里有无法解开的 delta（可能引用了缺失的基对象）");
        }
        let before = pending.len();
        let mut next = Vec::new();
        for entry in pending {
            match &entry.kind {
                PackKind::Base(kind) => {
                    let (oid, _) = hash_payload(kind, &entry.data);
                    by_offset.insert(entry.offset_key, oid);
                    cache.objects.insert(
                        oid,
                        Resolved {
                            kind,
                            payload: entry.data,
                        },
                    );
                }
                PackKind::Ofs(base_key) => {
                    let Some(base_oid) = by_offset.get(base_key).copied() else {
                        next.push(entry);
                        continue;
                    };
                    let base = cache.objects.get(&base_oid).context("delta 基对象丢失")?;
                    let payload = apply_delta(&base.payload, &entry.data)?;
                    let kind = base.kind;
                    let (oid, _) = hash_payload(kind, &payload);
                    by_offset.insert(entry.offset_key, oid);
                    cache.objects.insert(oid, Resolved { kind, payload });
                }
                PackKind::Ref(base_oid) => {
                    let base = if let Some(found) = cache.objects.get(base_oid) {
                        Some(found.clone())
                    } else if let Some(raw) = odb::read_loose(git_dir, *base_oid)? {
                        let (kind, payload) = super::object::split_raw(&raw)?;
                        Some(Resolved {
                            kind: match kind {
                                "commit" => "commit",
                                "tree" => "tree",
                                "blob" => "blob",
                                "tag" => "tag",
                                _ => "blob",
                            },
                            payload: payload.to_vec(),
                        })
                    } else {
                        None
                    };
                    let Some(base) = base else {
                        next.push(entry);
                        continue;
                    };
                    let payload = apply_delta(&base.payload, &entry.data)?;
                    let (oid, _) = hash_payload(base.kind, &payload);
                    by_offset.insert(entry.offset_key, oid);
                    cache.objects.insert(
                        oid,
                        Resolved {
                            kind: base.kind,
                            payload,
                        },
                    );
                }
            }
        }
        if next.len() == before {
            bail!("pack delta 没有进展，仍有 {} 个对象未解开", next.len());
        }
        pending = next;
        guard = 0;
    }
    Ok(())
}

pub fn apply_delta(base: &[u8], delta: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut i = 0;
    let base_size = read_varint(delta, &mut i)?;
    let result_size = read_varint(delta, &mut i)?;
    if base_size != base.len() {
        bail!(
            "delta 基对象大小不符: 声明 {base_size}，实际 {}",
            base.len()
        );
    }
    let mut out = Vec::with_capacity(result_size);
    while i < delta.len() {
        let cmd = delta[i];
        i += 1;
        if cmd & 0x80 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for bit in 0..4 {
                if cmd & (1 << bit) != 0 {
                    if i >= delta.len() {
                        bail!("delta 复制指令被截断");
                    }
                    offset |= (delta[i] as usize) << (8 * bit);
                    i += 1;
                }
            }
            for bit in 0..3 {
                if cmd & (1 << (4 + bit)) != 0 {
                    if i >= delta.len() {
                        bail!("delta 复制指令被截断");
                    }
                    size |= (delta[i] as usize) << (8 * bit);
                    i += 1;
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            let end = offset + size;
            if end > base.len() {
                bail!("delta 复制越界");
            }
            out.extend_from_slice(&base[offset..end]);
        } else if cmd != 0 {
            let n = cmd as usize;
            if i + n > delta.len() {
                bail!("delta 插入被截断");
            }
            out.extend_from_slice(&delta[i..i + n]);
            i += n;
        } else {
            bail!("delta 出现非法指令 0");
        }
    }
    if out.len() != result_size {
        bail!("delta 结果大小不符: 声明 {result_size}，实际 {}", out.len());
    }
    Ok(out)
}

fn read_varint(data: &[u8], i: &mut usize) -> anyhow::Result<usize> {
    let mut n = 0usize;
    let mut shift = 0;
    loop {
        if *i >= data.len() {
            bail!("变长整数被截断");
        }
        let b = data[*i];
        *i += 1;
        n |= ((b & 0x7f) as usize) << shift;
        if b & 0x80 == 0 {
            return Ok(n);
        }
        shift += 7;
        if shift > 28 {
            bail!("变长整数过长");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_copy_and_insert() {
        let base = b"hello";
        let mut delta = Vec::new();
        delta.extend(encode_size(base.len()));
        delta.extend(encode_size(6));
        delta.push(0x90);
        delta.push(5);
        delta.push(1);
        delta.push(b'!');
        let out = apply_delta(base, &delta).unwrap();
        assert_eq!(out, b"hello!");
    }

    fn encode_size(mut n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        loop {
            let mut b = (n & 0x7f) as u8;
            n >>= 7;
            if n > 0 {
                b |= 0x80;
            }
            v.push(b);
            if n == 0 {
                break;
            }
        }
        v
    }
}
