//! Signs disposable local mirror fixtures using a throwaway key for all roles.
use std::{fs, path::PathBuf};

use anyhow::{Result, anyhow, ensure};
use clap::Parser;
use jiff::{Span, Timestamp};
use tough::editor::signed::PathExists;
use url::Url;

mod tuf;

#[derive(Parser)]
struct Arguments {
    export: PathBuf,
    mirror: PathBuf,
    version: u64,
}

fn main() -> Result<()> {
    let Arguments {
        export,
        mirror,
        version,
    } = Arguments::parse();
    ensure!(version > 0, "release version must be nonzero");
    fs::create_dir_all(&mirror)?;
    let anchor = mirror.join("root.json");
    let key = mirror.join("fixture-key.der");
    if version == 1 {
        ensure!(
            !anchor.exists() && !key.exists(),
            "mirror fixture already exists"
        );
        tuf::sign_root(&anchor, &key, None)?;
    }
    let expires = Timestamp::now().checked_add(Span::new().hours(24))?;
    tuf::sign_export(
        &export,
        &mirror,
        version,
        expires,
        &anchor,
        &key,
        PathExists::Skip,
    )?;
    let url = Url::from_directory_path(mirror.canonicalize()?)
        .map_err(|()| anyhow!("cannot form a file URL for the mirror fixture"))?;
    println!("{url}");
    Ok(())
}
