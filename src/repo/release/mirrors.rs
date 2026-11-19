//! Where a publisher's TUF repository can be fetched from, and in which order.
//!
//! Every mirror serves the same signed repository, so a mirror is only a
//! transport. The candidates for one publisher come from three places:
//!
//! 1. the administrator's local mirror list, `/etc/cps/mirrors/NAME.kdl`,
//!    whose mirrors are always tried first, in file order;
//! 2. the configured source: the URL the publisher was added from, or the
//!    image configuration's `mirror` node;
//! 3. the publisher's signed mirror list, a TUF target named `mirrors`.
//!
//! Sources 2 and 3 form a pool that `cps publisher mirror rank` orders by
//! freshness and latency. Each operation tries the candidates in order until
//! one completes.

use std::{
    cmp::Reverse,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tough::schema::{Root as TufRoot, Signed, Timestamp};
use url::Url;

use crate::{
    kdl_format::mirror_list::{MIRROR_LIST_LIMIT, MirrorList, MirrorListKind, parse_mirror_list},
    transport::https_client,
};

use super::super::{Root, path_present, read_limited, write_atomic};
use super::{ARCHIVE, ROOT_LIMIT, TIMESTAMP_LIMIT, TUF_LIMITS, lock_repo, open_state, unix_now};

/// The TUF target that holds the publisher's signed mirror list.
pub(crate) const SIGNED_MIRRORS_TARGET: &str = "mirrors";
pub(crate) const SIGNED_MIRRORS_LIMIT: u64 = MIRROR_LIST_LIMIT;
/// The configured source, written by every sync.
pub(super) const ENDPOINTS: &str = "endpoints";
/// The order the last sync or ranking resolved, used by later fetches.
const EFFECTIVE: &str = "mirrors";
/// The verified copy of the publisher's signed mirror list.
pub(super) const SIGNED: &str = "signed-mirrors";
const RANKING: &str = "mirror-ranking.json";
const RANKING_LIMIT: u64 = 256 * 1024;
pub(super) const ENDPOINTS_LIMIT: u64 = 64 * 1024;
/// One operation tries at most this many mirrors before it fails, so an
/// explicit list such as `repo sync --url` and its fallbacks holds no more.
pub(super) const MAX_ATTEMPTS: usize = 5;
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const PROBES: usize = 8;

/// The metadata and targets directories of one mirror.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Endpoint {
    pub(super) metadata: Url,
    pub(super) targets: Url,
}

impl Endpoint {
    fn new(metadata: &str, targets: &str) -> Result<Self> {
        Ok(Self {
            metadata: endpoint(metadata)?,
            targets: endpoint(targets)?,
        })
    }

    pub(super) fn from_base(base: &Url) -> Result<Self> {
        Self::new(
            base.join("metadata/")?.as_str(),
            base.join("targets/")?.as_str(),
        )
    }

    fn https(&self) -> bool {
        self.metadata.scheme() == "https" && self.targets.scheme() == "https"
    }

    fn line(&self) -> String {
        format!("{} {}\n", self.metadata, self.targets)
    }
}

impl std::fmt::Display for Endpoint {
    /// A mirror with the usual layout is shown by its base URL.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.metadata.join("../") {
            Ok(base) if base.join("targets/").ok().as_ref() == Some(&self.targets) => {
                write!(f, "{base}")
            }
            _ => write!(f, "{} {}", self.metadata, self.targets),
        }
    }
}

pub(super) fn endpoint(value: &str) -> Result<Url> {
    let url = Url::parse(value).with_context(|| format!("invalid mirror URL {value}"))?;
    ensure!(
        matches!(url.scheme(), "file" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path().ends_with('/'),
        "mirror endpoint must be a directory URL with a trailing slash, using https:// or file://"
    );
    Ok(url)
}

pub(super) fn mirror_base(base: &str) -> Result<Url> {
    if base.ends_with('/') {
        endpoint(base)
    } else {
        endpoint(&format!("{base}/"))
    }
}

/// Mirrors of one signed TUF repository, in the order to try them.
#[derive(Clone, Debug)]
pub struct Mirror {
    pub(super) endpoints: Vec<Endpoint>,
}

impl Mirror {
    /// One mirror whose metadata and targets live at separate URLs.
    pub fn new(metadata: &str, targets: &str) -> Result<Self> {
        Ok(Self {
            endpoints: vec![Endpoint::new(metadata, targets)?],
        })
    }

    /// One mirror with `metadata/` and `targets/` under a base URL.
    pub fn from_base(base: &str) -> Result<Self> {
        Self::from_base_url(&mirror_base(base)?)
    }

    pub(super) fn from_base_url(base: &Url) -> Result<Self> {
        Ok(Self {
            endpoints: vec![Endpoint::from_base(base)?],
        })
    }

    /// Append more base URLs for the same signed repository, in preference order.
    pub fn with_fallbacks(mut self, urls: &[String]) -> Result<Self> {
        for url in urls {
            let endpoint = Endpoint::from_base(&mirror_base(url)?)?;
            ensure!(
                !self.endpoints.contains(&endpoint),
                "duplicate mirror URL {url}"
            );
            self.endpoints.push(endpoint);
        }
        ensure!(
            self.endpoints.len() <= MAX_ATTEMPTS,
            "use at most {} fallback mirror URLs: each operation tries at most {MAX_ATTEMPTS} mirrors",
            MAX_ATTEMPTS - 1
        );
        Ok(self)
    }

    pub(super) fn document(&self) -> String {
        self.endpoints.iter().map(Endpoint::line).collect()
    }

    fn parse(document: &str) -> Result<Self> {
        let endpoints = document
            .lines()
            .map(|line| {
                let (metadata, targets) = line
                    .split_once(' ')
                    .context("invalid cached mirror endpoints")?;
                Endpoint::new(metadata, targets)
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(!endpoints.is_empty(), "invalid cached mirror endpoints");
        Ok(Self { endpoints })
    }

    fn read_file(state: &Root, name: &str) -> Result<Self> {
        Self::parse(&String::from_utf8(read_limited(
            state
                .file(name)
                .context("run repo sync to configure this cache's mirror")?,
            ENDPOINTS_LIMIT,
        )?)?)
    }

    /// The order resolved by the last sync or ranking, for later fetches.
    pub(super) fn read(state: &Root) -> Result<Self> {
        if !path_present(&Root::fd_path(state.as_file()).join(EFFECTIVE))? {
            return Self::read_configured(state);
        }
        Self::read_file(state, EFFECTIVE).or_else(|error| {
            eprintln!(
                "Warning: ignoring the resolved mirror order: {error:#}. Using the configured source."
            );
            Self::read_configured(state)
        })
    }

    /// The configured source recorded by the last sync.
    pub(super) fn read_configured(state: &Root) -> Result<Self> {
        Self::read_file(state, ENDPOINTS)
    }
}

/// Where a mirror candidate came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MirrorSource {
    /// The administrator's local mirror list.
    Local,
    /// The URL the publisher was added from, or the image configuration.
    Configured,
    /// The publisher's signed mirror list.
    Signed,
}

impl std::fmt::Display for MirrorSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Local => "local",
            Self::Configured => "configured",
            Self::Signed => "signed",
        })
    }
}

struct Candidate {
    endpoint: Endpoint,
    source: MirrorSource,
    region: Option<String>,
}

/// The mirrors a sync may use for one publisher.
#[derive(Clone, Debug)]
pub struct MirrorSources {
    configured: Mirror,
    list: Option<PathBuf>,
    signed: bool,
}

impl MirrorSources {
    /// Exactly these mirrors, in this order, as `repo sync` uses them.
    pub fn explicit(mirror: Mirror) -> Self {
        Self {
            configured: mirror,
            list: None,
            signed: false,
        }
    }

    /// A publisher configured in an image: its configured source, the local
    /// list `CONFIG_DIR/mirrors/NAME.kdl`, and the publisher's signed list.
    pub(crate) fn publisher(configured: Mirror, config_dir: &Path, publisher: &str) -> Self {
        Self {
            configured,
            list: Some(local_list_path(config_dir, publisher)),
            signed: true,
        }
    }

    pub(super) fn configured(&self) -> &Mirror {
        &self.configured
    }

    fn candidates(&self, state: &Root) -> Result<Vec<Candidate>> {
        let local = match &self.list {
            Some(path) if path_present(path)? => Some(read_local_list(path)?),
            _ => None,
        };
        let mut pinned = Vec::new();
        let mut pool = Vec::new();
        let mut excluded = Vec::new();
        if let Some(local) = &local {
            for url in &local.exclude {
                excluded.push(Endpoint::from_base(&mirror_base(url)?)?);
            }
            for mirror in &local.mirrors {
                pinned.push(Candidate {
                    endpoint: Endpoint::from_base(&mirror_base(&mirror.url)?)?,
                    source: MirrorSource::Local,
                    region: mirror.region.clone(),
                });
            }
        }
        for endpoint in &self.configured.endpoints {
            pool.push(Candidate {
                endpoint: endpoint.clone(),
                source: MirrorSource::Configured,
                region: None,
            });
        }
        if self.signed
            && local.as_ref().is_none_or(|local| local.signed_mirrors)
            && let Some(signed) = read_signed_list(state)?
        {
            for mirror in signed.mirrors {
                pool.push(Candidate {
                    endpoint: signed_endpoint(&mirror.url)?,
                    source: MirrorSource::Signed,
                    region: mirror.region,
                });
            }
        }
        let regions = local.map(|local| local.regions).unwrap_or_default();
        if !regions.is_empty() {
            pool.sort_by_key(|candidate| {
                candidate
                    .region
                    .as_ref()
                    .and_then(|region| regions.iter().position(|preferred| preferred == region))
                    .unwrap_or(regions.len())
            });
        }
        if let Some(ranking) = read_ranking(state) {
            pool.sort_by_key(|candidate| ranking.order(&candidate.endpoint));
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        for candidate in pinned.into_iter().chain(pool) {
            if !excluded.contains(&candidate.endpoint)
                && !candidates
                    .iter()
                    .any(|seen| seen.endpoint == candidate.endpoint)
            {
                candidates.push(candidate);
            }
        }
        ensure!(
            !candidates.is_empty(),
            "the local mirror list excludes every mirror"
        );
        Ok(candidates)
    }

    /// The mirrors to try, best first.
    pub(super) fn resolve(&self, state: &Root) -> Result<Mirror> {
        Ok(Mirror {
            endpoints: self
                .candidates(state)?
                .into_iter()
                .map(|candidate| candidate.endpoint)
                .collect(),
        })
    }

    /// Record the configured source and the order later fetches use.
    pub(super) fn record(&self, repo: &Path, state: &Root) -> Result<()> {
        let directory = repo.join(".tuf");
        // Later fetches try only the first mirrors, so only those are kept.
        let mut effective = self.resolve(state)?;
        effective.endpoints.truncate(MAX_ATTEMPTS);
        let documents = [
            (ENDPOINTS, self.configured.document()),
            (EFFECTIVE, effective.document()),
        ];
        ensure!(
            documents
                .iter()
                .all(|(_, document)| document.len() as u64 <= ENDPOINTS_LIMIT),
            "the mirror URLs are too long to record"
        );
        for (name, document) in documents {
            write_atomic(&directory.join(name), document.as_bytes(), false)?;
        }
        Ok(())
    }
}

pub(crate) fn local_list_path(config_dir: &Path, publisher: &str) -> PathBuf {
    config_dir.join("mirrors").join(format!("{publisher}.kdl"))
}

fn read_local_list(path: &Path) -> Result<MirrorList> {
    (|| {
        let bytes = read_limited(std::fs::File::open(path)?, MIRROR_LIST_LIMIT)?;
        parse_mirror_list(std::str::from_utf8(&bytes)?, MirrorListKind::Local)
    })()
    .with_context(|| format!("reading mirror list {}", path.display()))
}

/// A signed list may name only HTTPS mirrors.
fn signed_endpoint(url: &str) -> Result<Endpoint> {
    let endpoint = Endpoint::from_base(&mirror_base(url)?)?;
    ensure!(
        endpoint.https(),
        "a signed mirror list may name only https:// mirrors, not {url}"
    );
    Ok(endpoint)
}

/// Check a signed mirror list before it is published or cached.
pub(crate) fn check_signed_list(bytes: &[u8]) -> Result<MirrorList> {
    let list = parse_mirror_list(std::str::from_utf8(bytes)?, MirrorListKind::Signed)
        .context("checking the signed mirror list")?;
    for mirror in &list.mirrors {
        signed_endpoint(&mirror.url)?;
    }
    Ok(list)
}

fn read_signed_list(state: &Root) -> Result<Option<MirrorList>> {
    if !path_present(&Root::fd_path(state.as_file()).join(SIGNED))? {
        return Ok(None);
    }
    check_signed_list(&read_limited(state.file(SIGNED)?, SIGNED_MIRRORS_LIMIT)?).map(Some)
}

/// What the last probe found at a mirror.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MirrorStatus {
    /// Serves the newest timestamp seen, with a valid signature.
    Current,
    /// Serves a valid but older timestamp.
    Behind,
    /// Serves an expired timestamp.
    Expired,
    /// Serves a timestamp that does not verify against the trusted root.
    Invalid,
    /// Could not be reached, or did not answer in time.
    Unreachable,
}

impl std::fmt::Display for MirrorStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Current => "current",
            Self::Behind => "behind",
            Self::Expired => "expired",
            Self::Invalid => "invalid",
            Self::Unreachable => "unreachable",
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MirrorProbe {
    pub metadata: String,
    pub targets: String,
    pub status: MirrorStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl MirrorProbe {
    fn matches(&self, endpoint: &Endpoint) -> bool {
        self.metadata == endpoint.metadata.as_str() && self.targets == endpoint.targets.as_str()
    }
}

/// Probe results, best mirror first.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct MirrorRanking {
    checked: u64,
    mirrors: Vec<MirrorProbe>,
}

impl MirrorRanking {
    /// Current mirrors by rank, then unprobed ones, then the rest by rank.
    fn order(&self, endpoint: &Endpoint) -> (u8, usize) {
        match self
            .mirrors
            .iter()
            .position(|probe| probe.matches(endpoint))
        {
            Some(index) if self.mirrors[index].status == MirrorStatus::Current => (0, index),
            None => (1, 0),
            Some(index) => (2, index),
        }
    }
}

fn read_ranking(state: &Root) -> Option<MirrorRanking> {
    let bytes = read_limited(state.file(RANKING).ok()?, RANKING_LIMIT).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// One mirror candidate as shown by `cps publisher mirror list`.
#[derive(Clone, Debug, Serialize)]
pub struct MirrorEntry {
    pub url: String,
    pub source: MirrorSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<MirrorProbe>,
}

/// A publisher's mirrors in the order the next operation tries them.
#[derive(Clone, Debug, Serialize)]
pub struct PublisherMirrors {
    pub publisher: String,
    /// Unix time of the last ranking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ranked: Option<u64>,
    pub mirrors: Vec<MirrorEntry>,
    /// Why ranking this publisher failed. Other publishers are still ranked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl std::fmt::Display for PublisherMirrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Publisher {}", self.publisher)?;
        match self.ranked {
            Some(time) => writeln!(
                f,
                " (ranked {} hours ago)",
                unix_now().saturating_sub(time) / 3600
            )?,
            None => writeln!(f, " (not ranked)")?,
        }
        if let Some(error) = &self.error {
            writeln!(f, "  Ranking failed: {error}")?;
        }
        for (index, mirror) in self.mirrors.iter().enumerate() {
            write!(f, "{:>3}. {} [{}", index + 1, mirror.url, mirror.source)?;
            if let Some(region) = &mirror.region {
                write!(f, ", {region}")?;
            }
            write!(f, "]")?;
            if let Some(probe) = &mirror.probe {
                write!(f, " {}", probe.status)?;
                if let Some(version) = probe.version {
                    write!(f, ", timestamp {version}")?;
                }
                if let Some(latency) = probe.latency_ms {
                    write!(f, ", {latency} ms")?;
                }
                if let Some(error) = &probe.error {
                    write!(f, ": {error}")?;
                }
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

/// The image's configuration directory, which holds `mirrors/NAME.kdl`.
pub(crate) fn image_config_dir(root: &Path) -> PathBuf {
    root.join("etc/cps")
}

/// Publishers of an image that sync from TUF mirrors, with their caches.
fn mirrored_publishers(
    root: &Path,
    names: &[String],
) -> Result<Vec<(String, PathBuf, MirrorSources)>> {
    let image = crate::image::Image::open_query(root)?;
    let publishers = image.publishers()?;
    for name in names {
        ensure!(
            publishers.get(name).is_some(),
            "image has no publisher {name}"
        );
    }
    let config_dir = image_config_dir(root);
    let mut mirrored = Vec::new();
    for publisher in publishers.as_slice() {
        if !names.is_empty() && !names.contains(&publisher.name) {
            continue;
        }
        let configured = (|| {
            let state = publisher.repo.join(".tuf");
            if !path_present(&state.join(ENDPOINTS))? || path_present(&state.join(ARCHIVE))? {
                return Ok(None);
            }
            Mirror::read_configured(&Root::open(&publisher.repo)?.subdir(".tuf")?).map(Some)
        })()
        .map_err(|error| state_error(error, &publisher.name))?;
        let Some(configured) = configured else {
            ensure!(
                !names.contains(&publisher.name),
                "publisher {} does not sync from a TUF mirror",
                publisher.name
            );
            continue;
        };
        let sources = MirrorSources::publisher(configured, &config_dir, &publisher.name);
        mirrored.push((publisher.name.clone(), publisher.repo.clone(), sources));
    }
    Ok(mirrored)
}

/// The TUF state is private to the account that syncs it, usually root.
fn state_error(error: anyhow::Error, publisher: &str) -> anyhow::Error {
    let denied = error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
            || cause.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::ACCESS)
    });
    let error = error.context(format!("reading the mirror state of publisher {publisher}"));
    if denied {
        error.context("run this as the account that syncs the image, usually with sudo")
    } else {
        error
    }
}

fn describe(name: &str, state: &Root, sources: &MirrorSources) -> Result<PublisherMirrors> {
    let ranking = read_ranking(state);
    let mirrors = sources
        .candidates(state)?
        .into_iter()
        .map(|candidate| MirrorEntry {
            url: candidate.endpoint.to_string(),
            source: candidate.source,
            region: candidate.region,
            probe: ranking.as_ref().and_then(|ranking| {
                ranking
                    .mirrors
                    .iter()
                    .find(|probe| probe.matches(&candidate.endpoint))
                    .cloned()
            }),
        })
        .collect();
    Ok(PublisherMirrors {
        publisher: name.to_owned(),
        ranked: ranking.map(|ranking| ranking.checked),
        mirrors,
        error: None,
    })
}

/// Show each mirrored publisher's candidates in the order they are tried.
pub fn list_publisher_mirrors(root: &Path, names: &[String]) -> Result<Vec<PublisherMirrors>> {
    mirrored_publishers(root, names)?
        .into_iter()
        .map(|(name, repo, sources)| {
            // Listing reads the cache without taking its lock.
            (|| describe(&name, &Root::open(&repo)?.subdir(".tuf")?, &sources))()
                .map_err(|error| state_error(error, &name))
        })
        .collect()
}

/// Probe every candidate's signed timestamp and rank the mirrors.
///
/// A publisher that cannot be ranked, for example because it was never
/// synced, keeps its previous order and is reported with its error; the
/// other publishers are still ranked. Check [`PublisherMirrors::error`].
pub fn rank_publisher_mirrors(root: &Path, names: &[String]) -> Result<Vec<PublisherMirrors>> {
    let mut report = Vec::new();
    for (name, repo, sources) in mirrored_publishers(root, names)? {
        let ranked = (|| {
            let _lock = lock_repo(&repo)?;
            let state = open_state(&repo)?;
            let ranking = rank(&state, &sources)?;
            write_atomic(
                &repo.join(".tuf").join(RANKING),
                &serde_json::to_vec_pretty(&ranking)?,
                false,
            )?;
            sources.record(&repo, &state)?;
            describe(&name, &state, &sources)
        })();
        report.push(match ranked {
            Ok(mirrors) => mirrors,
            Err(error) => {
                let error = format!("{:#}", state_error(error, &name));
                let state = Root::open(&repo).and_then(|repo| repo.subdir(".tuf"));
                let mut mirrors = state
                    .and_then(|state| describe(&name, &state, &sources))
                    .unwrap_or_else(|_| PublisherMirrors {
                        publisher: name.clone(),
                        ranked: None,
                        mirrors: Vec::new(),
                        error: None,
                    });
                mirrors.error = Some(error);
                mirrors
            }
        });
    }
    Ok(report)
}

fn rank(state: &Root, sources: &MirrorSources) -> Result<MirrorRanking> {
    let root: Signed<TufRoot> = serde_json::from_slice(&read_limited(
        state
            .file("trusted/root.json")
            .context("sync the publisher before ranking its mirrors")?,
        ROOT_LIMIT,
    )?)
    .context("reading the trusted TUF root")?;
    let trusted_version = state
        .file("trusted/timestamp.json")
        .ok()
        .and_then(|file| read_limited(file, TIMESTAMP_LIMIT).ok())
        .and_then(|bytes| serde_json::from_slice::<Signed<Timestamp>>(&bytes).ok())
        .map_or(0, |timestamp| timestamp.signed.version.get());
    let candidates = sources.candidates(state)?;
    let client = candidates
        .iter()
        .any(|candidate| candidate.endpoint.metadata.scheme() == "https")
        .then(|| https_client().timeout(PROBE_TIMEOUT).build())
        .transpose()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut probes = runtime.block_on(
        stream::iter(&candidates)
            .map(|candidate| probe(client.as_ref(), &candidate.endpoint, &root))
            .buffered(PROBES)
            .collect::<Vec<_>>(),
    );
    let newest = probes
        .iter()
        .filter(|probe| probe.status == MirrorStatus::Current)
        .filter_map(|probe| probe.version)
        .max()
        .unwrap_or(0)
        .max(trusted_version);
    for probe in &mut probes {
        if probe.status == MirrorStatus::Current && probe.version < Some(newest) {
            probe.status = MirrorStatus::Behind;
        }
    }
    probes.sort_by_key(|probe| {
        (
            probe.status,
            Reverse(probe.version),
            probe.latency_ms.unwrap_or(u64::MAX),
        )
    });
    Ok(MirrorRanking {
        checked: unix_now(),
        mirrors: probes,
    })
}

async fn probe(
    client: Option<&Client>,
    endpoint: &Endpoint,
    root: &Signed<TufRoot>,
) -> MirrorProbe {
    let mut result = MirrorProbe {
        metadata: endpoint.metadata.to_string(),
        targets: endpoint.targets.to_string(),
        status: MirrorStatus::Unreachable,
        version: None,
        expires: None,
        latency_ms: None,
        error: None,
    };
    let started = Instant::now();
    let body = match fetch_metadata(client, endpoint, "timestamp.json", TIMESTAMP_LIMIT).await {
        Ok(Some(body)) => body,
        Ok(None) => {
            result.error = Some("the mirror has no timestamp.json".to_owned());
            return result;
        }
        Err(error) => {
            result.error = Some(format!("{error:#}"));
            return result;
        }
    };
    result.latency_ms = Some(started.elapsed().as_millis().try_into().unwrap_or(u64::MAX));
    // A publisher that rotated its keys signs with a newer root.
    let root = match latest_root(client, endpoint, root).await {
        Ok(root) => root,
        Err(error) => {
            result.status = MirrorStatus::Invalid;
            result.error = Some(format!("{error:#}"));
            return result;
        }
    };
    let checked = serde_json::from_slice::<Signed<Timestamp>>(&body)
        .context("parsing timestamp.json")
        .and_then(|timestamp| {
            root.signed
                .verify_role(&timestamp)
                .context("checking the timestamp signatures")?;
            Ok(timestamp)
        });
    match checked {
        Ok(timestamp) => {
            result.version = Some(timestamp.signed.version.get());
            result.expires = Some(timestamp.signed.expires.to_string());
            result.status = if timestamp.signed.expires > jiff::Timestamp::now() {
                MirrorStatus::Current
            } else {
                MirrorStatus::Expired
            };
        }
        Err(error) => {
            result.status = MirrorStatus::Invalid;
            result.error = Some(format!("{error:#}"));
        }
    }
    result
}

/// Follow the root history a mirror publishes after the trusted root, as a
/// client's next sync would. Each root must be signed by its predecessor's
/// root keys and by its own.
async fn latest_root<'a>(
    client: Option<&Client>,
    endpoint: &Endpoint,
    trusted: &'a Signed<TufRoot>,
) -> Result<std::borrow::Cow<'a, Signed<TufRoot>>> {
    let mut root = std::borrow::Cow::Borrowed(trusted);
    for _ in 0..TUF_LIMITS.max_root_updates {
        let version = root.signed.version.get() + 1;
        let name = format!("{version}.root.json");
        let Some(bytes) = fetch_metadata(client, endpoint, &name, ROOT_LIMIT).await? else {
            break;
        };
        let next: Signed<TufRoot> =
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {name}"))?;
        ensure!(
            next.signed.version.get() == version,
            "{name} holds root version {}",
            next.signed.version
        );
        root.signed
            .verify_role(&next)
            .and_then(|()| next.signed.verify_role(&next))
            .with_context(|| format!("checking the signatures of {name}"))?;
        root = std::borrow::Cow::Owned(next);
    }
    Ok(root)
}

/// A metadata file of a mirror, or `None` when the mirror does not have it.
async fn fetch_metadata(
    client: Option<&Client>,
    endpoint: &Endpoint,
    name: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>> {
    let url = endpoint.metadata.join(name)?;
    if url.scheme() == "file" {
        let path = url
            .to_file_path()
            .map_err(|()| anyhow::anyhow!("invalid file URL {url}"))?;
        return match std::fs::File::open(path) {
            Ok(file) => read_limited(file, limit).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        };
    }
    let client = client.context("missing HTTPS client")?;
    get_limited(client, url, limit).await
}

/// Fetch a small file over HTTPS, or `None` when the server does not have it.
pub(super) async fn get_limited(client: &Client, url: Url, limit: u64) -> Result<Option<Vec<u8>>> {
    let response = client.get(url).send().await?;
    match response.status() {
        StatusCode::NOT_FOUND | StatusCode::GONE => return Ok(None),
        StatusCode::OK => {}
        status => anyhow::bail!("HTTP status {status}"),
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.try_next().await? {
        ensure!(
            body.len() + chunk.len() <= limit as usize,
            "the file exceeds the {limit}-byte limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_lists_pin_exclude_and_disable_signed_mirrors() -> Result<()> {
        let work = tempfile::tempdir()?;
        let state = work.path().join(".tuf");
        std::fs::create_dir(&state)?;
        std::fs::write(
            state.join(SIGNED),
            r#"schema-version 1 kind="mirror-list"
mirror "https://us.example/canopa/" region="us"
mirror "https://eu.example/canopa/" region="eu"
mirror "https://slow.example/canopa/"
"#,
        )?;
        let config = work.path().join("etc");
        let configured = Mirror::from_base("https://origin.example/canopa")?;
        let sources = MirrorSources::publisher(configured.clone(), &config, "canopa");
        let directory = Root::open(&state)?;
        let urls = |sources: &MirrorSources| -> Result<Vec<String>> {
            Ok(sources
                .resolve(&directory)?
                .endpoints
                .iter()
                .map(ToString::to_string)
                .collect())
        };
        assert_eq!(
            urls(&sources)?,
            [
                "https://origin.example/canopa/",
                "https://us.example/canopa/",
                "https://eu.example/canopa/",
                "https://slow.example/canopa/",
            ]
        );
        std::fs::create_dir_all(config.join("mirrors"))?;
        std::fs::write(
            config.join("mirrors/canopa.kdl"),
            r#"schema-version 1 kind="mirror-list"
mirror "https://lan.example/canopa/"
exclude "https://slow.example/canopa"
region "eu"
"#,
        )?;
        assert_eq!(
            urls(&sources)?,
            [
                "https://lan.example/canopa/",
                "https://eu.example/canopa/",
                "https://origin.example/canopa/",
                "https://us.example/canopa/",
            ]
        );
        std::fs::write(
            config.join("mirrors/canopa.kdl"),
            "schema-version 1 kind=\"mirror-list\"\nsigned-mirrors #false\n",
        )?;
        assert_eq!(urls(&sources)?, ["https://origin.example/canopa/"]);
        assert_eq!(
            urls(&MirrorSources::explicit(configured))?,
            ["https://origin.example/canopa/"]
        );

        // A ranking moves current mirrors first and failed ones last.
        std::fs::remove_file(config.join("mirrors/canopa.kdl"))?;
        let probe = |url: &str, status| MirrorProbe {
            metadata: format!("{url}metadata/"),
            targets: format!("{url}targets/"),
            status,
            version: None,
            expires: None,
            latency_ms: None,
            error: None,
        };
        let ranking = MirrorRanking {
            checked: 0,
            mirrors: vec![
                probe("https://eu.example/canopa/", MirrorStatus::Current),
                probe("https://origin.example/canopa/", MirrorStatus::Unreachable),
            ],
        };
        std::fs::write(state.join(RANKING), serde_json::to_vec(&ranking)?)?;
        assert_eq!(
            urls(&sources)?,
            [
                "https://eu.example/canopa/",
                "https://us.example/canopa/",
                "https://slow.example/canopa/",
                "https://origin.example/canopa/",
            ]
        );
        Ok(())
    }

    #[test]
    fn signed_lists_name_only_https_mirrors() {
        assert!(
            check_signed_list(
                b"schema-version 1 kind=\"mirror-list\"\nmirror \"file:///srv/canopa/\"\n"
            )
            .is_err()
        );
        assert!(
            check_signed_list(
                b"schema-version 1 kind=\"mirror-list\"\nmirror \"https://a.example/\"\n"
            )
            .is_ok()
        );
    }

    #[test]
    fn endpoint_documents_round_trip() -> Result<()> {
        let mirror = Mirror::new("https://m.example/meta/", "https://cdn.example/t/")?
            .with_fallbacks(&["https://b.example/canopa".to_owned()])?;
        let parsed = Mirror::parse(&mirror.document())?;
        assert_eq!(parsed.endpoints, mirror.endpoints);
        assert_eq!(parsed.endpoints[1].to_string(), "https://b.example/canopa/");
        assert_eq!(
            parsed.endpoints[0].to_string(),
            "https://m.example/meta/ https://cdn.example/t/"
        );
        assert!(
            Mirror::from_base("https://a.example/")?
                .with_fallbacks(&["https://a.example".to_owned()])
                .is_err()
        );
        // Each operation tries at most five mirrors, so no more are accepted.
        let fallbacks = |count: usize| {
            (0..count)
                .map(|index| format!("https://m{index}.example/"))
                .collect::<Vec<_>>()
        };
        let base = Mirror::from_base("https://a.example/")?;
        assert!(base.clone().with_fallbacks(&fallbacks(4)).is_ok());
        assert!(base.with_fallbacks(&fallbacks(5)).is_err());
        Ok(())
    }
}
