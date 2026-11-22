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
use reqwest::{Client, StatusCode, header::ACCEPT_ENCODING, redirect::Policy};
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

pub(crate) const OBJECTS: &str = "sha256";

/// Fetch missing pinned inputs over HTTPS and verify every object in the recipe.
pub(crate) fn fetch_sources(recipe: &Path, cache: &Path) -> Result<usize> {
    cache_sources(recipe, cache, None, true)
}

/// Prefer local recipe inputs and fetch missing ones over HTTPS.
pub(crate) fn fetch_sources_with_local(recipe: &Path, cache: &Path, from: &Path) -> Result<usize> {
    cache_sources(recipe, cache, Some(Root::open(from)?), true)
}

/// Verify and cache pinned inputs from a local directory, offline.
pub(crate) fn import_sources(recipe: &Path, cache: &Path, from: &Path) -> Result<usize> {
    cache_sources(recipe, cache, Some(Root::open(from)?), false)
}

fn cache_sources(recipe: &Path, cache: &Path, local: Option<Root>, fetch: bool) -> Result<usize> {
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
        let local_file = match &local {
            Some(root) => find_local(root, &aliases)?,
            None => None,
        };
        if let Some((name, file)) = local_file {
            copy_verified(file, temp.as_file_mut(), pin, name)?;
        } else {
            ensure!(fetch, "no local source file for {name}");
            let urls: Vec<_> = aliases
                .iter()
                .flat_map(|(_, source)| &source.urls)
                .collect();
            ensure!(!urls.is_empty(), "source {name} is uncached and has no URL");
            let (runtime, client) = match &https {
                Some(https) => https,
                None => https.insert(http()?),
            };
            // Each URL must deliver the pinned bytes; the next one is tried
            // only after a failure.
            let mut failures = Vec::new();
            for url in urls {
                let file = temp.as_file_mut();
                file.set_len(0)?;
                file.rewind()?;
                match runtime.block_on(download(client, url, file, pin)) {
                    Ok(()) => {
                        failures.clear();
                        break;
                    }
                    Err(error) => failures.push(format!("{url}: {error:#}")),
                }
            }
            ensure!(
                failures.is_empty(),
                "fetching source {name} failed from every URL:\n  {}",
                failures.join("\n  ")
            );
        }
        publish_verified(&cache, temp, pin)?;
    }
    Ok(recipe.sources.len())
}

fn find_local<'a>(root: &Root, aliases: &[(&'a str, &Source)]) -> Result<Option<(&'a str, File)>> {
    for &(name, _) in aliases {
        match root.file(name) {
            Ok(file) => return Ok(Some((name, file))),
            Err(err) if err.downcast_ref::<Errno>() == Some(&Errno::NOENT) => {}
            Err(err) => return Err(err.context(format!("opening source {name}"))),
        }
    }
    Ok(None)
}

pub(crate) fn copy_verified(
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

pub(crate) fn verify_cached(file: File, pin: &FileDigest) -> Result<()> {
    ensure!(
        file.metadata()?.len() == pin.size
            && digest_reader(file.take(pin.size.saturating_add(1)))? == pin.sha256,
        "cached source {} differs from its pin",
        pin.sha256
    );
    Ok(())
}

pub(crate) fn publish_verified(cache: &Root, temp: NamedTempFile, pin: &FileDigest) -> Result<()> {
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o444))?;
    temp.as_file().sync_all()?;
    match temp.persist_noclobber(Root::fd_path(cache.as_file()).join(&pin.sha256)) {
        Ok(_) => cache.as_file().sync_all()?,
        Err(err) if err.error.kind() == io::ErrorKind::AlreadyExists => {
            verify_cached(cache.file(&pin.sha256)?, pin)?;
        }
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

/// The most redirects a source download follows.
const MAX_REDIRECTS: usize = 10;

fn http() -> Result<(Runtime, Client)> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    // Release assets and mirrors usually redirect. The content is pinned by
    // digest, so following HTTPS redirects cannot change what is accepted.
    let redirects = Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error(format!("more than {MAX_REDIRECTS} redirects"))
        } else if attempt.url().scheme() != "https" {
            attempt.error("redirect leaves HTTPS")
        } else {
            attempt.follow()
        }
    });
    let client = https_client()
        .redirect(redirects)
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
