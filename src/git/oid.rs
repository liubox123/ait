use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Git 对象名，SHA-1，20 字节。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    pub const ZERO: Oid = Oid([0u8; 20]);

    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut oid = [0u8; 20];
        let n = bytes.len().min(20);
        oid[..n].copy_from_slice(&bytes[..n]);
        Oid(oid)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        if text.len() != 40 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!("无效的对象名: {text}");
        }
        let raw = hex::decode(text)?;
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&raw);
        Ok(Oid(oid))
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    pub fn short(self) -> String {
        self.to_hex()[..7].to_string()
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.short())
    }
}

impl FromStr for Oid {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Oid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Oid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Oid::parse(&s).map_err(serde::de::Error::custom)
    }
}
