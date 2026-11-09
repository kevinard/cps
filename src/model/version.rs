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

    /// Compare by release semantics, ignoring spelling.
    ///
    /// Use this view for every newer, older, or same-release decision: version
    /// constraints, upgrades, obsoletion cutoffs, and snapshot allocation. It
    /// treats `2.0.0`, `2.0.0-0`, and `0:02.0.0-00` as the same release. The
    /// `Ord` impl on [`Version`] adds a spelling tiebreak for stable ordering of
    /// distinct package IDs only.
    pub fn semantic(&self) -> Semantic<'_> {
        Semantic(self)
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

/// A [`Version`] compared by release semantics; see [`Version::semantic`].
#[derive(Clone, Copy, Debug)]
pub struct Semantic<'a>(&'a Version);

impl Ord for Semantic<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        let (epoch, upstream, revision) = self.0.parts();
        let (other_epoch, other_upstream, other_revision) = other.0.parts();
        cmp_numeric(epoch, other_epoch)
            .then_with(|| tokens(upstream).cmp(tokens(other_upstream)))
            .then_with(|| tokens(revision).cmp(tokens(other_revision)))
    }
}

impl PartialOrd for Semantic<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Semantic<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Semantic<'_> {}

/// Total order for distinct package IDs: semantic order, then the spelling.
impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.semantic()
            .cmp(&other.semantic())
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

/// One comparison unit of a version part, in ascending order of the variants.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Token<'a> {
    Tilde,
    /// A `-` followed by a letter, as in `1.0.0-rc1`.
    PreRelease,
    End,
    /// A digit run without leading zeros. Ordering by length and then bytes is
    /// numeric ordering without overflow.
    Number(usize, &'a [u8]),
    Letter(u8),
    /// `+`, `-` before a digit, `.`, or `_`, in ASCII order.
    Separator(u8),
}

/// Split a version part into tokens and terminate it with [`Token::End`].
fn tokens(part: &str) -> impl Iterator<Item = Token<'_>> {
    let bytes = part.as_bytes();
    let mut index = 0;
    std::iter::from_fn(move || {
        let byte = *bytes.get(index)?;
        let start = index;
        index += 1;
        Some(match byte {
            b'0'..=b'9' => {
                while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                    index += 1;
                }
                let digits = part[start..index].trim_start_matches('0').as_bytes();
                Token::Number(digits.len(), digits)
            }
            b'~' => Token::Tilde,
            b'-' if bytes.get(index).is_some_and(u8::is_ascii_alphabetic) => Token::PreRelease,
            byte if byte.is_ascii_alphabetic() => Token::Letter(byte),
            byte => Token::Separator(byte),
        })
    })
    .chain(std::iter::once(Token::End))
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

    fn versions<const N: usize>(values: [&str; N]) -> Result<[Version; N]> {
        let parsed = values
            .map(str::parse::<Version>)
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        Ok(parsed.try_into().expect("same length"))
    }

    #[test]
    fn orders_legacy_versions_and_imported_releases() -> Result<()> {
        let ordered = versions([
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
        ])?;
        assert!(ordered.is_sorted_by(|a, b| a.semantic() < b.semantic()));
        assert!(ordered.is_sorted_by(|a, b| a < b));
        Ok(())
    }

    #[test]
    fn orders_each_part_like_dpkg_with_semver_prereleases() -> Result<()> {
        let ordered = versions([
            "1.0~a1~1",
            "1.0~a1",
            "1.0~beta",
            "1.0-alpha",
            "1.0-alpha.1",
            "1.0-beta",
            "1.0-rc1",
            "1.0",
            "1.0a",
            "1.0b3",
            "1.0+b1",
            "1.0-1-1",
            "1.0-15-1",
            "1.0.0",
            "1.0.1",
            "1.0_1",
            "1.1",
            "1.1.0-rc1",
            "1.1.0",
        ])?;
        for pair in ordered.windows(2) {
            assert_eq!(
                pair[0].semantic().cmp(&pair[1].semantic()),
                Ordering::Less,
                "{} < {}",
                pair[0],
                pair[1]
            );
        }
        let [beta, patch, image, image_release, prerelease, release] = versions([
            "2.0b3",
            "2.0.1",
            "7.1.1-15-1",
            "7.1.1-1",
            "1.0.0-rc1",
            "1.0.0",
        ])?;
        assert!(beta.semantic() < patch.semantic());
        assert!(image.semantic() > image_release.semantic());
        assert!(prerelease.semantic() < release.semantic());
        Ok(())
    }

    #[test]
    fn semantic_equality_ignores_spelling() -> Result<()> {
        for group in [
            &["2.0.0", "2.0.0-0", "0:2.0.0", "00:02.00.000-00"][..],
            &["1.1", "1.01", "0:1.001-0"],
            &["1.0-1.0", "1.0-01.00"],
            &["3:1.0", "03:1.0-0"],
        ] {
            let group = group
                .iter()
                .map(|value| value.parse::<Version>())
                .collect::<Result<Vec<_>>>()?;
            for a in &group {
                for b in &group {
                    assert_eq!(a.semantic(), b.semantic(), "{a} = {b}");
                    assert_eq!(a.cmp(b) == Ordering::Equal, a == b, "{a} ~ {b}");
                }
            }
        }
        for (a, b) in [
            ("1.0", "1.0.0"),
            ("1.0-1", "1.0-1.0"),
            ("1a", "1a0"),
            ("1.0", "1:1.0"),
        ] {
            let [a, b] = versions([a, b])?;
            assert!(a.semantic() < b.semantic(), "{a} < {b}");
        }
        Ok(())
    }

    #[test]
    fn constraints_match_semantically() -> Result<()> {
        use crate::model::VersionConstraint;
        let matches = |constraint: &str, version: &str| -> Result<bool> {
            Ok(VersionConstraint::parse(Some(constraint))?.matches(&version.parse()?))
        };
        for (constraint, version) in [
            ("=2.0.0-0", "2.0.0"),
            (">=2.0.0-0", "2.0.0"),
            ("<=2.0.0", "0:2.0.0-0"),
            (">= 1.1", "1.01"),
            ("<  2.0.0-0, >=1.0", "1.9"),
            (">1.0.0-rc1", "1.0.0"),
            ("<7.1.1-15-0", "7.1.1-14-3"),
            ("> 7.1.1", "7.1.1-15-0"),
        ] {
            assert!(matches(constraint, version)?, "{version} {constraint}");
        }
        for (constraint, version) in [
            ("<1.0", "0:1.0"),
            (">2.0.0-0", "2.0.0"),
            ("<2.0.0", "2.0.0-0"),
            ("<=2.0.1", "2.0.1b"),
            ("=1.0", "1.0.0"),
        ] {
            assert!(!matches(constraint, version)?, "{version} {constraint}");
        }
        for invalid in ["", "1.0", ">=", "> =1.0", ">=1.0,", "=>1.0"] {
            assert!(
                VersionConstraint::parse(Some(invalid)).is_err(),
                "{invalid:?}"
            );
        }
        let exact = VersionConstraint::exact("2.0.0-0".parse()?);
        assert!(exact.matches(&"2.0.0-0".parse()?));
        assert!(!exact.matches(&"2.0.0".parse()?));
        Ok(())
    }

    #[test]
    fn source_revisions_advance_semantically() -> Result<()> {
        for (current, next) in [
            ("1.0.0", "1.0.0-1"),
            ("1.0.0-0", "1.0.0-1"),
            ("1.0.0-00", "1.0.0-1"),
            ("2:1.0-1.9", "2:1.0-1.10"),
            ("7.1.1-15-0", "7.1.1-15-1"),
            ("1.0.0-rc1", "1.0.0-rc1-1"),
        ] {
            let [current, expected] = versions([current, next])?;
            let next = current.next_source_revision()?;
            assert_eq!(next, expected);
            assert!(next.semantic() > current.semantic(), "{next} > {current}");
        }
        Ok(())
    }

    /// Deterministic generator for property checks, without extra dependencies.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % bound as u64) as usize
        }

        fn digits(&mut self) -> String {
            let zeros = "0".repeat(self.next(3) / 2);
            format!("{zeros}{}", ["0", "1", "2", "9", "10", "99"][self.next(6)])
        }

        fn version(&mut self) -> Version {
            let mut text = String::new();
            if self.next(4) == 0 {
                text += &format!("{}:", self.digits());
            }
            text += &self.digits();
            for _ in 0..self.next(4) {
                text.push(['.', '+', '_', '~', '-'][self.next(5)]);
                if self.next(3) == 0 {
                    text.push(['a', 'b', 'Z', 'r'][self.next(4)]);
                }
                text += &self.digits();
            }
            if self.next(3) == 0 {
                text += &format!("-{}", self.digits());
            }
            text.parse().expect("generated versions are valid")
        }
    }

    #[test]
    fn orders_are_total_transitive_and_consistent() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let sample: Vec<_> = (0..120).map(|_| rng.version()).collect();
        for a in &sample {
            assert_eq!(a.semantic().cmp(&a.semantic()), Ordering::Equal);
            for b in &sample {
                let semantic = a.semantic().cmp(&b.semantic());
                assert_eq!(semantic, b.semantic().cmp(&a.semantic()).reverse());
                assert_eq!(a.cmp(b), b.cmp(a).reverse());
                assert_eq!(a.cmp(b) == Ordering::Equal, a == b, "{a} {b}");
                // The total order refines the semantic order.
                assert!(semantic == Ordering::Equal || a.cmp(b) == semantic);
                for c in &sample {
                    let bc = b.semantic().cmp(&c.semantic());
                    if semantic != Ordering::Greater && bc != Ordering::Greater {
                        assert_ne!(
                            a.semantic().cmp(&c.semantic()),
                            Ordering::Greater,
                            "{a} <= {b} <= {c}"
                        );
                    }
                    if a < b && b < c {
                        assert!(a < c, "{a} < {b} < {c}");
                    }
                }
            }
        }
        let mut sorted = sample.clone();
        sorted.sort();
        assert!(sorted.is_sorted_by(|a, b| a.semantic() <= b.semantic()));
    }

    #[test]
    fn spelling_variants_compare_like_the_original() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..400 {
            let (a, b) = (rng.version(), rng.version());
            let text = a.to_string();
            let mut variants = vec![format!("0:{text}")];
            let has_revision = text
                .rsplit_once('-')
                .is_some_and(|(_, revision)| revision.split('.').all(is_numeric));
            if !has_revision {
                variants.push(format!("{text}-0"));
            }
            if let Some(release) = text.strip_prefix("0:") {
                variants.push(release.to_owned());
            }
            // Pad every digit run that starts a number with a leading zero.
            let mut padded = String::new();
            let mut previous = None;
            for ch in text.chars() {
                if ch.is_ascii_digit() && !previous.is_some_and(|p: char| p.is_ascii_digit()) {
                    padded.push('0');
                }
                padded.push(ch);
                previous = Some(ch);
            }
            variants.push(padded);
            for variant in variants {
                let Ok(variant) = variant.parse::<Version>() else {
                    continue;
                };
                assert_eq!(variant.semantic(), a.semantic(), "{variant} = {a}");
                assert_eq!(
                    variant.semantic().cmp(&b.semantic()),
                    a.semantic().cmp(&b.semantic()),
                    "{variant} vs {b}"
                );
            }
        }
    }

    #[test]
    fn preserves_exact_spelling_and_rejects_invalid_selectors() -> Result<()> {
        let legacy = "1.2.3-0".parse::<Version>()?;
        let imported = "0:01.2.3".parse::<Version>()?;
        assert_eq!(legacy.semantic(), imported.semantic());
        assert_ne!(legacy.cmp(&imported), Ordering::Equal);
        assert_ne!(legacy, imported);
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
