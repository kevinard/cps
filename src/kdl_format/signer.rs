//! External signer descriptors: `schema-version 1 kind="signer"`.
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};

use super::{Fields, document, one, schema};
use crate::repo::read_limited;

const DESCRIPTOR_LIMIT: u64 = 64 * 1024;
const DEFAULT_TIMEOUT: u64 = 30;
const MAX_TIMEOUT: u64 = 600;
const MAX_ARGUMENTS: usize = 64;

/// How to reach a signing key that CPS cannot read directly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SignerDescriptor {
    /// Absolute path of the helper. CPS executes it directly, without a shell.
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Declared public key. Every returned signature is verified against it.
    pub public_key: PathBuf,
    pub timeout: Duration,
}

pub(crate) fn read_signer(path: &Path) -> Result<SignerDescriptor> {
    (|| {
        let bytes = read_limited(File::open(path)?, DESCRIPTOR_LIMIT)?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        parse_signer(std::str::from_utf8(&bytes)?, parent)
    })()
    .with_context(|| format!("reading signer descriptor {}", path.display()))
}

fn parse_signer(text: &str, parent: &Path) -> Result<SignerDescriptor> {
    let doc = document(text)?;
    schema(&doc, "signer")?;
    for node in doc.nodes() {
        let name = node.name().value();
        ensure!(
            matches!(
                name,
                "schema-version" | "command" | "public-key" | "timeout-seconds"
            ),
            "unknown signer node {name}"
        );
    }
    let command = one(&doc, "command")?;
    ensure!(
        command.ty().is_none() && command.children().is_none(),
        "command takes only string arguments"
    );
    let mut words = Vec::new();
    for entry in command.entries() {
        ensure!(
            entry.name().is_none() && entry.ty().is_none(),
            "command takes only positional string arguments"
        );
        let word = entry
            .value()
            .as_string()
            .context("command arguments must be strings")?;
        ensure!(!word.contains('\0'), "command arguments cannot contain NUL");
        words.push(word.to_owned());
    }
    ensure!(
        !words.is_empty() && words.len() <= MAX_ARGUMENTS + 1,
        "command needs a program and at most {MAX_ARGUMENTS} arguments"
    );
    let program = PathBuf::from(words.remove(0));
    ensure!(
        program.is_absolute(),
        "signer program must be an absolute path"
    );
    let public_key = Fields::read(one(&doc, "public-key")?, 1, &[], false)?.string_arg(0)?;
    let timeout = match doc
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "timeout-seconds")
        .count()
    {
        0 => DEFAULT_TIMEOUT,
        _ => {
            let fields = Fields::read(one(&doc, "timeout-seconds")?, 1, &[], false)?;
            fields.args[0]
                .as_integer()
                .and_then(|value| u64::try_from(value).ok())
                .filter(|value| (1..=MAX_TIMEOUT).contains(value))
                .with_context(|| format!("timeout-seconds must be between 1 and {MAX_TIMEOUT}"))?
        }
    };
    Ok(SignerDescriptor {
        program,
        args: words,
        public_key: parent.join(public_key),
        timeout: Duration::from_secs(timeout),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_require_an_absolute_program_and_a_public_key() -> Result<()> {
        let descriptor = parse_signer(
            r#"schema-version 1 kind="signer"
command "/usr/libexec/cps-kms-sign" "--key" "release/targets" "$HOME"
public-key "targets.pub"
timeout-seconds 5
"#,
            Path::new("/etc/cps/signers"),
        )?;
        assert_eq!(
            descriptor,
            SignerDescriptor {
                program: "/usr/libexec/cps-kms-sign".into(),
                args: vec!["--key".into(), "release/targets".into(), "$HOME".into()],
                public_key: "/etc/cps/signers/targets.pub".into(),
                timeout: Duration::from_secs(5),
            }
        );
        let default = parse_signer(
            "schema-version 1 kind=\"signer\"\ncommand \"/bin/sign\"\npublic-key \"/k.pub\"\n",
            Path::new("."),
        )?;
        assert_eq!(default.timeout, Duration::from_secs(DEFAULT_TIMEOUT));
        assert_eq!(default.public_key, Path::new("/k.pub"));

        for invalid in [
            "command \"sign\"\npublic-key \"k\"",
            "command\npublic-key \"k\"",
            "command \"/bin/sign\"",
            "command \"/bin/sign\" key=\"x\"\npublic-key \"k\"",
            "command \"/bin/sign\"\npublic-key \"k\"\ntimeout-seconds 0",
            "command \"/bin/sign\"\npublic-key \"k\"\ntimeout-seconds 601",
            "command \"/bin/sign\"\npublic-key \"k\"\nshell \"sh -c\"",
        ] {
            let text = format!("schema-version 1 kind=\"signer\"\n{invalid}\n");
            assert!(
                parse_signer(&text, Path::new(".")).is_err(),
                "accepted {invalid:?}"
            );
        }
        Ok(())
    }
}
