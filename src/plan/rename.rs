use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};

use super::freeze::{FrozenPackage, reason_suffix};
use crate::{
    image::Installed,
    model::{PackageSpec, Version},
    repo::ReleaseProfile,
};

#[derive(Default)]
pub(crate) struct Renames(BTreeMap<String, Successor>);

struct Successor {
    publisher: String,
    name: String,
}

impl Renames {
    pub(crate) fn add(
        &mut self,
        before: &BTreeMap<String, Installed>,
        publisher: &str,
        targets: &BTreeMap<String, String>,
        obsoletions: &BTreeMap<String, Version>,
    ) {
        for (old, to) in targets {
            if before.get(old).is_some_and(|package| {
                let manifest = &package.published.manifest;
                manifest.publisher == publisher
                    && manifest.version.semantic() <= obsoletions[old].semantic()
            }) {
                self.0.insert(
                    old.clone(),
                    Successor {
                        publisher: publisher.to_owned(),
                        name: to.clone(),
                    },
                );
            }
        }
    }

    pub(super) fn check_request(&self, updating: bool) -> Result<()> {
        if let Some((old, to)) = self.0.first_key_value() {
            ensure!(
                updating,
                "{old} was renamed to {}; upgrade the image before making other catalog-based changes",
                to.name
            );
        }
        Ok(())
    }

    pub(crate) fn reserve_sources<'a>(
        &'a self,
        sources: &mut BTreeMap<String, &'a str>,
    ) -> Result<()> {
        for (old, to) in &self.0 {
            ensure!(
                super::claim_source(sources, &to.publisher, &to.name),
                "cannot rename {old} to cps://{}/{} because that name is reserved for a different publisher",
                to.publisher,
                to.name
            );
        }
        Ok(())
    }

    pub(super) fn check_freezes(&self, freezes: &BTreeMap<String, FrozenPackage>) -> Result<()> {
        for (old, to) in &self.0 {
            if let Some(frozen) = freezes.get(old) {
                anyhow::bail!(
                    "cannot rename {old} to {} because {old} is frozen at {}{}",
                    to.name,
                    frozen.version,
                    reason_suffix(frozen)
                );
            }
        }
        Ok(())
    }

    pub(crate) fn check_profile(
        &self,
        protected: impl Fn(&str) -> bool,
        next: Option<&ReleaseProfile>,
    ) -> Result<()> {
        for (old, to) in &self.0 {
            ensure!(
                !protected(old)
                    || next.is_some_and(|profile| profile.composition.contains_key(&to.name)),
                "{old} is protected by the installed signed profile; the signed profile update must include its successor {}",
                to.name
            );
        }
        Ok(())
    }

    pub(crate) fn requirements(&self) -> BTreeMap<&str, PackageSpec<'_>> {
        self.0
            .values()
            .map(|to| {
                (
                    to.name.as_str(),
                    PackageSpec {
                        name: &to.name,
                        publisher: Some(&to.publisher),
                        version: None,
                    },
                )
            })
            .collect()
    }

    pub(super) fn transfer_local(&self, local: &mut BTreeSet<String>) {
        for (old, to) in &self.0 {
            if local.remove(old) {
                local.insert(to.name.clone());
            }
        }
    }

    pub(super) fn transfer_explicit(
        &self,
        before: &BTreeMap<String, Installed>,
        after: &mut BTreeMap<String, Installed>,
    ) {
        let targets = self.0.values().map(|to| &to.name).collect::<BTreeSet<_>>();
        for name in targets {
            after
                .get_mut(name)
                .expect("required rename successor")
                .explicit = before.get(name).is_some_and(|package| package.explicit);
        }
        for (old, to) in &self.0 {
            if before[old].explicit {
                after
                    .get_mut(&to.name)
                    .expect("required rename successor")
                    .explicit = true;
            }
        }
    }
}
