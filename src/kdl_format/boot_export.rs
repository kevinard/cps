use anyhow::{Context, Result, ensure};

use super::{Fields, document, one, schema};
use crate::model::{FORMAT, validate_digest};

pub(crate) struct ExportReceipt {
    pub image: String,
    pub revision: u64,
    pub state_sha256: String,
    pub tree_sha256: Option<String>,
    pub bytes: u64,
    pub sha256: String,
}

impl ExportReceipt {
    pub fn parse(text: &str) -> Result<Self> {
        let doc = document(text)?;
        schema(&doc)?;
        ensure!(
            doc.nodes().len() == 2,
            "expected only schema-version and root-export nodes"
        );
        let fields = Fields::read(
            one(&doc, "root-export")?,
            1,
            &["revision", "state-sha256", "bytes", "sha256", "tree-sha256"],
            false,
        )?;
        let receipt = Self {
            image: fields.string_arg(0)?,
            revision: fields
                .integer("revision")?
                .context("missing revision property")?,
            state_sha256: fields.required_string("state-sha256")?,
            tree_sha256: fields.string("tree-sha256")?,
            bytes: fields.integer("bytes")?.context("missing bytes property")?,
            sha256: fields.required_string("sha256")?,
        };
        validate_digest(&receipt.state_sha256)?;
        if let Some(tree) = &receipt.tree_sha256 {
            validate_digest(tree)?;
        }
        validate_digest(&receipt.sha256)?;
        Ok(receipt)
    }

    pub fn render(&self) -> String {
        // Export constructs these strings from a canonical UUID and hex digests.
        let mut text = format!(
            "schema-version {FORMAT}\nroot-export \"{}\" revision={} state-sha256=\"{}\" bytes={} sha256=\"{}\"",
            self.image, self.revision, self.state_sha256, self.bytes, self.sha256
        );
        if let Some(tree) = &self.tree_sha256 {
            text.push_str(&format!(" tree-sha256=\"{tree}\""));
        }
        text.push('\n');
        text
    }
}
