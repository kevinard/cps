use anyhow::{Context, Result, ensure};

use super::{Fields, document, one, schema_version};
use crate::model::validate_digest;

const RECEIPT_FORMAT: u32 = 1;
const RECEIPT_KIND: &str = "boot-export-receipt";

pub(crate) struct ExportReceipt {
    pub image: String,
    pub revision: u64,
    pub state_sha256: String,
    pub tree_sha256: String,
    pub bytes: u64,
    pub sha256: String,
}

impl ExportReceipt {
    pub fn parse(text: &str) -> Result<Self> {
        let doc = document(text)?;
        schema_version(&doc, RECEIPT_FORMAT, RECEIPT_KIND)?;
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
            tree_sha256: fields.required_string("tree-sha256")?,
            bytes: fields.integer("bytes")?.context("missing bytes property")?,
            sha256: fields.required_string("sha256")?,
        };
        validate_digest(&receipt.state_sha256)?;
        validate_digest(&receipt.tree_sha256)?;
        validate_digest(&receipt.sha256)?;
        Ok(receipt)
    }

    pub fn render(&self) -> String {
        // Export constructs these strings from a canonical UUID and hex digests.
        format!(
            "schema-version {RECEIPT_FORMAT} kind=\"{RECEIPT_KIND}\"\nroot-export \"{}\" revision={} state-sha256=\"{}\" bytes={} sha256=\"{}\" tree-sha256=\"{}\"\n",
            self.image, self.revision, self.state_sha256, self.bytes, self.sha256, self.tree_sha256
        )
    }
}
