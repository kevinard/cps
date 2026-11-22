//! Mirror lists: `schema-version 1 kind="mirror-list"`.
//!
//! A publisher signs a list of public mirrors as a TUF target. An
//! administrator may also keep a local list per publisher, which can add
//! preferred mirrors, exclude mirrors, prefer regions, or ignore the
//! publisher's list.
use anyhow::{Context, Result, ensure};

use super::{Fields, document, schema};

pub(crate) const MIRROR_LIST_LIMIT: u64 = 64 * 1024;
const MAX_MIRRORS: usize = 64;
const MAX_REGIONS: usize = 16;

/// One mirror base URL, which holds `metadata/` and `targets/`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ListedMirror {
    pub url: String,
    pub region: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MirrorList {
    /// Mirrors in the order they appear. A local list's mirrors are tried
    /// before every other source, in this order.
    pub mirrors: Vec<ListedMirror>,
    /// Base URLs never to use. Local lists only.
    pub exclude: Vec<String>,
    /// Whether to use the publisher's signed list. Local lists only.
    pub signed_mirrors: bool,
    /// Preferred regions, most preferred first. Local lists only.
    pub regions: Vec<String>,
}

/// Which mirror list a document is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MirrorListKind {
    /// Signed by the publisher: only `mirror` entries.
    Signed,
    /// Kept by an administrator for one publisher.
    Local,
}

pub(crate) fn parse_mirror_list(text: &str, kind: MirrorListKind) -> Result<MirrorList> {
    let doc = document(text)?;
    schema(&doc, super::Kind::MirrorList)?;
    let mut list = MirrorList {
        mirrors: Vec::new(),
        exclude: Vec::new(),
        signed_mirrors: true,
        regions: Vec::new(),
    };
    let mut signed_setting = false;
    for node in doc.nodes() {
        let name = node.name().value();
        let local = kind == MirrorListKind::Local;
        match name {
            "schema-version" => {}
            "mirror" => {
                let fields = Fields::read(node, 1, &["region"], false)?;
                let region = fields.string("region")?;
                if let Some(region) = &region {
                    check_region(region)?;
                }
                list.mirrors.push(ListedMirror {
                    url: fields.string_arg(0)?,
                    region,
                });
            }
            "exclude" if local => list
                .exclude
                .push(Fields::read(node, 1, &[], false)?.string_arg(0)?),
            "region" if local => {
                let region = Fields::read(node, 1, &[], false)?.string_arg(0)?;
                check_region(&region)?;
                ensure!(!list.regions.contains(&region), "duplicate region {region}");
                list.regions.push(region);
            }
            "signed-mirrors" if local => {
                ensure!(!signed_setting, "duplicate signed-mirrors node");
                signed_setting = true;
                let fields = Fields::read(node, 1, &[], false)?;
                list.signed_mirrors = fields.args[0]
                    .as_bool()
                    .context("signed-mirrors must be #true or #false")?;
            }
            _ => anyhow::bail!("unknown mirror-list node {name}"),
        }
    }
    ensure!(
        list.mirrors.len() <= MAX_MIRRORS && list.exclude.len() <= MAX_MIRRORS,
        "a mirror list holds at most {MAX_MIRRORS} mirrors and {MAX_MIRRORS} exclusions"
    );
    ensure!(
        list.regions.len() <= MAX_REGIONS,
        "a mirror list prefers at most {MAX_REGIONS} regions"
    );
    Ok(list)
}

fn check_region(region: &str) -> Result<()> {
    ensure!(
        !region.is_empty()
            && region.len() <= 32
            && region
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "a region is 1 to 32 ASCII letters, digits, or hyphens"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_lists_accept_policy_and_signed_lists_only_mirrors() -> Result<()> {
        let local = parse_mirror_list(
            r#"schema-version 1 kind="mirror-list"
mirror "https://lan.example/canopa/"
exclude "https://slow.example/canopa/"
region "eu"
signed-mirrors #false
"#,
            MirrorListKind::Local,
        )?;
        assert_eq!(local.mirrors[0].url, "https://lan.example/canopa/");
        assert_eq!(local.exclude, ["https://slow.example/canopa/"]);
        assert_eq!(local.regions, ["eu"]);
        assert!(!local.signed_mirrors);

        let signed = parse_mirror_list(
            r#"schema-version 1 kind="mirror-list"
mirror "https://a.example/" region="eu"
mirror "https://b.example/"
"#,
            MirrorListKind::Signed,
        )?;
        assert_eq!(signed.mirrors[0].region.as_deref(), Some("eu"));
        assert!(signed.signed_mirrors);
        for node in [
            "exclude \"https://a.example/\"",
            "region \"eu\"",
            "signed-mirrors #false",
        ] {
            let text = format!("schema-version 1 kind=\"mirror-list\"\n{node}\n");
            assert!(parse_mirror_list(&text, MirrorListKind::Signed).is_err());
            assert!(parse_mirror_list(&text, MirrorListKind::Local).is_ok());
        }
        assert!(
            parse_mirror_list(
                "schema-version 1 kind=\"mirror-list\"\nmirror \"https://a/\" region=\"a b\"\n",
                MirrorListKind::Signed
            )
            .is_err()
        );
        Ok(())
    }
}
