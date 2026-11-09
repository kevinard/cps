use std::{
    collections::{BTreeSet, HashMap},
    fs, io,
    num::NonZeroU64,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use rustix::fs::{RenameFlags, renameat_with};
use tempfile::TempDir;
use tough::{
    async_trait,
    editor::{RepositoryEditor, signed::SignedRole},
    key_source::{KeySource, LocalKeySource},
    schema::{
        Hashes, KeyHolder, Metafile, PathHashPrefix, PathSet, RoleType, Root, Signed, Snapshot,
        Timestamp, decoded::Decoded, key::Key,
    },
    sign::Sign,
};

use super::{
    ARCHIVE, PUBLISHER_KEY_TARGET, PUBLISHER_NAME_TARGET, RETAINED, ROLE_LIMIT, ROOT_LIMIT,
    TIMESTAMP_LIMIT,
};
use crate::{
    model::digest,
    repo::{
        keys::signer::{
            CommandSigner, KeySpec, SignRequest, tuf_algorithm, tuf_public_key,
            verify_tuf_signature,
        },
        read_limited,
    },
};

const PREFIXES: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];
const MAX_ROLE_KEYS: usize = 32;

/// Key sources for the online TUF roles of a CPS mirror.
///
/// Each entry is `PATH`, `file:PATH`, or `command:DESCRIPTOR`. The root
/// metadata decides which keys belong to the targets, snapshot, and timestamp
/// roles and how many signatures each needs. The `objects-*` delegations use
/// `objects` keys with `objects_threshold`, or the first targets key when no
/// objects keys are given.
#[derive(Clone, Copy, Debug)]
pub struct ReleaseKeys<'a> {
    pub targets: &'a [PathBuf],
    pub snapshot: &'a [PathBuf],
    pub timestamp: &'a [PathBuf],
    pub objects: &'a [PathBuf],
    pub objects_threshold: NonZeroU64,
}

/// Sign a flat CPS export using stable TUF roles for content-addressed targets.
pub struct SignExport<'a> {
    pub root: &'a Path,
    pub export: &'a Path,
    pub output: &'a Path,
    pub keys: ReleaseKeys<'a>,
    pub version: NonZeroU64,
    pub release_expires: &'a str,
    pub timestamp_expires: &'a str,
}

/// Re-sign the timestamp, and optionally the snapshot, over published metadata.
///
/// This needs neither the targets keys nor the export. Without snapshot keys,
/// the new timestamp covers the existing snapshot byte for byte. With snapshot
/// keys, a new snapshot lists exactly the role versions and digests of the
/// existing one under a new version and expiry.
pub struct RefreshMetadata<'a> {
    pub root: &'a Path,
    pub snapshot: &'a Path,
    pub output: &'a Path,
    pub timestamp_keys: &'a [PathBuf],
    pub snapshot_keys: &'a [PathBuf],
    pub version: NonZeroU64,
    pub snapshot_expires: Option<&'a str>,
    pub timestamp_expires: &'a str,
}

/// A TUF key held by an external signer.
#[derive(Debug)]
struct CommandKeySource {
    signer: Arc<CommandSigner>,
    key: Key,
}

impl CommandKeySource {
    fn open(descriptor: &Path) -> Result<Self> {
        let signer = CommandSigner::open(descriptor)?;
        let key = tuf_public_key(&signer.public_key()?)
            .with_context(|| format!("reading the public key for {}", descriptor.display()))?;
        Ok(Self {
            signer: Arc::new(signer),
            key,
        })
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[async_trait]
impl KeySource for CommandKeySource {
    async fn as_sign(&self) -> std::result::Result<Box<dyn Sign>, BoxError> {
        Ok(Box::new(CommandSign {
            signer: self.signer.clone(),
            key_id: hex::encode(self.key.key_id()?),
            key: self.key.clone(),
        }))
    }

    async fn write(&self, _value: &str, _key_id: &str) -> std::result::Result<(), BoxError> {
        Err("an external signer cannot store a key".into())
    }
}

struct CommandSign {
    signer: Arc<CommandSigner>,
    key: Key,
    key_id: String,
}

#[async_trait]
impl Sign for CommandSign {
    fn tuf_key(&self) -> Key {
        self.key.clone()
    }

    async fn sign(
        &self,
        message: &[u8],
        _rng: &(dyn SecureRandom + Sync),
    ) -> std::result::Result<Vec<u8>, BoxError> {
        let request = SignRequest {
            context: "tuf",
            key_id: &self.key_id,
            algorithm: tuf_algorithm(&self.key),
        };
        let signature = self.signer.run(&request, message)?;
        verify_tuf_signature(&self.key, message, &signature)
            .with_context(|| format!("checking the external signature for key {}", self.key_id))?;
        Ok(signature)
    }
}

/// Open key sources and return them with their distinct TUF key IDs.
async fn open_keys(
    specs: &[PathBuf],
    role: &str,
) -> Result<(Vec<Box<dyn KeySource>>, Vec<String>)> {
    ensure!(
        specs.len() <= MAX_ROLE_KEYS,
        "at most {MAX_ROLE_KEYS} {role} keys are supported"
    );
    let mut sources = Vec::new();
    let mut ids = Vec::new();
    for spec in specs {
        let source: Box<dyn KeySource> = match KeySpec::parse(spec) {
            KeySpec::File(path) => Box::new(LocalKeySource { path }),
            KeySpec::Command(descriptor) => Box::new(CommandKeySource::open(&descriptor)?),
        };
        let id = hex::encode(
            source
                .as_sign()
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))
                .with_context(|| format!("loading {role} key {}", spec.display()))?
                .tuf_key()
                .key_id()?,
        );
        // A repeated key would add a duplicate signature, which clients reject.
        ensure!(!ids.contains(&id), "{role} key {id} is listed twice");
        ids.push(id);
        sources.push(source);
    }
    Ok((sources, ids))
}

/// Require that every supplied key belongs to the role and that they meet its threshold.
fn check_role_keys(root: &Root, role: RoleType, ids: &[String]) -> Result<()> {
    let keys = root
        .roles
        .get(&role)
        .with_context(|| format!("the TUF root has no {role} role"))?;
    let authorized: BTreeSet<_> = keys.keyids.iter().map(hex::encode).collect();
    for id in ids {
        ensure!(
            authorized.contains(id),
            "key {id} is not a {role} key in the TUF root"
        );
    }
    ensure!(
        ids.len() as u64 >= keys.threshold.get(),
        "the TUF root requires {} {role} signatures, but {} {role} keys were supplied",
        keys.threshold,
        ids.len()
    );
    Ok(())
}

fn read_root(path: &Path) -> Result<Signed<Root>> {
    let root: Signed<Root> =
        serde_json::from_slice(&read_limited(fs::File::open(path)?, ROOT_LIMIT)?)
            .with_context(|| format!("reading TUF root {}", path.display()))?;
    ensure!(
        root.signed.consistent_snapshot,
        "use a consistent-snapshot TUF root"
    );
    Ok(root)
}

/// Stage signed output beside its destination and publish it as a new directory.
fn publish_stage(output: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let output_name = output.file_name().context("missing mirror output name")?;
    let stage = TempDir::new_in(parent)?;
    write(stage.path())?;
    let directory = fs::File::open(parent)?;
    renameat_with(
        &directory,
        stage
            .path()
            .file_name()
            .context("missing mirror staging name")?,
        &directory,
        output_name,
        RenameFlags::NOREPLACE,
    )?;
    directory.sync_all()?;
    Ok(())
}

fn check_metadata_limits(metadata: &Path) -> Result<()> {
    for entry in fs::read_dir(metadata)? {
        let entry = entry?;
        let limit = if entry.file_name() == "timestamp.json" {
            TIMESTAMP_LIMIT
        } else {
            ROLE_LIMIT
        };
        ensure!(
            entry.metadata()?.len() <= limit,
            "TUF metadata exceeds the client size limit at {}",
            entry.path().display()
        );
    }
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

pub fn sign_export(request: SignExport<'_>) -> Result<()> {
    let root = read_root(request.root)?;
    let keys = request.keys;
    ensure!(!keys.targets.is_empty(), "supply at least one targets key");
    publish_stage(request.output, |stage| {
        let targets = stage.join("targets");
        let metadata = stage.join("metadata");
        fs::create_dir(&targets)?;
        runtime()?.block_on(async {
            let (targets_keys, targets_ids) = open_keys(keys.targets, "targets").await?;
            let (snapshot_keys, snapshot_ids) = open_keys(keys.snapshot, "snapshot").await?;
            let (timestamp_keys, timestamp_ids) = open_keys(keys.timestamp, "timestamp").await?;
            check_role_keys(&root.signed, RoleType::Targets, &targets_ids)?;
            check_role_keys(&root.signed, RoleType::Snapshot, &snapshot_ids)?;
            check_role_keys(&root.signed, RoleType::Timestamp, &timestamp_ids)?;
            let objects_keys = if keys.objects.is_empty() {
                // Match the original single-key layout unless operators choose otherwise.
                open_keys(&keys.targets[..1], "objects").await?.0
            } else {
                open_keys(keys.objects, "objects").await?.0
            };
            ensure!(
                keys.objects_threshold.get() <= objects_keys.len() as u64,
                "the objects threshold of {} exceeds the {} objects keys",
                keys.objects_threshold,
                objects_keys.len()
            );
            let mut editor = RepositoryEditor::new(request.root).await?;
            let expires = request
                .release_expires
                .parse()
                .context("invalid release expiry")?;
            let version = request.version;
            editor
                .targets_version(version)?
                .targets_expires(expires)?
                .snapshot_version(version)
                .snapshot_expires(expires)
                .timestamp_version(version)
                .timestamp_expires(
                    request
                        .timestamp_expires
                        .parse()
                        .context("invalid timestamp expiry")?,
                );
            let mut shards: [_; PREFIXES.len()] = std::array::from_fn(|_| Vec::new());
            for entry in fs::read_dir(request.export)? {
                let entry = entry?;
                ensure!(
                    entry.file_type()?.is_file(),
                    "export contains a non-file entry"
                );
                let (name, target) = RepositoryEditor::build_target(entry.path()).await?;
                ensure!(
                    name.resolved()
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte)),
                    "invalid CPS mirror target name {:?}",
                    name.resolved()
                );
                let target_path = targets.join(format!(
                    "{}.{}",
                    hex::encode(target.hashes.sha256.as_ref()),
                    name.resolved()
                ));
                if let Err(error) = fs::hard_link(entry.path(), &target_path) {
                    if error.kind() != io::ErrorKind::CrossesDevices {
                        return Err(error.into());
                    }
                    fs::copy(entry.path(), &target_path)?;
                }
                if matches!(
                    name.resolved(),
                    "current" | ARCHIVE | RETAINED | PUBLISHER_NAME_TARGET | PUBLISHER_KEY_TARGET
                ) {
                    editor.add_target(name, target)?;
                } else {
                    let hash = digest(name.resolved().as_bytes());
                    let prefix = usize::from(u8::from_str_radix(&hash[..1], 16)?);
                    shards[prefix].push((name, target));
                }
            }
            // Keep every role, including empty roles. A newer TUF snapshot
            // cannot drop roles from the previous trusted snapshot.
            for prefix in PREFIXES {
                editor
                    .delegate_role(
                        &format!("objects-{prefix}"),
                        &objects_keys,
                        PathSet::PathHashPrefixes(vec![PathHashPrefix::new(prefix.to_string())?]),
                        true,
                        keys.objects_threshold,
                        expires,
                        version,
                    )
                    .await?;
            }
            editor.sign_targets_editor(&targets_keys).await?;
            for (prefix, targets) in PREFIXES.into_iter().zip(shards) {
                if targets.is_empty() {
                    continue;
                }
                editor.change_delegated_targets(&format!("objects-{prefix}"))?;
                editor.targets_version(version)?.targets_expires(expires)?;
                for (name, target) in targets {
                    editor.add_target(name, target)?;
                }
                editor.sign_targets_editor(&objects_keys).await?;
            }
            let mut role_keys = snapshot_keys;
            role_keys.extend(timestamp_keys);
            editor.sign(&role_keys).await?.write(&metadata).await?;
            anyhow::Ok(())
        })?;
        check_metadata_limits(&metadata)
    })
}

fn signed_metafile<T: tough::schema::Role>(role: &SignedRole<T>) -> Metafile {
    Metafile {
        hashes: Some(Hashes {
            sha256: Decoded::from(role.sha256().to_vec()),
            _extra: HashMap::new(),
        }),
        length: Some(*role.length()),
        version: role.signed().signed.version(),
        _extra: HashMap::new(),
    }
}

pub fn refresh_metadata(request: RefreshMetadata<'_>) -> Result<()> {
    let root = read_root(request.root)?;
    let bytes = read_limited(fs::File::open(request.snapshot)?, ROLE_LIMIT)
        .with_context(|| format!("reading TUF snapshot {}", request.snapshot.display()))?;
    let snapshot: Signed<Snapshot> = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing TUF snapshot {}", request.snapshot.display()))?;
    root.signed
        .verify_role(&snapshot)
        .context("the snapshot is not signed by the root's snapshot keys")?;
    ensure!(
        request.version > snapshot.signed.version,
        "the refreshed version must exceed snapshot version {}",
        snapshot.signed.version
    );
    ensure!(
        request.snapshot_keys.is_empty() == request.snapshot_expires.is_none(),
        "supply snapshot keys and a snapshot expiry together"
    );
    let timestamp_expires: jiff::Timestamp = request
        .timestamp_expires
        .parse()
        .context("invalid timestamp expiry")?;
    let now = jiff::Timestamp::now();
    ensure!(
        timestamp_expires > now,
        "the timestamp expiry is in the past"
    );
    let holder = KeyHolder::Root(root.signed.clone());
    let rng = SystemRandom::new();
    publish_stage(request.output, |stage| {
        runtime()?.block_on(async {
            let (timestamp_keys, timestamp_ids) =
                open_keys(request.timestamp_keys, "timestamp").await?;
            check_role_keys(&root.signed, RoleType::Timestamp, &timestamp_ids)?;
            let (meta, snapshot_expires) = if let Some(expires) = request.snapshot_expires {
                let (snapshot_keys, snapshot_ids) =
                    open_keys(request.snapshot_keys, "snapshot").await?;
                check_role_keys(&root.signed, RoleType::Snapshot, &snapshot_ids)?;
                let expires: jiff::Timestamp =
                    expires.parse().context("invalid snapshot expiry")?;
                ensure!(expires > now, "the snapshot expiry is in the past");
                let mut next = Snapshot::new(
                    snapshot.signed.spec_version.clone(),
                    request.version,
                    expires,
                );
                next.meta = snapshot.signed.meta.clone();
                next._extra = snapshot.signed._extra.clone();
                let signed = SignedRole::new(next, &holder, &snapshot_keys, &rng).await?;
                fs::write(
                    stage.join(format!("{}.snapshot.json", request.version)),
                    signed.buffer(),
                )?;
                (signed_metafile(&signed), expires)
            } else {
                ensure!(
                    snapshot.signed.expires > now,
                    "the snapshot expired at {}; re-sign it with snapshot keys",
                    snapshot.signed.expires
                );
                let hash = ring::digest::digest(&ring::digest::SHA256, &bytes);
                (
                    Metafile {
                        hashes: Some(Hashes {
                            sha256: Decoded::from(hash.as_ref().to_vec()),
                            _extra: HashMap::new(),
                        }),
                        length: Some(bytes.len() as u64),
                        version: snapshot.signed.version,
                        _extra: HashMap::new(),
                    },
                    snapshot.signed.expires,
                )
            };
            if snapshot_expires < timestamp_expires {
                eprintln!(
                    "Warning: the snapshot expires at {snapshot_expires}, before the timestamp."
                );
            }
            let mut timestamp = Timestamp::new(
                snapshot.signed.spec_version.clone(),
                request.version,
                timestamp_expires,
            );
            timestamp.meta.insert("snapshot.json".to_owned(), meta);
            let signed = SignedRole::new(timestamp, &holder, &timestamp_keys, &rng).await?;
            fs::write(stage.join("timestamp.json"), signed.buffer())?;
            anyhow::Ok(())
        })?;
        check_metadata_limits(stage)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt};
    use tough::{
        RepositoryLoader,
        schema::{Role, RoleKeys, Signature},
        sign::parse_keypair,
    };

    struct Fixture {
        work: TempDir,
        root: PathBuf,
        keys: BTreeMap<&'static str, Vec<PathBuf>>,
    }

    fn generate(path: &Path) -> Result<Key> {
        let document =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .expect("generate TUF key");
        fs::write(path, document.as_ref())?;
        Ok(parse_keypair(document.as_ref())?.tuf_key())
    }

    /// A root with two targets keys and a 2-of-2 targets threshold.
    fn fixture() -> Result<Fixture> {
        let work = tempfile::tempdir()?;
        let mut keys = BTreeMap::new();
        let mut all = HashMap::new();
        let mut roles = HashMap::new();
        for (role, kind, count, threshold) in [
            ("root", RoleType::Root, 1, 1),
            ("targets", RoleType::Targets, 2, 2),
            ("snapshot", RoleType::Snapshot, 1, 1),
            ("timestamp", RoleType::Timestamp, 1, 1),
        ] {
            let mut ids = Vec::new();
            for index in 0..count {
                let path = work.path().join(format!("{role}-{index}.der"));
                let key = generate(&path)?;
                let id = key.key_id()?;
                all.insert(id.clone(), key);
                ids.push(id);
                keys.entry(role).or_insert_with(Vec::new).push(path);
            }
            let role_keys = serde_json::from_value::<RoleKeys>(serde_json::json!({
                "keyids": ids,
                "threshold": threshold,
            }))?;
            roles.insert(kind, role_keys);
        }
        let root = Root {
            spec_version: "1.0.0".to_owned(),
            consistent_snapshot: true,
            version: NonZeroU64::MIN,
            expires: jiff::Timestamp::now().checked_add(jiff::Span::new().hours(720))?,
            keys: all,
            roles,
            _extra: HashMap::new(),
        };
        let canonical = root.canonical_form()?;
        let root_key = fs::read(&keys["root"][0])?;
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(&root_key).expect("root key");
        let signed = Signed {
            signatures: vec![Signature {
                keyid: parse_keypair(&root_key)?.tuf_key().key_id()?,
                sig: pair.sign(&canonical).as_ref().to_vec().into(),
            }],
            signed: root,
        };
        let path = work.path().join("root.json");
        fs::write(&path, serde_json::to_vec(&signed)?)?;
        let export = work.path().join("export");
        fs::create_dir(&export)?;
        fs::write(export.join("current"), format!("{}\n", digest(b"catalog")))?;
        fs::write(export.join("manifests-abc"), b"manifest")?;
        Ok(Fixture {
            work,
            root: path,
            keys,
        })
    }

    fn in_hours(hours: i64) -> String {
        jiff::Timestamp::now()
            .checked_add(jiff::Span::new().hours(hours))
            .unwrap()
            .to_string()
    }

    impl Fixture {
        fn path(&self, name: &str) -> PathBuf {
            self.work.path().join(name)
        }

        fn sign(
            &self,
            output: &str,
            targets: &[PathBuf],
            objects: &[PathBuf],
            threshold: u64,
        ) -> Result<()> {
            sign_export(SignExport {
                root: &self.root,
                export: &self.path("export"),
                output: &self.path(output),
                keys: ReleaseKeys {
                    targets,
                    snapshot: &self.keys["snapshot"],
                    timestamp: &self.keys["timestamp"],
                    objects,
                    objects_threshold: NonZeroU64::new(threshold).unwrap(),
                },
                version: NonZeroU64::MIN,
                release_expires: &in_hours(48),
                timestamp_expires: &in_hours(24),
            })
        }

        fn load(&self, mirror: &str) -> Result<tough::Repository> {
            let mirror = self.path(mirror);
            let root = fs::read(&self.root)?;
            let datastore = tempfile::tempdir()?;
            runtime()?.block_on(async {
                Ok(RepositoryLoader::new(
                    &root,
                    url::Url::from_directory_path(mirror.join("metadata")).unwrap(),
                    url::Url::from_directory_path(mirror.join("targets")).unwrap(),
                )
                .limits(super::super::TUF_LIMITS)
                .datastore(datastore.path())
                .load()
                .await?)
            })
        }

        /// Copy a signed mirror and overlay refreshed metadata.
        fn overlay(&self, from: &str, refreshed: &str, to: &str) -> Result<()> {
            let to = self.path(to);
            fs::create_dir(&to)?;
            for directory in ["metadata", "targets"] {
                fs::create_dir(to.join(directory))?;
                for entry in fs::read_dir(self.path(from).join(directory))? {
                    let entry = entry?;
                    fs::copy(entry.path(), to.join(directory).join(entry.file_name()))?;
                }
            }
            for entry in fs::read_dir(self.path(refreshed))? {
                let entry = entry?;
                fs::copy(entry.path(), to.join("metadata").join(entry.file_name()))?;
            }
            Ok(())
        }
    }

    #[test]
    fn signing_enforces_root_thresholds_and_objects_delegations() -> Result<()> {
        let fixture = fixture()?;
        let targets = &fixture.keys["targets"];
        let error = fixture.sign("one", &targets[..1], &[], 1).unwrap_err();
        assert!(format!("{error:#}").contains("requires 2 targets signatures"));
        let error = fixture
            .sign("twice", &[targets[0].clone(), targets[0].clone()], &[], 1)
            .unwrap_err();
        assert!(format!("{error:#}").contains("listed twice"));
        let wrong = [targets[0].clone(), fixture.keys["snapshot"][0].clone()];
        assert!(fixture.sign("wrong", &wrong, &[], 1).is_err());

        // Default: every shard is delegated to the first targets key, threshold 1.
        fixture.sign("default", targets, &[], 1)?;
        let repository = fixture.load("default")?;
        let delegations = repository.targets().signed.delegations.as_ref().unwrap();
        let first = hex::encode(parse_keypair(&fs::read(&targets[0])?)?.tuf_key().key_id()?);
        for role in &delegations.roles {
            assert_eq!(role.threshold.get(), 1);
            assert_eq!(
                role.keyids.iter().map(hex::encode).collect::<Vec<_>>(),
                [first.as_str()]
            );
        }
        assert_eq!(repository.targets().signatures.len(), 2);

        // Dedicated objects keys with a 2-of-3 threshold.
        let objects: Vec<_> = (0..3)
            .map(|index| {
                let path = fixture.path(&format!("objects-{index}.der"));
                generate(&path).map(|_| path)
            })
            .collect::<Result<_>>()?;
        assert!(fixture.sign("too-high", targets, &objects, 4).is_err());
        fixture.sign("objects", targets, &objects, 2)?;
        let repository = fixture.load("objects")?;
        let delegations = repository.targets().signed.delegations.as_ref().unwrap();
        assert_eq!(delegations.roles.len(), 16);
        for role in &delegations.roles {
            assert_eq!(role.threshold.get(), 2);
            assert_eq!(role.keyids.len(), 3);
        }
        assert!(
            repository
                .targets()
                .signed
                .find_target(&tough::TargetName::new("manifests-abc")?, false)
                .is_ok()
        );
        Ok(())
    }

    fn write_helper(fixture: &Fixture, name: &str, key: &Path, mode: &str) -> Result<PathBuf> {
        // The helper signs with openssl as an HSM or KMS front end would.
        let script = fixture.path(&format!("{name}.sh"));
        let body = match mode {
            "sign" => format!(
                "#!/bin/sh\nset -eu\n[ \"$CPS_SIGNER_PROTOCOL\" = 1 ] && [ \"$CPS_SIGNER_CONTEXT\" = tuf ]\nmessage=$(mktemp)\ntrap 'rm -f \"$message\"' EXIT\ncat > \"$message\"\nopenssl pkeyutl -sign -rawin -inkey {} -keyform DER -in \"$message\"\n",
                key.display()
            ),
            "garbage" => "#!/bin/sh\ncat >/dev/null\nhead -c 64 /dev/zero\n".to_owned(),
            "fail" => "#!/bin/sh\ncat >/dev/null\necho refusing >&2\nexit 3\n".to_owned(),
            "hang" => "#!/bin/sh\nexec sleep 30\n".to_owned(),
            _ => unreachable!(),
        };
        fs::write(&script, body)?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        let public = fixture.path(&format!("{name}.pub"));
        let tuf_key = parse_keypair(&fs::read(key)?)?.tuf_key();
        let Key::Ed25519 { keyval, .. } = tuf_key else {
            unreachable!()
        };
        fs::write(&public, hex::encode(keyval.public.as_ref()))?;
        let descriptor = fixture.path(&format!("{name}.signer.kdl"));
        fs::write(
            &descriptor,
            format!(
                "schema-version 1 kind=\"signer\"\ncommand \"{}\"\npublic-key \"{name}.pub\"\ntimeout-seconds 2\n",
                script.display()
            ),
        )?;
        Ok(PathBuf::from(format!("command:{}", descriptor.display())))
    }

    #[test]
    fn timestamp_refresh_needs_only_the_timestamp_key() -> Result<()> {
        if std::process::Command::new("openssl")
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("skipping: openssl is unavailable");
            return Ok(());
        }
        let fixture = fixture()?;
        let targets = &fixture.keys["targets"];
        // One targets key comes from an external signer.
        let external = write_helper(&fixture, "targets", &targets[1], "sign")?;
        fixture.sign("signed", &[targets[0].clone(), external], &[], 1)?;
        fixture.load("signed")?;
        let snapshot = fixture.path("signed/metadata/1.snapshot.json");
        let timestamp_key = &fixture.keys["timestamp"];
        let refresh = |output: &str, keys: &[PathBuf], snapshot_keys: &[PathBuf], version| {
            refresh_metadata(RefreshMetadata {
                root: &fixture.root,
                snapshot: &snapshot,
                output: &fixture.path(output),
                timestamp_keys: keys,
                snapshot_keys,
                version: NonZeroU64::new(version).unwrap(),
                snapshot_expires: (!snapshot_keys.is_empty()).then(|| in_hours(72)).as_deref(),
                timestamp_expires: &in_hours(12),
            })
        };
        assert!(refresh("same", timestamp_key, &[], 1).is_err());
        assert!(refresh("wrong", &targets[..1], &[], 2).is_err());
        let hang = write_helper(&fixture, "hang", &timestamp_key[0], "hang")?;
        let started = std::time::Instant::now();
        let error = refresh("hang", &[hang], &[], 2).unwrap_err();
        assert!(format!("{error:#}").contains("did not finish within 2 seconds"));
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        for mode in ["garbage", "fail"] {
            let helper = write_helper(&fixture, mode, &timestamp_key[0], mode)?;
            let error = refresh(mode, &[helper], &[], 2).unwrap_err();
            assert!(!fixture.path(mode).exists());
            let message = format!("{error:#}");
            assert!(
                message.contains("does not verify") || message.contains("exit status: 3"),
                "{message}"
            );
        }

        let helper = write_helper(&fixture, "timestamp", &timestamp_key[0], "sign")?;
        refresh("refreshed", &[helper], &[], 2)?;
        assert_eq!(
            fs::read_dir(fixture.path("refreshed"))?.count(),
            1,
            "a timestamp-only refresh writes only timestamp.json"
        );
        fixture.overlay("signed", "refreshed", "mirror-2")?;
        let repository = fixture.load("mirror-2")?;
        assert_eq!(repository.timestamp().signed.version.get(), 2);
        assert_eq!(repository.snapshot().signed.version.get(), 1);

        refresh("resigned", timestamp_key, &fixture.keys["snapshot"], 3)?;
        assert!(fixture.path("resigned/3.snapshot.json").is_file());
        fixture.overlay("mirror-2", "resigned", "mirror-3")?;
        let repository = fixture.load("mirror-3")?;
        assert_eq!(repository.snapshot().signed.version.get(), 3);
        assert_eq!(repository.targets().signed.version.get(), 1);

        // A snapshot not signed by the root's snapshot keys is rejected.
        let mut forged: serde_json::Value = serde_json::from_slice(&fs::read(&snapshot)?)?;
        forged["signed"]["version"] = 7.into();
        let forged_path = fixture.path("forged.json");
        fs::write(&forged_path, serde_json::to_vec(&forged)?)?;
        let error = refresh_metadata(RefreshMetadata {
            root: &fixture.root,
            snapshot: &forged_path,
            output: &fixture.path("forged"),
            timestamp_keys: timestamp_key,
            snapshot_keys: &[],
            version: NonZeroU64::new(9).unwrap(),
            snapshot_expires: None,
            timestamp_expires: &in_hours(12),
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("not signed by the root's snapshot keys"));
        Ok(())
    }

    #[test]
    fn unsupported_key_sources_fail_before_signing() {
        assert!(matches!(
            KeySpec::parse(Path::new("command:/etc/cps/t.kdl")),
            KeySpec::Command(path) if path == Path::new("/etc/cps/t.kdl")
        ));
        assert!(matches!(
            KeySpec::parse(Path::new("file:command:odd")),
            KeySpec::File(path) if path == Path::new("command:odd")
        ));
        assert!(matches!(
            KeySpec::parse(Path::new("keys/targets.pem")),
            KeySpec::File(path) if path == Path::new("keys/targets.pem")
        ));
    }
}
