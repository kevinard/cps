use std::{collections::BTreeSet, num::NonZeroU64};

use anyhow::{Context, Result, ensure};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

use crate::model::validate_digest;

const MAX_ROTATIONS: usize = 64;
type PublicKey = [u8; 32];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Rotation {
    transition: Transition,
    previous_signature: String,
    next_signature: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Transition {
    publisher: String,
    base: String,
    first_sequence: NonZeroU64,
    previous: PublicKey,
    next: PublicKey,
}

impl Transition {
    fn message(&self) -> Result<Vec<u8>> {
        let mut message = b"cps publisher key rotation v1\n".to_vec();
        serde_json::to_writer(&mut message, self)?;
        Ok(message)
    }
}

impl Rotation {
    pub(super) fn new(
        publisher: &str,
        base: &str,
        first_sequence: NonZeroU64,
        previous: &Ed25519KeyPair,
        next: &Ed25519KeyPair,
    ) -> Result<Self> {
        let transition = Transition {
            publisher: publisher.to_owned(),
            base: base.to_owned(),
            first_sequence,
            previous: public_key(previous.public_key().as_ref())?,
            next: public_key(next.public_key().as_ref())?,
        };
        let message = transition.message()?;
        Ok(Self {
            transition,
            previous_signature: hex::encode(previous.sign(&message).as_ref()),
            next_signature: hex::encode(next.sign(&message).as_ref()),
        })
    }

    pub(super) fn previous_key(&self) -> &PublicKey {
        &self.transition.previous
    }

    pub(super) fn next_key(&self) -> &PublicKey {
        &self.transition.next
    }
}

pub(super) enum Trust<'a> {
    Anchor(&'a [u8]),
    Current(&'a [u8]),
}

fn public_key(bytes: &[u8]) -> Result<PublicKey> {
    bytes
        .try_into()
        .context("invalid Ed25519 public key length")
}

pub(super) fn signer(
    publisher: &str,
    sequence: u64,
    rotations: &[Rotation],
    trust: Trust<'_>,
) -> Result<PublicKey> {
    ensure!(
        rotations.len() <= MAX_ROTATIONS,
        "publisher key rotation limit exceeded"
    );
    let expected = public_key(match trust {
        Trust::Anchor(key) | Trust::Current(key) => key,
    })?;
    let Some(first) = rotations.first() else {
        return Ok(expected);
    };
    let mut signer = *first.previous_key();
    match trust {
        Trust::Anchor(_) => ensure!(
            signer == expected,
            "publisher key history has a different trust anchor"
        ),
        Trust::Current(_) => ensure!(
            rotations
                .last()
                .is_some_and(|rotation| rotation.transition.next == expected),
            "signing key is not the current publisher key"
        ),
    }
    let mut seen = BTreeSet::from([signer]);
    let mut previous_sequence = 0;
    for rotation in rotations {
        let transition = &rotation.transition;
        validate_digest(&transition.base)?;
        ensure!(
            transition.publisher == publisher && transition.previous == signer,
            "invalid publisher key rotation chain"
        );
        let first = transition.first_sequence.get();
        ensure!(
            first > previous_sequence && first <= sequence,
            "invalid publisher key rotation sequence"
        );
        ensure!(
            seen.insert(transition.next),
            "publisher key was already used"
        );
        let message = transition.message()?;
        for (key, encoded) in [
            (signer, &rotation.previous_signature),
            (transition.next, &rotation.next_signature),
        ] {
            let mut signature = [0u8; 64];
            hex::decode_to_slice(encoded, &mut signature)?;
            UnparsedPublicKey::new(&ED25519, key)
                .verify(&message, &signature)
                .map_err(|_| anyhow::anyhow!("invalid publisher key rotation signature"))?;
        }
        signer = transition.next;
        previous_sequence = first;
    }
    Ok(signer)
}

pub(super) fn check_successor(
    previous: &[Rotation],
    sequence: u64,
    previous_hash: &str,
    next: &[Rotation],
) -> Result<()> {
    ensure!(
        next.starts_with(previous),
        "release drops or changes the publisher key history"
    );
    if let Some(rotation) = next.get(previous.len()) {
        ensure!(
            rotation.transition.first_sequence.get() > sequence,
            "release backdates a publisher key rotation"
        );
        if sequence.checked_add(1) == Some(rotation.transition.first_sequence.get()) {
            ensure!(
                rotation.transition.base == previous_hash,
                "publisher key rotation does not match the authenticated base catalog"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;

    fn key() -> Ed25519KeyPair {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).expect("generate key");
        Ed25519KeyPair::from_pkcs8(document.as_ref()).expect("read generated key")
    }

    #[test]
    fn rotations_require_both_signatures_the_right_publisher_and_a_new_sequence() -> Result<()> {
        let old = key();
        let new = key();
        let first = NonZeroU64::new(2).unwrap();
        let anchor = old.public_key().as_ref();
        let base = crate::model::digest(b"base catalog");
        let rotation = Rotation::new("canopa", &base, first, &old, &new)?;
        let chain = [rotation.clone()];
        assert_eq!(
            signer("canopa", first.get(), &chain, Trust::Anchor(anchor))?,
            new.public_key().as_ref()
        );
        assert!(signer("other", first.get(), &chain, Trust::Anchor(anchor)).is_err());
        assert!(signer("canopa", 1, &chain, Trust::Anchor(anchor)).is_err());
        assert!(
            signer(
                "canopa",
                first.get(),
                &chain,
                Trust::Anchor(new.public_key().as_ref())
            )
            .is_err()
        );
        assert!(check_successor(&[], first.get(), &base, &chain).is_err());
        check_successor(&[], 1, &base, &chain)?;
        let fork = crate::model::digest(b"different catalog at the same sequence");
        assert!(check_successor(&[], 1, &fork, &chain).is_err());
        Ok(())
    }
}
