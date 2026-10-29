//! Git pack v2 的读写。有 `.idx` 时按索引随用随读；没有索引的 pack 整个扫描一遍。
//! 写出时生成 pack v2 + idx v2，相近对象之间用 OFS_DELTA 压缩。

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{bail, Context};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::{Compression, Crc};

use super::object::{hash_payload, sha1, Object};
use super::odb;
use super::oid::Oid;

const MAX_PACK: u64 = 2 * 1024 * 1024 * 1024;
const MAX_DELTA_DEPTH: usize = 50;

#[derive(Clone)]
struct Resolved {
    kind: &'static str,
    payload: Rc<Vec<u8>>,
}

struct IndexedPack {
    path: PathBuf,
    entries: Vec<(Oid, u64)>,
    data: OnceCell<Vec<u8>>,
}

impl IndexedPack {
    fn find(&self, oid: Oid) -> Option<u64> {
        self.entries
            .binary_search_by(|(o, _)| o.0.cmp(&oid.0))
            .ok()
            .map(|i| self.entries[i].1)
    }

    fn data(&self) -> anyhow::Result<&[u8]> {
        if let Some(data) = self.data.get() {
            return Ok(data);
        }
        let bytes = fs::read(&self.path).with_context(|| format!("读取 {}", self.path.display()))?;
        if bytes.len() < 32 || &bytes[..4] != b"PACK" {
            bail!("{} 不是 pack 文件", self.path.display());
        }
        Ok(self.data.get_or_init(|| bytes))
    }
}

pub struct PackCache {
    git_dir: PathBuf,
    indexed: Vec<IndexedPack>,
    scanned: HashMap<Oid, Resolved>,
    recent: RefCell<HashMap<(usize, u64), Resolved>>,
}

impl PackCache {
    pub fn load(git_dir: &Path) -> anyhow::Result<Self> {
        let dir = git_dir.join("objects").join("pack");
        let mut cache = PackCache {
            git_dir: git_dir.to_path_buf(),
            indexed: Vec::new(),
            scanned: HashMap::new(),
            recent: RefCell::new(HashMap::new()),
        };
        if !dir.exists() {
            return Ok(cache);
        }
        let mut raw_packs = Vec::new();
        let mut names: Vec<PathBuf> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("pack"))
            .collect();
        names.sort();
        for path in names {
            let meta = fs::metadata(&path)?;
            if meta.len() > MAX_PACK {
                bail!("pack 过大（{} 字节），超过 ait 当前 {} 字节上限", meta.len(), MAX_PACK);
            }
            let idx = path.with_extension("idx");
            if let Ok(bytes) = fs::read(&idx) {
                if let Ok(entries) = parse_idx(&bytes) {
                    cache.indexed.push(IndexedPack {
                        path,
                        entries,
                        data: OnceCell::new(),
                    });
                    continue;
                }
            }
            raw_packs.push(parse_pack(&fs::read(&path)?)?);
        }
        if !raw_packs.is_empty() {
            let git = git_dir.to_path_buf();
            let resolved = resolve_entries(raw_packs, &|oid| {
                odb::read_loose(&git, oid)
                    .ok()
                    .flatten()
                    .and_then(|raw| {
                        let (kind, payload) = super::object::split_raw(&raw).ok()?;
                        Some((static_kind(kind)?, payload.to_vec()))
                    })
            })?;
            for (oid, kind, payload) in resolved {
                cache.scanned.insert(
                    oid,
                    Resolved {
                        kind,
                        payload: Rc::new(payload),
                    },
                );
            }
        }
        Ok(cache)
    }

    pub fn get(&self, oid: Oid) -> Option<anyhow::Result<Object>> {
        self.raw(oid)
            .map(|found| found.and_then(|(kind, payload)| Object::parse(kind, &payload)))
    }

    /// 对象类型和原始内容。
    pub fn raw(&self, oid: Oid) -> Option<anyhow::Result<(&'static str, Rc<Vec<u8>>)>> {
        if let Some(found) = self.scanned.get(&oid) {
            return Some(Ok((found.kind, found.payload.clone())));
        }
        for (index, pack) in self.indexed.iter().enumerate() {
            if let Some(offset) = pack.find(oid) {
                return Some(
                    self.read_at(index, offset, 0)
                        .map(|r| (r.kind, r.payload))
                        .with_context(|| format!("从 pack 读取 {oid}")),
                );
            }
        }
        None
    }

    pub fn contains(&self, oid: Oid) -> bool {
        self.scanned.contains_key(&oid) || self.indexed.iter().any(|p| p.find(oid).is_some())
    }

    pub fn oids(&self) -> Vec<Oid> {
        let mut out: Vec<Oid> = self.scanned.keys().copied().collect();
        for pack in &self.indexed {
            out.extend(pack.entries.iter().map(|(o, _)| *o));
        }
        out
    }

    pub fn pack_paths(&self) -> Vec<PathBuf> {
        self.indexed.iter().map(|p| p.path.clone()).collect()
    }

    fn read_at(&self, pack: usize, offset: u64, depth: usize) -> anyhow::Result<Resolved> {
        if depth > MAX_DELTA_DEPTH {
            bail!("delta 链过长");
        }
        if let Some(hit) = self.recent.borrow().get(&(pack, offset)) {
            return Ok(hit.clone());
        }
        let data = self.indexed[pack].data()?;
        let start = offset as usize;
        if start >= data.len() {
            bail!("pack 偏移越界");
        }
        let (kind_n, size, mut i) = read_type_size(data, start)?;
        let resolved = match kind_n {
            1..=4 => {
                let payload = inflate(&data[i..], size)?.0;
                Resolved {
                    kind: ["commit", "tree", "blob", "tag"][kind_n as usize - 1],
                    payload: Rc::new(payload),
                }
            }
            6 => {
                let (ofs, next) = read_ofs(data, i)?;
                i = next;
                if ofs > offset {
                    bail!("ofs delta 偏移无效");
                }
                let base = self.read_at(pack, offset - ofs, depth + 1)?;
                let delta = inflate(&data[i..], size)?.0;
                Resolved {
                    kind: base.kind,
                    payload: Rc::new(apply_delta(&base.payload, &delta)?),
                }
            }
            7 => {
                if i + 20 > data.len() {
                    bail!("ref delta 被截断");
                }
                let mut raw = [0u8; 20];
                raw.copy_from_slice(&data[i..i + 20]);
                i += 20;
                let base_oid = Oid(raw);
                let (kind, base) = self.lookup_any(base_oid, depth + 1)?;
                let delta = inflate(&data[i..], size)?.0;
                Resolved {
                    kind,
                    payload: Rc::new(apply_delta(&base, &delta)?),
                }
            }
            other => bail!("未知 pack 对象类型 {other}"),
        };
        let mut recent = self.recent.borrow_mut();
        if recent.len() > 512 {
            recent.clear();
        }
        recent.insert((pack, offset), resolved.clone());
        Ok(resolved)
    }

    fn lookup_any(&self, oid: Oid, depth: usize) -> anyhow::Result<(&'static str, Rc<Vec<u8>>)> {
        if let Some(found) = self.scanned.get(&oid) {
            return Ok((found.kind, found.payload.clone()));
        }
        for (index, pack) in self.indexed.iter().enumerate() {
            if let Some(offset) = pack.find(oid) {
                let r = self.read_at(index, offset, depth)?;
                return Ok((r.kind, r.payload));
            }
        }
        if let Some(raw) = odb::read_loose(&self.git_dir, oid)? {
            let (kind, payload) = super::object::split_raw(&raw)?;
            return Ok((static_kind(kind).context("未知对象类型")?, Rc::new(payload.to_vec())));
        }
        bail!("delta 基对象 {oid} 不存在")
    }
}

pub fn static_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "commit" => Some("commit"),
        "tree" => Some("tree"),
        "blob" => Some("blob"),
        "tag" => Some("tag"),
        _ => None,
    }
}

fn inflate(data: &[u8], size: usize) -> anyhow::Result<(Vec<u8>, usize)> {
    let mut dec = ZlibDecoder::new(data);
    let mut out = Vec::with_capacity(size);
    dec.read_to_end(&mut out).context("解压 pack 对象")?;
    if out.len() != size {
        bail!("pack 对象大小不符");
    }
    Ok((out, dec.total_in() as usize))
}

fn parse_idx(bytes: &[u8]) -> anyhow::Result<Vec<(Oid, u64)>> {
    if bytes.len() < 8 + 1024 + 40 || bytes[..4] != [0xff, b't', b'O', b'c'] {
        bail!("不是 idx v2");
    }
    if u32::from_be_bytes(bytes[4..8].try_into().unwrap()) != 2 {
        bail!("不支持的 idx 版本");
    }
    let count = u32::from_be_bytes(bytes[8 + 255 * 4..8 + 256 * 4].try_into().unwrap()) as usize;
    let names = 8 + 1024;
    let crcs = names + count * 20;
    let offsets = crcs + count * 4;
    let large = offsets + count * 4;
    if bytes.len() < large + 40 {
        bail!("idx 被截断");
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let mut raw = [0u8; 20];
        raw.copy_from_slice(&bytes[names + i * 20..names + i * 20 + 20]);
        let small = u32::from_be_bytes(bytes[offsets + i * 4..offsets + i * 4 + 4].try_into().unwrap());
        let offset = if small & 0x8000_0000 != 0 {
            let at = large + ((small & 0x7fff_ffff) as usize) * 8;
            if at + 8 > bytes.len() {
                bail!("idx 大偏移越界");
            }
            u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
        } else {
            small as u64
        };
        out.push((Oid(raw), offset));
    }
    Ok(out)
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
    let body_end = data.len() - 20;
    if sha1(&data[..body_end])[..] != data[body_end..] {
        bail!("pack 校验和不符");
    }
    let count = u32::from_be_bytes(data[8..12].try_into().unwrap()) as usize;
    let mut i = 12usize;
    let mut entries = Vec::with_capacity(count.min(1 << 20));
    while entries.len() < count {
        if i >= body_end {
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
        let (inflated, used) = inflate(&data[i..body_end], size)?;
        i += used;
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
        if i >= data.len() || shift > 56 {
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
        if i >= data.len() || ofs > (u64::MAX >> 8) {
            bail!("ofs delta 被截断");
        }
        c = data[i];
        i += 1;
        ofs = ((ofs + 1) << 7) | (c & 0x7f) as u64;
    }
    Ok((ofs, i))
}

/// 解开一个完整的 pack（允许 thin pack：REF_DELTA 的基对象可以由 `lookup` 从仓库里提供）。
pub fn unpack(
    data: &[u8],
    lookup: &dyn Fn(Oid) -> Option<(&'static str, Vec<u8>)>,
) -> anyhow::Result<Vec<(Oid, &'static str, Vec<u8>)>> {
    let entries = parse_pack(data)?;
    resolve_entries(vec![entries], lookup)
}

fn resolve_entries(
    packs: Vec<Vec<RawEntry>>,
    lookup: &dyn Fn(Oid) -> Option<(&'static str, Vec<u8>)>,
) -> anyhow::Result<Vec<(Oid, &'static str, Vec<u8>)>> {
    struct Pending {
        key: u64,
        kind: PackKind,
        data: Vec<u8>,
    }
    let mut pending = Vec::new();
    let mut by_offset: HashMap<u64, Oid> = HashMap::new();
    let mut objects: HashMap<Oid, (&'static str, Rc<Vec<u8>>)> = HashMap::new();
    let mut order = Vec::new();
    for (pi, pack) in packs.into_iter().enumerate() {
        for entry in pack {
            let key = ((pi as u64) << 48) | entry.offset;
            let kind = match entry.kind {
                PackKind::Base(k) => PackKind::Base(k),
                PackKind::Ofs(abs) => PackKind::Ofs(((pi as u64) << 48) | abs),
                PackKind::Ref(oid) => PackKind::Ref(oid),
            };
            pending.push(Pending {
                key,
                kind,
                data: entry.data,
            });
        }
    }
    while !pending.is_empty() {
        let before = pending.len();
        let mut next = Vec::new();
        for entry in pending {
            let (kind, payload) = match &entry.kind {
                PackKind::Base(kind) => (*kind, entry.data),
                PackKind::Ofs(base_key) => {
                    let Some(base_oid) = by_offset.get(base_key).copied() else {
                        next.push(entry);
                        continue;
                    };
                    let (kind, base) = objects.get(&base_oid).cloned().context("delta 基对象丢失")?;
                    (kind, apply_delta(&base, &entry.data)?)
                }
                PackKind::Ref(base_oid) => {
                    let base = match objects.get(base_oid) {
                        Some((kind, payload)) => Some((*kind, payload.as_ref().clone())),
                        None => lookup(*base_oid),
                    };
                    let Some((kind, base)) = base else {
                        next.push(entry);
                        continue;
                    };
                    (kind, apply_delta(&base, &entry.data)?)
                }
            };
            let (oid, _) = hash_payload(kind, &payload);
            by_offset.insert(entry.key, oid);
            if !objects.contains_key(&oid) {
                order.push(oid);
            }
            objects.insert(oid, (kind, Rc::new(payload)));
        }
        if next.len() == before {
            bail!("pack 里有 {} 个 delta 找不到基对象", next.len());
        }
        pending = next;
    }
    Ok(order
        .into_iter()
        .map(|oid| {
            let (kind, payload) = objects.remove(&oid).expect("刚插入");
            (oid, kind, Rc::try_unwrap(payload).unwrap_or_else(|rc| rc.as_ref().clone()))
        })
        .collect())
}

pub fn apply_delta(base: &[u8], delta: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut i = 0;
    let base_size = read_varint(delta, &mut i)?;
    let result_size = read_varint(delta, &mut i)?;
    if base_size != base.len() {
        bail!("delta 基对象大小不符: 声明 {base_size}，实际 {}", base.len());
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
        if shift > 56 {
            bail!("变长整数过长");
        }
    }
}

// ---------- 写 pack ----------

/// 待打包的对象。`hint` 是路径之类的名字，名字相近的对象更可能互为 delta。
pub struct PackItem {
    pub oid: Oid,
    pub kind: &'static str,
    pub payload: Vec<u8>,
    pub hint: String,
}

pub struct WrittenPack {
    pub pack: Vec<u8>,
    pub idx: Vec<u8>,
    pub checksum: [u8; 20],
}

pub fn write_pack(mut items: Vec<PackItem>, deltas: bool) -> anyhow::Result<WrittenPack> {
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| seen.insert(item.oid));
    let type_rank = |kind: &str| match kind {
        "commit" => 0,
        "tag" => 1,
        "tree" => 2,
        _ => 3,
    };
    items.sort_by(|a, b| {
        type_rank(a.kind)
            .cmp(&type_rank(b.kind))
            .then_with(|| name_hash(&a.hint).cmp(&name_hash(&b.hint)))
            .then_with(|| b.payload.len().cmp(&a.payload.len()))
    });
    let mut pack = Vec::new();
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&(items.len() as u32).to_be_bytes());
    let mut offsets: Vec<u64> = Vec::with_capacity(items.len());
    let mut depth: Vec<usize> = Vec::with_capacity(items.len());
    let mut index_rows: Vec<(Oid, u32, u64)> = Vec::with_capacity(items.len());
    const WINDOW: usize = 10;
    let mut indexes: HashMap<usize, DeltaIndex> = HashMap::new();
    for (n, item) in items.iter().enumerate() {
        let offset = pack.len() as u64;
        let mut best: Option<(usize, Vec<u8>)> = None;
        let can_delta = deltas
            && matches!(item.kind, "blob" | "tree")
            && item.payload.len() >= 64
            && item.payload.len() <= 32 * 1024 * 1024;
        if can_delta {
            indexes.retain(|k, _| *k + WINDOW >= n);
            for back in 1..=WINDOW.min(n) {
                let base_n = n - back;
                let base = &items[base_n];
                if base.kind != item.kind || depth[base_n] >= 16 {
                    continue;
                }
                if base.payload.len() < item.payload.len() / 4 || base.payload.len() > item.payload.len() * 4 {
                    continue;
                }
                let limit = best
                    .as_ref()
                    .map(|(_, d)| d.len())
                    .unwrap_or(item.payload.len() / 2);
                if base.payload.len() < BLOCK {
                    continue;
                }
                let index = indexes
                    .entry(base_n)
                    .or_insert_with(|| DeltaIndex::new(&base.payload));
                if let Some(delta) = delta_with(index, &base.payload, &item.payload, limit) {
                    best = Some((base_n, delta));
                }
            }
        }
        let start = pack.len();
        match &best {
            Some((base_n, delta)) => {
                write_header(&mut pack, 6, delta.len());
                write_ofs(&mut pack, offset - offsets[*base_n]);
                pack.extend_from_slice(&deflate(delta)?);
                depth.push(depth[*base_n] + 1);
            }
            None => {
                let code = match item.kind {
                    "commit" => 1,
                    "tree" => 2,
                    "blob" => 3,
                    "tag" => 4,
                    other => bail!("不能打包 {other}"),
                };
                write_header(&mut pack, code, item.payload.len());
                pack.extend_from_slice(&deflate(&item.payload)?);
                depth.push(0);
            }
        }
        let mut crc = Crc::new();
        crc.update(&pack[start..]);
        offsets.push(offset);
        index_rows.push((item.oid, crc.sum(), offset));
    }
    let checksum = sha1(&pack);
    pack.extend_from_slice(&checksum);
    let idx = write_idx(index_rows, &checksum);
    Ok(WrittenPack { pack, idx, checksum })
}

fn name_hash(name: &str) -> u32 {
    let mut hash = 0u32;
    for byte in name.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        hash = (hash >> 2).wrapping_add((byte as u32) << 24);
    }
    hash
}

fn deflate(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

fn write_header(out: &mut Vec<u8>, kind: u8, size: usize) {
    let mut size = size;
    let mut byte = (kind << 4) | (size & 15) as u8;
    size >>= 4;
    while size > 0 {
        out.push(byte | 0x80);
        byte = (size & 0x7f) as u8;
        size >>= 7;
    }
    out.push(byte);
}

fn write_ofs(out: &mut Vec<u8>, mut ofs: u64) {
    let mut buf = vec![(ofs & 0x7f) as u8];
    ofs >>= 7;
    while ofs > 0 {
        ofs -= 1;
        buf.push(0x80 | (ofs & 0x7f) as u8);
        ofs >>= 7;
    }
    buf.reverse();
    out.extend_from_slice(&buf);
}

fn write_idx(mut rows: Vec<(Oid, u32, u64)>, pack_checksum: &[u8; 20]) -> Vec<u8> {
    rows.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
    let mut out = Vec::new();
    out.extend_from_slice(&[0xff, b't', b'O', b'c']);
    out.extend_from_slice(&2u32.to_be_bytes());
    let mut fanout = [0u32; 256];
    for (oid, _, _) in &rows {
        fanout[oid.0[0] as usize] += 1;
    }
    let mut total = 0u32;
    for slot in fanout.iter_mut() {
        total += *slot;
        *slot = total;
    }
    for value in fanout {
        out.extend_from_slice(&value.to_be_bytes());
    }
    for (oid, _, _) in &rows {
        out.extend_from_slice(&oid.0);
    }
    for (_, crc, _) in &rows {
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let mut large = Vec::new();
    for (_, _, offset) in &rows {
        if *offset >= 0x8000_0000 {
            out.extend_from_slice(&(0x8000_0000u32 | large.len() as u32).to_be_bytes());
            large.push(*offset);
        } else {
            out.extend_from_slice(&(*offset as u32).to_be_bytes());
        }
    }
    for offset in large {
        out.extend_from_slice(&offset.to_be_bytes());
    }
    out.extend_from_slice(pack_checksum);
    let checksum = sha1(&out);
    out.extend_from_slice(&checksum);
    out
}

const BLOCK: usize = 16;

struct DeltaIndex {
    blocks: HashMap<u64, Vec<u32>>,
}

impl DeltaIndex {
    fn new(base: &[u8]) -> Self {
        let mut blocks: HashMap<u64, Vec<u32>> = HashMap::new();
        let mut i = 0;
        while i + BLOCK <= base.len() {
            let list = blocks.entry(block_hash(&base[i..i + BLOCK])).or_default();
            if list.len() < 64 {
                list.push(i as u32);
            }
            i += BLOCK;
        }
        DeltaIndex { blocks }
    }
}

fn block_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// 生成 `target` 相对 `base` 的 Git delta；结果不小于 `limit` 时放弃。
pub fn make_delta(base: &[u8], target: &[u8], limit: usize) -> Option<Vec<u8>> {
    if base.len() < BLOCK {
        return None;
    }
    delta_with(&DeltaIndex::new(base), base, target, limit)
}

fn delta_with(index: &DeltaIndex, base: &[u8], target: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    push_varint(&mut out, base.len());
    push_varint(&mut out, target.len());
    let mut literal: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < target.len() {
        let mut best_len = 0;
        let mut best_off = 0;
        if i + BLOCK <= target.len() {
            if let Some(cands) = index.blocks.get(&block_hash(&target[i..i + BLOCK])) {
                for &cand in cands {
                    let cand = cand as usize;
                    let mut len = 0;
                    while cand + len < base.len()
                        && i + len < target.len()
                        && base[cand + len] == target[i + len]
                        && len < 0xff_ffff
                    {
                        len += 1;
                    }
                    if len > best_len {
                        best_len = len;
                        best_off = cand;
                    }
                }
            }
        }
        if best_len >= BLOCK {
            while !literal.is_empty() && best_off > 0 && base[best_off - 1] == *literal.last().unwrap() {
                literal.pop();
                best_off -= 1;
                best_len += 1;
                i -= 1;
            }
            flush_literal(&mut out, &mut literal);
            push_copy(&mut out, best_off, best_len);
            i += best_len;
        } else {
            literal.push(target[i]);
            if literal.len() == 127 {
                flush_literal(&mut out, &mut literal);
            }
            i += 1;
        }
        if out.len() + literal.len() >= limit {
            return None;
        }
    }
    flush_literal(&mut out, &mut literal);
    (out.len() < limit).then_some(out)
}

fn push_varint(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut byte = (n & 0x7f) as u8;
        n >>= 7;
        if n > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if n == 0 {
            break;
        }
    }
}

fn flush_literal(out: &mut Vec<u8>, literal: &mut Vec<u8>) {
    for chunk in literal.chunks(127) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    literal.clear();
}

fn push_copy(out: &mut Vec<u8>, offset: usize, len: usize) {
    let mut remaining = len;
    let mut offset = offset;
    while remaining > 0 {
        let size = remaining.min(0xff_ffff);
        let mut cmd = 0x80u8;
        let mut args = Vec::new();
        for bit in 0..4 {
            let byte = ((offset >> (8 * bit)) & 0xff) as u8;
            if byte != 0 {
                cmd |= 1 << bit;
                args.push(byte);
            }
        }
        if size != 0x10000 {
            for bit in 0..3 {
                let byte = ((size >> (8 * bit)) & 0xff) as u8;
                if byte != 0 {
                    cmd |= 1 << (4 + bit);
                    args.push(byte);
                }
            }
        }
        out.push(cmd);
        out.extend_from_slice(&args);
        offset += size;
        remaining -= size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_copy_and_insert() {
        let base = b"hello";
        let mut delta = Vec::new();
        push_varint(&mut delta, base.len());
        push_varint(&mut delta, 6);
        delta.push(0x90);
        delta.push(5);
        delta.push(1);
        delta.push(b'!');
        let out = apply_delta(base, &delta).unwrap();
        assert_eq!(out, b"hello!");
    }

    #[test]
    fn made_delta_round_trips() {
        let base: Vec<u8> = (0..4000u32).flat_map(|i| format!("line {i}\n").into_bytes()).collect();
        let mut target = base.clone();
        target.splice(1000..1010, b"CHANGED!!".iter().copied());
        target.extend_from_slice(b"tail\n");
        let delta = make_delta(&base, &target, target.len()).unwrap();
        assert!(delta.len() < 200, "delta {} bytes", delta.len());
        assert_eq!(apply_delta(&base, &delta).unwrap(), target);
    }

    #[test]
    fn written_pack_reads_back() {
        let a: Vec<u8> = (0..500u32).flat_map(|i| format!("row {i}\n").into_bytes()).collect();
        let mut b = a.clone();
        b.extend_from_slice(b"more\n");
        let items: Vec<PackItem> = [a.clone(), b.clone()]
            .into_iter()
            .map(|payload| PackItem {
                oid: hash_payload("blob", &payload).0,
                kind: "blob",
                payload,
                hint: "f.txt".into(),
            })
            .collect();
        let written = write_pack(items, true).unwrap();
        let objects = unpack(&written.pack, &|_| None).unwrap();
        assert_eq!(objects.len(), 2);
        let entries = parse_idx(&written.idx).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(objects.iter().any(|(_, _, p)| *p == b));
    }
}
