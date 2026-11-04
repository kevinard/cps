use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read, Seek, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use futures_util::TryStreamExt;
use reqwest::{Client, StatusCode, header::ACCEPT_ENCODING};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::runtime::{Builder, Runtime};
use url::Url;

use crate::{
    image::root::Root,
    kdl_format::build::{self, Source},
    model::{FileDigest, digest_reader},
    transport::https_client,
};

pub(super) const OBJECTS: &str = "sha256";

/// Fetch missing pinned inputs over HTTPS and verify every object in the recipe.
pub fn fetch_sources(recipe: &Path, cache: &Path) -> Result<usize> {
    cache_sources(recipe, cache, None)
}

/// Verify and cache pinned inputs from a local directory, offline.
pub fn import_sources(recipe: &Path, cache: &Path, from: &Path) -> Result<usize> {
    cache_sources(recipe, cache, Some(Root::open(from)?))
}

fn cache_sources(recipe: &Path, cache: &Path, local: Option<Root>) -> Result<usize> {
    let (_, recipe) = build::read(recipe)?;
    fs::create_dir_all(cache)?;
    let cache = Root::open(cache)?.ensure_dir(OBJECTS)?;
    let directory = Root::fd_path(cache.as_file());
    let mut objects = BTreeMap::<&str, Vec<(&str, &Source)>>::new();
    for (name, source) in &recipe.sources {
        objects
            .entry(&source.pin.sha256)
            .or_default()
            .push((name, source));
    }
    let mut https = None;
    for (hash, aliases) in objects {
        let (name, source) = aliases[0];
        let pin = &source.pin;
        match cache.file(hash) {
            Ok(file) => {
                verify_cached(file, pin)?;
                continue;
            }
            Err(err) if err.downcast_ref::<Errno>() == Some(&Errno::NOENT) => {}
            Err(err) => return Err(err.context(format!("opening cached source {}", pin.sha256))),
        }
        let mut temp = NamedTempFile::new_in(&directory)?;
        if let Some(root) = &local {
            let (name, file) = find_local(root, &aliases)?;
            copy_verified(file, temp.as_file_mut(), pin, name)?;
        } else {
            let (name, url) = aliases
                .iter()
                .find_map(|(name, source)| source.url.as_ref().map(|url| (*name, url)))
                .with_context(|| format!("source {name} is uncached and has no URL"))?;
            let (runtime, client) = match &https {
                Some(https) => https,
                None => https.insert(http()?),
            };
            runtime
                .block_on(download(client, url, temp.as_file_mut(), pin))
                .with_context(|| format!("fetching source {name}"))?;
        }
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o444))?;
        temp.as_file().sync_all()?;
        match temp.persist_noclobber(directory.join(hash)) {
            Ok(_) => cache.as_file().sync_all()?,
            Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => {
                // Another fetch may have published this digest while we were streaming it.
                verify_cached(cache.file(hash)?, pin)?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(recipe.sources.len())
}

fn find_local<'a>(root: &Root, aliases: &[(&'a str, &Source)]) -> Result<(&'a str, File)> {
    for &(name, _) in aliases {
        match root.file(name) {
            Ok(file) => return Ok((name, file)),
            Err(err) if err.downcast_ref::<Errno>() == Some(&Errno::NOENT) => {}
            Err(err) => return Err(err.context(format!("opening source {name}"))),
        }
    }
    anyhow::bail!("no local source file for {}", aliases[0].0)
}

pub(super) fn copy_verified(
    source: File,
    destination: &mut File,
    pin: &FileDigest,
    name: &str,
) -> Result<()> {
    ensure!(
        source.metadata()?.len() == pin.size,
        "source length mismatch for {name}"
    );
    let copied = io::copy(&mut source.take(pin.size.saturating_add(1)), destination)?;
    destination.rewind()?;
    ensure!(
        copied == pin.size && digest_reader(destination)? == pin.sha256,
        "source digest or length mismatch for {name}"
    );
    Ok(())
}

fn verify_cached(file: File, pin: &FileDigest) -> Result<()> {
    ensure!(
        file.metadata()?.len() == pin.size
            && digest_reader(file.take(pin.size.saturating_add(1)))? == pin.sha256,
        "cached source {} differs from its pin",
        pin.sha256
    );
    Ok(())
}

fn http() -> Result<(Runtime, Client)> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let client = https_client()
        .timeout(Duration::from_secs(30 * 60))
        .build()?;
    Ok((runtime, client))
}

async fn download(client: &Client, url: &Url, file: &mut File, pin: &FileDigest) -> Result<()> {
    let response = client
        .get(url.clone())
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let status = response.status();
    ensure!(
        status == StatusCode::OK,
        "expected HTTP 200, received {status}"
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length == pin.size),
        "source Content-Length differs from its pin"
    );
    let mut body = response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut remaining = pin.size;
    while let Some(chunk) = body.try_next().await.map_err(reqwest::Error::without_url)? {
        remaining = remaining
            .checked_sub(chunk.len() as u64)
            .context("source exceeded its pinned size")?;
        file.write_all(&chunk)?;
        hasher.update(&chunk);
    }
    ensure!(remaining == 0, "source is shorter than its pinned size");
    ensure!(
        hex::encode(hasher.finalize()) == pin.sha256,
        "source digest differs from its pin"
    );
    Ok(())
}
