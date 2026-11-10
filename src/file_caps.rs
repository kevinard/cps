use std::{
    fmt,
    fs::{self, File},
};

use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{XattrFlags, fsetxattr, getxattr},
    io::Errno,
};
use serde::{Deserialize, Serialize};

use crate::image::root::Root;

pub(crate) const ATTRIBUTE: &str = "security.capability";
const REVISION_1: u32 = 0x0100_0000;
const REVISION_2: u32 = 0x0200_0000;
const REVISION_3: u32 = 0x0300_0000;
const REVISION_MASK: u32 = 0xff00_0000;
const EFFECTIVE: u32 = 1;

// In Linux UAPI bit order. Keep numeric IDs out of the package syntax.
const NAMES: &[&str] = &[
    "cap_chown",
    "cap_dac_override",
    "cap_dac_read_search",
    "cap_fowner",
    "cap_fsetid",
    "cap_kill",
    "cap_setgid",
    "cap_setuid",
    "cap_setpcap",
    "cap_linux_immutable",
    "cap_net_bind_service",
    "cap_net_broadcast",
    "cap_net_admin",
    "cap_net_raw",
    "cap_ipc_lock",
    "cap_ipc_owner",
    "cap_sys_module",
    "cap_sys_rawio",
    "cap_sys_chroot",
    "cap_sys_ptrace",
    "cap_sys_pacct",
    "cap_sys_admin",
    "cap_sys_boot",
    "cap_sys_nice",
    "cap_sys_resource",
    "cap_sys_time",
    "cap_sys_tty_config",
    "cap_mknod",
    "cap_lease",
    "cap_audit_write",
    "cap_audit_control",
    "cap_setfcap",
    "cap_mac_override",
    "cap_mac_admin",
    "cap_syslog",
    "cap_wake_alarm",
    "cap_block_suspend",
    "cap_audit_read",
    "cap_perfmon",
    "cap_bpf",
    "cap_checkpoint_restore",
];

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileCapabilities {
    permitted: u64,
    inheritable: u64,
    effective: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace_root: Option<u32>,
}

impl FileCapabilities {
    pub(crate) fn parse(text: &str) -> Result<Self> {
        const INHERITABLE: u32 = 1 << 1;
        const PERMITTED: u32 = 1 << 2;
        if text == "=" {
            return Ok(Self::default());
        }
        let mut caps = Self::default();
        let mut effective = None;
        for clause in text.split_ascii_whitespace() {
            let (names, flags) = clause
                .split_once('=')
                .context("expected capability names followed by = and flags")?;
            let mut seen = 0;
            for flag in flags.bytes() {
                let bit = match flag {
                    b'e' => EFFECTIVE,
                    b'i' => INHERITABLE,
                    b'p' => PERMITTED,
                    _ => bail!("invalid file capability flag in {flags:?}"),
                };
                ensure!(
                    seen & bit == 0,
                    "duplicate file capability flag in {flags:?}"
                );
                seen |= bit;
            }
            ensure!(
                seen & (PERMITTED | INHERITABLE) != 0,
                "file capability clauses require p or i"
            );
            let enable = seen & EFFECTIVE != 0;
            ensure!(
                effective.is_none_or(|previous| previous == enable),
                "the effective file capability flag applies to every clause"
            );
            effective = Some(enable);
            for name in names.split(',') {
                let index = NAMES
                    .iter()
                    .position(|known| *known == name)
                    .with_context(|| format!("unknown Linux file capability {name:?}"))?;
                let bit = 1 << index;
                ensure!(
                    (caps.permitted | caps.inheritable) & bit == 0,
                    "duplicate file capability {name}"
                );
                if seen & PERMITTED != 0 {
                    caps.permitted |= bit;
                }
                if seen & INHERITABLE != 0 {
                    caps.inheritable |= bit;
                }
            }
        }
        caps.effective = effective.context("expected file capabilities or = for an empty set")?;
        Ok(caps)
    }

    pub(crate) fn validate_declaration(&self) -> Result<()> {
        let bits = self.permitted | self.inheritable;
        ensure!(
            self.namespace_root.is_none(),
            "namespaced file capabilities cannot be declared"
        );
        ensure!(
            bits >> NAMES.len() == 0,
            "unknown Linux file capability bits"
        );
        ensure!(
            bits != 0 || !self.effective,
            "effective file capabilities require a nonempty set"
        );
        Ok(())
    }

    pub(crate) fn read(file: &File) -> Result<Option<Self>> {
        let mut bytes = [0u8; 24];
        let size = match getxattr(Root::fd_path(file), ATTRIBUTE, &mut bytes[..]) {
            Ok(size) => size,
            Err(Errno::NODATA | Errno::NOTSUP) => return Ok(None),
            Err(error) => return Err(error).context("reading security.capability"),
        };
        require_unmapped_ids()?;
        Self::decode(&bytes[..size]).map(Some)
    }

    pub(crate) fn install(&self, file: &File) -> Result<()> {
        self.validate_declaration()?;
        require_unmapped_ids()?;
        fsetxattr(file, ATTRIBUTE, &self.encode(), XattrFlags::empty())
            .context("setting security.capability (requires CAP_SETFCAP and filesystem support)")?;
        ensure!(
            Self::read(file)? == Some(*self),
            "filesystem did not retain the declared file capabilities"
        );
        Ok(())
    }

    fn encode(&self) -> [u8; 20] {
        let words = [
            REVISION_2 | u32::from(self.effective),
            self.permitted as u32,
            self.inheritable as u32,
            (self.permitted >> 32) as u32,
            (self.inheritable >> 32) as u32,
        ];
        let mut bytes = [0; 20];
        for (word, output) in words.into_iter().zip(bytes.as_chunks_mut::<4>().0) {
            *output = word.to_le_bytes();
        }
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut words = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .copied()
            .map(u32::from_le_bytes);
        let magic = words.next().context("missing security.capability header")?;
        let revision = magic & REVISION_MASK;
        ensure!(
            matches!(
                (revision, bytes.len()),
                (REVISION_1, 12) | (REVISION_2, 20) | (REVISION_3, 24)
            ) && magic & !(REVISION_MASK | EFFECTIVE) == 0,
            "unsupported security.capability encoding"
        );
        let mut caps = Self {
            permitted: u64::from(words.next().expect("validated length")),
            inheritable: u64::from(words.next().expect("validated length")),
            effective: magic & EFFECTIVE != 0,
            namespace_root: None,
        };
        if revision != REVISION_1 {
            caps.permitted |= u64::from(words.next().expect("validated length")) << 32;
            caps.inheritable |= u64::from(words.next().expect("validated length")) << 32;
        }
        if revision == REVISION_3 {
            caps.namespace_root = words.next();
        }
        Ok(caps)
    }
}

impl fmt::Display for FileCapabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first_clause = true;
        for (set, flags) in [
            (self.permitted & !self.inheritable, "p"),
            (self.inheritable & !self.permitted, "i"),
            (self.permitted & self.inheritable, "ip"),
        ] {
            if set == 0 {
                continue;
            }
            if !first_clause {
                f.write_str(" ")?;
            }
            first_clause = false;
            let mut first_name = true;
            for (index, name) in NAMES.iter().enumerate() {
                if set & (1 << index) != 0 {
                    if !first_name {
                        f.write_str(",")?;
                    }
                    first_name = false;
                    f.write_str(name)?;
                }
            }
            f.write_str(if self.effective { "=e" } else { "=" })?;
            f.write_str(flags)?;
        }
        if first_clause {
            f.write_str("=")?;
        }
        Ok(())
    }
}

fn require_unmapped_ids() -> Result<()> {
    // The kernel can translate v3 xattrs to v2 when observed inside the
    // namespace that created them. A boot image needs host-scope metadata.
    let map = match fs::read_to_string("/proc/self/uid_map") {
        Ok(map) => map,
        // A kernel without user namespaces has no uid_map: every process is
        // in the initial namespace.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && std::path::Path::new("/proc/self/status").exists() =>
        {
            return Ok(());
        }
        Err(error) => return Err(error).context("checking file capability UID namespace"),
    };
    ensure!(
        map.split_ascii_whitespace().eq(["0", "0", "4294967295"]),
        "file capabilities require an unmapped UID namespace"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_capabilities_round_trip_through_linux_encoding_and_text() -> Result<()> {
        let raw = FileCapabilities::parse("cap_net_raw=ep")?;
        assert_eq!(
            raw.encode(),
            [1, 0, 0, 2, 0, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,]
        );
        let combined = FileCapabilities::parse(
            "cap_net_raw,cap_checkpoint_restore=pe cap_bpf=ie cap_chown=eip",
        )?;
        assert_eq!(FileCapabilities::decode(&combined.encode())?, combined);
        assert_eq!(FileCapabilities::parse(&combined.to_string())?, combined);
        assert_eq!(FileCapabilities::parse("=")?.to_string(), "=");

        let mut namespaced = raw.encode().to_vec();
        namespaced[3] = 3;
        namespaced.extend_from_slice(&1000_u32.to_le_bytes());
        let namespaced = FileCapabilities::decode(&namespaced)?;
        assert_ne!(namespaced, raw);
        assert!(namespaced.validate_declaration().is_err());
        for text in [
            "",
            "cap_net_raw=e",
            "cap_net_raw=pee",
            "cap_raw=ep",
            "cap_net_raw=p cap_net_raw=i",
            "cap_net_raw=ep cap_chown=p",
        ] {
            assert!(FileCapabilities::parse(text).is_err(), "{text}");
        }
        Ok(())
    }
}
