use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

use super::object::{hash_payload, split_raw, Object};
use super::oid::Oid;

pub fn loose_path(git_dir: &Path, oid: Oid) -> PathBuf {
    let hex = oid.to_hex();
    git_dir.join("objects").join(&hex[..2]).join(&hex[2..])
}

pub fn read_loose(git_dir: &Path, oid: Oid) -> anyhow::Result<Option<Vec<u8>>> {
    let path = loose_path(git_dir, oid);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("读取对象 {}", oid))?;
    let mut dec = ZlibDecoder::new(&bytes[..]);
    let mut raw = Vec::new();
    dec.read_to_end(&mut raw)
        .with_context(|| format!("解压对象 {}", oid))?;
    Ok(Some(raw))
}

pub fn write_loose(git_dir: &Path, kind: &str, payload: &[u8]) -> anyhow::Result<Oid> {
    let (oid, raw) = hash_payload(kind, payload);
    let path = loose_path(git_dir, oid);
    if path.exists() {
        return Ok(oid);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw)?;
    let compressed = enc.finish()?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, compressed)?;
    fs::rename(&tmp, &path)?;
    Ok(oid)
}

pub fn parse_stored(raw: &[u8]) -> anyhow::Result<Object> {
    let (kind, payload) = split_raw(raw)?;
    Object::parse(kind, payload)
}
