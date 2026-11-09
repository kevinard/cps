//! External signer protocol, version 1.
//!
//! CPS executes the descriptor's program directly with its fixed arguments. No
//! shell is involved and no CPS data is interpolated into the command line.
//! The message to sign is written to stdin, followed by EOF. The helper writes
//! the raw signature bytes to stdout and exits with status 0. Any other exit
//! status, a signal, or a timeout fails the signing operation. Stderr passes
//! through to the operator. CPS verifies every returned signature against the
//! declared public key before using it.
use std::{
    fmt,
    io::{Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use ring::signature::{
    ECDSA_P256_SHA256_ASN1, ED25519, RSA_PSS_2048_8192_SHA256, UnparsedPublicKey,
    VerificationAlgorithm,
};
use rustix::process::{Pid, Signal, kill_process_group};
use tough::schema::key::Key;

use crate::kdl_format::signer::{SignerDescriptor, read_signer};
use crate::repo::{parse_public_key, read_limited};

pub(crate) const PROTOCOL_VERSION: &str = "1";
/// Large enough for an RSA-8192 signature.
const SIGNATURE_LIMIT: usize = 2048;
const PUBLIC_KEY_LIMIT: u64 = 16 * 1024;

/// Where a signing key comes from, as given on the command line.
///
/// `command:DESCRIPTOR` selects an external signer. `file:PATH` or a bare path
/// selects a private key file that CPS reads directly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum KeySpec {
    File(PathBuf),
    Command(PathBuf),
}

impl KeySpec {
    pub(crate) fn parse(value: &Path) -> Self {
        use std::os::unix::ffi::OsStrExt;
        let bytes = value.as_os_str().as_bytes();
        let path =
            |prefix: &[u8]| PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[prefix.len()..]));
        if bytes.starts_with(b"command:") {
            Self::Command(path(b"command:"))
        } else if bytes.starts_with(b"file:") {
            Self::File(path(b"file:"))
        } else {
            Self::File(value.to_owned())
        }
    }
}

/// Facts about one signing request, passed to the helper in its environment.
pub(crate) struct SignRequest<'a> {
    /// `tuf` for mirror metadata or `catalog` for a CPS publisher signature.
    pub context: &'static str,
    pub key_id: &'a str,
    pub algorithm: &'static str,
}

pub(crate) struct CommandSigner {
    descriptor: PathBuf,
    config: SignerDescriptor,
}

impl fmt::Debug for CommandSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandSigner")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

impl CommandSigner {
    pub(crate) fn open(descriptor: &Path) -> Result<Self> {
        Ok(Self {
            descriptor: descriptor.to_owned(),
            config: read_signer(descriptor)?,
        })
    }

    /// The declared public key file, unparsed.
    pub(crate) fn public_key(&self) -> Result<Vec<u8>> {
        let path = &self.config.public_key;
        (|| read_limited(std::fs::File::open(path)?, PUBLIC_KEY_LIMIT))()
            .with_context(|| format!("reading signer public key {}", path.display()))
    }

    /// Run the helper once. The caller verifies the returned bytes.
    pub(crate) fn run(&self, request: &SignRequest<'_>, message: &[u8]) -> Result<Vec<u8>> {
        self.exchange(request, message).with_context(|| {
            format!(
                "external signer {} ({})",
                self.descriptor.display(),
                self.config.program.display()
            )
        })
    }

    fn exchange(&self, request: &SignRequest<'_>, message: &[u8]) -> Result<Vec<u8>> {
        let deadline = Instant::now() + self.config.timeout;
        let mut child = Command::new(&self.config.program)
            .args(&self.config.args)
            .env("CPS_SIGNER_PROTOCOL", PROTOCOL_VERSION)
            .env("CPS_SIGNER_CONTEXT", request.context)
            .env("CPS_SIGNER_KEY_ID", request.key_id)
            .env("CPS_SIGNER_ALGORITHM", request.algorithm)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // A separate group lets a timeout stop the helper and anything it started.
            .process_group(0)
            .spawn()
            .context("starting the helper")?;
        let group = Pid::from_raw(i32::try_from(child.id())?).context("invalid helper PID")?;
        let mut stdin = child.stdin.take().context("missing helper stdin")?;
        let mut stdout = child.stdout.take().context("missing helper stdout")?;
        let message = message.to_vec();
        // A helper that exits without reading its input closes the pipe. The
        // write then fails, and the exit status and signature checks decide.
        thread::spawn(move || {
            let _ = stdin.write_all(&message);
        });
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut output = Vec::new();
            let result = (&mut stdout)
                .take(SIGNATURE_LIMIT as u64 + 1)
                .read_to_end(&mut output)
                .map(|_| output);
            let _ = sender.send(result);
        });
        let stop = |child: &mut std::process::Child| {
            let _ = kill_process_group(group, Signal::KILL);
            let _ = child.wait();
        };
        let output = match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(output) => output.context("reading the signature")?,
            Err(_) => {
                stop(&mut child);
                bail!(
                    "the helper did not finish within {} seconds",
                    self.config.timeout.as_secs()
                );
            }
        };
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                stop(&mut child);
                bail!(
                    "the helper did not exit within {} seconds",
                    self.config.timeout.as_secs()
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        ensure!(status.success(), "the helper failed with {status}");
        ensure!(!output.is_empty(), "the helper returned no signature");
        ensure!(
            output.len() <= SIGNATURE_LIMIT,
            "the helper returned more than {SIGNATURE_LIMIT} bytes"
        );
        Ok(output)
    }
}

/// Parse a declared public key as a TUF key.
///
/// Accepts a PEM SubjectPublicKeyInfo (Ed25519, RSA, or ECDSA P-256), an
/// Ed25519 DER SubjectPublicKeyInfo as used for CPS trust keys, or the
/// 64-digit hexadecimal Ed25519 form used in TUF metadata.
pub(crate) fn tuf_public_key(bytes: &[u8]) -> Result<Key> {
    if let Ok(key) = parse_public_key(bytes) {
        return ed25519_tuf_key(key);
    }
    let text = std::str::from_utf8(bytes)
        .context("expected a PEM, DER, or hexadecimal public key")?
        .trim();
    if let Some(der) = pem_public_key(text)?
        && let Ok(key) = parse_public_key(&der)
    {
        return ed25519_tuf_key(key);
    }
    let mut key = Key::from_str(text).map_err(|error| anyhow::anyhow!("{error}"))?;
    // Key IDs hash the encoded text. Re-encode so a PEM file from another
    // tool matches the form that TUF tools write into root metadata.
    match &mut key {
        Key::Rsa { keyval, .. } => keyval.public = keyval.public.to_vec().into(),
        Key::Ed25519 { keyval, .. } => keyval.public = keyval.public.to_vec().into(),
        Key::Ecdsa { keyval, .. } | Key::EcdsaOld { keyval, .. } => {
            keyval.public = keyval.public.to_vec().into();
        }
    }
    Ok(key)
}

fn ed25519_tuf_key(key: &[u8]) -> Result<Key> {
    Key::from_str(&hex::encode(key)).map_err(|error| anyhow::anyhow!("{error}"))
}

/// Read an Ed25519 public key in DER, PEM, or hexadecimal form.
pub(crate) fn ed25519_public_key(bytes: &[u8]) -> Result<[u8; 32]> {
    match tuf_public_key(bytes)? {
        Key::Ed25519 { keyval, .. } => Ok(keyval.public.as_ref().try_into()?),
        _ => bail!("expected an Ed25519 public key"),
    }
}

fn pem_public_key(text: &str) -> Result<Option<Vec<u8>>> {
    let Some(body) = text
        .strip_prefix("-----BEGIN PUBLIC KEY-----")
        .and_then(|rest| rest.strip_suffix("-----END PUBLIC KEY-----"))
    else {
        return Ok(None);
    };
    let body: String = body.split_whitespace().collect();
    Ok(Some(
        base64::engine::general_purpose::STANDARD
            .decode(body)
            .context("invalid PEM public key")?,
    ))
}

/// The TUF signing scheme name for a key, passed to helpers.
pub(crate) fn tuf_algorithm(key: &Key) -> &'static str {
    match key {
        Key::Ed25519 { .. } => "ed25519",
        Key::Rsa { .. } => "rsassa-pss-sha256",
        Key::Ecdsa { .. } | Key::EcdsaOld { .. } => "ecdsa-sha2-nistp256",
    }
}

/// Check a signature exactly as a TUF client would.
pub(crate) fn verify_tuf_signature(key: &Key, message: &[u8], signature: &[u8]) -> Result<()> {
    let (algorithm, public): (&'static dyn VerificationAlgorithm, &[u8]) = match key {
        Key::Ed25519 { keyval, .. } => (&ED25519, keyval.public.as_ref()),
        Key::Rsa { keyval, .. } => (&RSA_PSS_2048_8192_SHA256, keyval.public.as_ref()),
        Key::Ecdsa { keyval, .. } | Key::EcdsaOld { keyval, .. } => {
            (&ECDSA_P256_SHA256_ASN1, keyval.public.as_ref())
        }
    };
    UnparsedPublicKey::new(algorithm, public)
        .verify(message, signature)
        .map_err(|_| anyhow::anyhow!("signature does not verify with the declared public key"))
}
