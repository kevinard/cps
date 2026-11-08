use std::{cmp::Ordering, fmt, str::FromStr, sync::Arc};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Version(Arc<str>);

impl FromStr for Version {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (epoch, release) = value.split_once(':').unwrap_or(("0", value));
        ensure!(
            value.len() <= 256
                && is_numeric(epoch)
                && release.starts_with(|ch: char| ch.is_ascii_digit())
                && release.split(['.', '+', '_', '~', '-']).all(|part| {
                    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
                }),
            "invalid package version {value:?}; expected [epoch:]release[-revision]"
        );
        Ok(Self(Arc::from(value)))
    }
}

impl Version {
    pub(crate) fn next_source_revision(&self) -> Result<Self> {
        let (_, release, revision) = self.parts();
        let (prefix, tail) = revision.rsplit_once('.').unwrap_or(("", revision));
        let next = tail
            .parse::<u64>()?
            .checked_add(1)
            .context("Git package revision exhausted")?;
        let epoch = self
            .0
            .split_once(':')
            .map_or(String::new(), |(epoch, _)| format!("{epoch}:"));
        let dot = if prefix.is_empty() { "" } else { "." };
        format!("{epoch}{release}-{prefix}{dot}{next}").parse()
    }

    fn parts(&self) -> (&str, &str, &str) {
        let (epoch, release) = self.0.split_once(':').unwrap_or(("0", &self.0));
        let (upstream, revision) = release
            .rsplit_once('-')
            .filter(|(_, revision)| revision.split('.').all(is_numeric))
            .unwrap_or((release, "0"));
        (epoch, upstream, revision)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let (epoch, release, revision) = self.parts();
        let (other_epoch, other_release, other_revision) = other.parts();
        cmp_numeric(epoch, other_epoch)
            .then_with(|| cmp_release(release, other_release))
            .then_with(|| cmp_release(revision, other_revision))
            // Preserve distinct package IDs when their numeric components tie.
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn is_numeric(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
}

fn cmp_numeric(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn cmp_release(mut a: &str, mut b: &str) -> Ordering {
    loop {
        let left = a.as_bytes().first().copied();
        let right = b.as_bytes().first().copied();
        let order = match (left, right) {
            (None, None) => return Ordering::Equal,
            (Some(l), Some(r)) if l.is_ascii_digit() && r.is_ascii_digit() => {
                let end_a = a.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(a.len());
                let end_b = b.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(b.len());
                let order = cmp_numeric(&a[..end_a], &b[..end_b]);
                a = &a[end_a..];
                b = &b[end_b..];
                order
            }
            _ => {
                let order = byte_rank(left).cmp(&byte_rank(right));
                if order == Ordering::Equal {
                    a = &a[1..];
                    b = &b[1..];
                }
                order
            }
        };
        if order != Ordering::Equal {
            return order;
        }
    }
}

fn byte_rank(byte: Option<u8>) -> u8 {
    match byte {
        Some(b'~') => 0,
        Some(b'-') => 1,
        None => 2,
        Some(byte) => byte,
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_legacy_versions_and_imported_releases() -> Result<()> {
        let ordered = [
            "1.2.3-2",
            "1.2.3-10",
            "1.2.10-0",
            "2.44",
            "2.44+r9+g1848099f063e-1",
            "2.44+r50+g1848099f063e-1",
            "3.0.0~rc2-1",
            "3.0.0~rc10-1",
            "3.0.0-rc.1-1",
            "3.0.0-1",
            "3.0.0-1.1",
            "7.2.14.arch1-1",
            "262.1-1",
            "1:1.3.2-4",
            "10:1.3.2-1",
        ]
        .map(str::parse::<Version>)
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
        assert!(ordered.is_sorted_by(|a, b| a < b));
        Ok(())
    }

    #[test]
    fn preserves_exact_spelling_and_rejects_invalid_selectors() -> Result<()> {
        let legacy = "1.2.3-0".parse::<Version>()?;
        let imported = "0:01.2.3".parse::<Version>()?;
        assert_ne!(legacy.cmp(&imported), Ordering::Equal);
        for version in [legacy, imported] {
            let encoded = serde_json::to_string(&version)?;
            assert_eq!(encoded, format!("\"{version}\""));
            assert_eq!(serde_json::from_str::<Version>(&encoded)?, version);
        }
        for invalid in [
            "", "1:", ":1", "1:2:3", "1..2", "1-", "1/2", "1@2", "1,2", "1\n",
        ] {
            assert!(invalid.parse::<Version>().is_err(), "{invalid:?}");
        }
        assert!("1".repeat(257).parse::<Version>().is_err());
        Ok(())
    }
}
