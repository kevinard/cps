//! Descriptive package metadata: SPDX license expressions and upstream URLs.

use anyhow::{Context, Result, bail, ensure};
use url::Url;

const LICENSE_LIMIT: usize = 1024;
const URL_LIMIT: usize = 2048;

/// Check the syntax of an SPDX license expression, such as
/// `GPL-2.0-or-later WITH Linux-syscall-note OR (MIT AND BSD-3-Clause)`.
///
/// CPS checks the grammar from SPDX specification annex D, not membership in the
/// SPDX license list, so new identifiers and `LicenseRef-` references work.
pub(crate) fn validate_license(expression: &str) -> Result<()> {
    ensure!(
        !expression.trim().is_empty() && expression.len() <= LICENSE_LIMIT,
        "license must be a nonempty SPDX expression of at most {LICENSE_LIMIT} bytes"
    );
    let tokens = tokenize(expression)?;
    let mut parser = Parser {
        tokens: &tokens,
        position: 0,
        depth: 0,
    };
    parser
        .or()
        .and_then(|()| match parser.tokens.get(parser.position) {
            None => Ok(()),
            Some(token) => bail!("unexpected {token:?}"),
        })
        .with_context(|| format!("invalid SPDX license expression {expression:?}"))
}

#[derive(Debug, PartialEq)]
enum Token<'a> {
    Open,
    Close,
    And,
    Or,
    With,
    Id(&'a str),
}

fn tokenize(expression: &str) -> Result<Vec<Token<'_>>> {
    let mut tokens = Vec::new();
    let mut rest = expression;
    loop {
        rest = rest.trim_start_matches([' ', '\t']);
        let Some(first) = rest.chars().next() else {
            return Ok(tokens);
        };
        let length = match first {
            '(' => {
                tokens.push(Token::Open);
                1
            }
            ')' => {
                tokens.push(Token::Close);
                1
            }
            _ => {
                let length = rest.find([' ', '\t', '(', ')']).unwrap_or(rest.len());
                tokens.push(match &rest[..length] {
                    "AND" => Token::And,
                    "OR" => Token::Or,
                    "WITH" => Token::With,
                    word @ ("and" | "or" | "with") => {
                        bail!("write the SPDX operator {word:?} in uppercase")
                    }
                    word => Token::Id(word),
                });
                length
            }
        };
        rest = &rest[length..];
    }
}

struct Parser<'a, 'b> {
    tokens: &'b [Token<'a>],
    position: usize,
    depth: usize,
}

impl Parser<'_, '_> {
    fn next(&mut self) -> Option<&Token<'_>> {
        let token = self.tokens.get(self.position);
        self.position += usize::from(token.is_some());
        token
    }

    fn accept(&mut self, expected: &Token<'_>) -> bool {
        let found = self.tokens.get(self.position) == Some(expected);
        self.position += usize::from(found);
        found
    }

    fn or(&mut self) -> Result<()> {
        self.and()?;
        while self.accept(&Token::Or) {
            self.and()?;
        }
        Ok(())
    }

    fn and(&mut self) -> Result<()> {
        self.term()?;
        while self.accept(&Token::And) {
            self.term()?;
        }
        Ok(())
    }

    fn term(&mut self) -> Result<()> {
        match self.next() {
            Some(Token::Open) => {
                self.depth += 1;
                ensure!(self.depth <= 32, "too deeply nested");
                self.or()?;
                ensure!(self.accept(&Token::Close), "missing closing parenthesis");
                self.depth -= 1;
                Ok(())
            }
            Some(Token::Id(id)) => {
                let id = *id;
                license(id)?;
                if self.accept(&Token::With) {
                    match self.next() {
                        Some(Token::Id(exception)) => exception_id(exception),
                        _ => bail!("WITH requires a license exception"),
                    }
                } else {
                    Ok(())
                }
            }
            Some(token) => bail!("expected a license, found {token:?}"),
            None => bail!("expected a license"),
        }
    }
}

fn idstring(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
}

fn license(id: &str) -> Result<()> {
    let reference = id
        .strip_prefix("DocumentRef-")
        .map(|rest| -> Result<&str> {
            let (document, reference) = rest
                .split_once(':')
                .context("DocumentRef- requires a ':LicenseRef-' suffix")?;
            ensure!(idstring(document), "invalid document reference {id:?}");
            Ok(reference)
        })
        .transpose()?;
    if let Some(reference) = reference.or_else(|| id.strip_prefix("LicenseRef-").map(|_| id)) {
        let name = reference
            .strip_prefix("LicenseRef-")
            .context("expected LicenseRef- after the document reference")?;
        ensure!(idstring(name), "invalid license reference {id:?}");
        return Ok(());
    }
    ensure!(
        idstring(id.strip_suffix('+').unwrap_or(id)),
        "invalid license identifier {id:?}"
    );
    Ok(())
}

fn exception_id(id: &str) -> Result<()> {
    let name = id.strip_prefix("AdditionRef-").unwrap_or(id);
    ensure!(idstring(name), "invalid license exception {id:?}");
    Ok(())
}

/// Check that a metadata URL uses HTTPS without credentials.
pub(crate) fn validate_https_url(value: &str) -> Result<()> {
    ensure!(value.len() <= URL_LIMIT, "URL exceeds {URL_LIMIT} bytes");
    let url = Url::parse(value).context("invalid URL")?;
    ensure!(
        url.scheme() == "https"
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none(),
        "URL must use HTTPS without credentials"
    );
    ensure!(url.as_str() == value, "write the URL in normalized form");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn license_expressions_follow_the_spdx_grammar() {
        for valid in [
            "MIT",
            "GPL-2.0-or-later",
            "GPL-2.0+",
            "Apache-2.0 OR MIT",
            "LGPL-2.1-or-later AND BSD-3-Clause AND GPL-2.0-or-later",
            "GPL-2.0-only WITH Linux-syscall-note",
            "(MIT OR Apache-2.0) AND Unicode-3.0",
            "((MIT))",
            "LicenseRef-blessing",
            "DocumentRef-spdx-tool-1.2:LicenseRef-MIT-Style-2",
            "GPL-3.0-or-later WITH AdditionRef-Autoconf-exception",
            "blessing",
        ] {
            validate_license(valid).unwrap_or_else(|error| panic!("{valid}: {error:#}"));
        }
        for invalid in [
            "",
            " ",
            "MIT and Apache-2.0",
            "MIT OR",
            "AND MIT",
            "(MIT",
            "MIT)",
            "MIT Apache-2.0",
            "GPL-2.0 WITH",
            "MIT/X11",
            "LicenseRef-",
            "LicenseRef-a+",
            "DocumentRef-x:MIT",
            "MIT WITH (Classpath-exception-2.0)",
            "()",
        ] {
            assert!(validate_license(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(validate_license(&format!("{}MIT", "(".repeat(40))).is_err());
    }

    #[test]
    fn metadata_urls_require_normalized_https() {
        validate_https_url("https://www.sqlite.org/").unwrap();
        validate_https_url("https://example.org/releases/a-1.0.tar.gz").unwrap();
        for invalid in [
            "http://example.org/",
            "https://user@example.org/",
            "ftp://example.org/a",
            "https://example.org",
            "example.org",
        ] {
            assert!(validate_https_url(invalid).is_err(), "accepted {invalid}");
        }
    }
}
