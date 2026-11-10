//! Build-time path patterns for `collect` and `discard` rules.
//!
//! A pattern is a root-relative path. A `**` component matches zero or more
//! whole components, or one or more at the end of the pattern. Inside a component, `*` matches any run of characters and
//! `?` matches one character. Every other character is literal.

use anyhow::{Result, ensure};

/// Check that a pattern is a well-formed root-relative path pattern.
pub(crate) fn validate(pattern: &str) -> Result<()> {
    ensure!(
        !pattern.is_empty()
            && !pattern.contains('\0')
            && pattern.split('/').all(|part| {
                !matches!(part, "" | "." | "..") && (part == "**" || !part.contains("**"))
            }),
        "invalid path pattern {pattern:?}; use root-relative components, with ** only as a whole component"
    );
    Ok(())
}

/// Match a validated pattern against a root-relative path.
pub(crate) fn matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<_> = pattern.split('/').collect();
    let path: Vec<_> = path.split('/').collect();
    components(&pattern, &path)
}

fn components(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        // A trailing `**` names what is inside a directory, not the directory itself.
        Some((&"**", [])) => !path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| components(rest, &path[skip..])),
        Some((first, rest)) => path.split_first().is_some_and(|(name, tail)| {
            component(first.as_bytes(), name.as_bytes()) && components(rest, tail)
        }),
    }
}

fn component(pattern: &[u8], name: &[u8]) -> bool {
    // Iterative wildcard matching with single-star backtracking.
    let (mut p, mut n) = (0, 0);
    let mut star = None;
    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&byte) if byte == b'?' || byte == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((star_p, star_n)) => {
                    p = star_p + 1;
                    n = star_n + 1;
                    star = Some((star_p, star_n + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&byte| byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_match_components_and_recursive_trees() {
        for (pattern, path) in [
            ("usr/bin/*", "usr/bin/ls"),
            ("usr/bin/*", "usr/bin/["),
            ("usr/lib/lib*.so.*", "usr/lib/libz.so.1.3"),
            ("usr/include/**", "usr/include/zlib.h"),
            ("usr/include/**", "usr/include/sys/a/b.h"),
            (
                "usr/share/locale/*/**",
                "usr/share/locale/fr/LC_MESSAGES/x.mo",
            ),
            ("**/*.la", "usr/lib/libz.la"),
            ("**", "a"),
            ("usr/**/man?", "usr/share/man1"),
            ("usr/bin/a*b*c", "usr/bin/axxbyyc"),
        ] {
            validate(pattern).unwrap();
            assert!(matches(pattern, path), "{pattern} should match {path}");
        }
        for (pattern, path) in [
            ("usr/bin/*", "usr/bin/sub/ls"),
            ("usr/bin/*", "usr/bin"),
            ("usr/include/**", "usr/include"),
            ("usr/lib/lib*.so", "usr/lib/libz.so.1"),
            ("usr/share/locale/*/**", "usr/share/locale/locale.alias"),
            ("usr/bin/a*b*c", "usr/bin/axxbyyca"),
            ("?", "ab"),
        ] {
            assert!(!matches(pattern, path), "{pattern} should not match {path}");
        }
        for invalid in [
            "",
            "/usr",
            "usr//bin",
            "usr/../etc",
            "usr/**a",
            "usr/a**",
            "./x",
        ] {
            assert!(validate(invalid).is_err(), "accepted {invalid:?}");
        }
    }
}
