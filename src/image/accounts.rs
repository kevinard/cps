//! File ownership by account name.
//!
//! Manifests and managed paths name each file's owner and group. Numeric IDs
//! exist only on disk, so each image root translates between the two:
//!
//! - A root-owned image resolves names against its own `etc/passwd` and
//!   `etc/group`, so the same package gets the IDs that each image's
//!   sysusers.d allocated. `root` is always ID 0, even before the image has
//!   an account database.
//! - An image that an unprivileged user created cannot hand files to other
//!   accounts. Every name maps to that user, as a user namespace would map
//!   them, and the user's own IDs read back as `root`.
//!
//! An ID with no name reads back as its decimal form, which no manifest can
//! write, so such a file never matches a manifest's owner. When names share
//! an ID, the ID reads back as the first of them in the database, except that
//! 0 is always `root`.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fs::File,
    io::Read,
};

use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{AtFlags, Mode, OFlags, ResolveFlags, openat2, statat},
    io::Errno,
};

/// The account that owns system files, with ID 0 in every image.
pub(crate) const ROOT: &str = "root";

// Account database files larger than this are rejected rather than read.
const DATABASE_LIMIT: u64 = 16 << 20;

/// Check that a manifest names an account the way sysusers.d can create it.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    let body = name.strip_suffix('$').unwrap_or(name);
    let mut chars = body.chars();
    ensure!(
        body.len() <= 31
            && chars
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "invalid account name {name:?}"
    );
    Ok(())
}

/// The name to ID mapping of one image root, shared by the roots opened
/// beneath it. The database is read when first needed and read again
/// whenever either file changes, for example after systemd-sysusers runs.
pub(crate) struct Accounts {
    base: File,
    creator: Cell<Option<(u32, u32)>>,
    database: RefCell<Option<Database>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Stamp {
    ino: u64,
    size: i64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

struct Database {
    stamps: [Option<Stamp>; 2],
    users: Table,
    groups: Table,
}

#[derive(Default)]
struct Table {
    ids: HashMap<String, u32>,
    names: HashMap<u32, String>,
}

const FILES: [(&str, &str); 2] = [("etc/passwd", "user"), ("etc/group", "group")];

impl Accounts {
    pub(crate) fn new(base: File) -> Self {
        Self {
            base,
            creator: Cell::new(None),
            database: RefCell::new(None),
        }
    }

    /// Map every account to the unprivileged user that created the image.
    pub(crate) fn map_to_creator(&self, uid: u32, gid: u32) {
        self.creator.set(Some((uid, gid)));
    }

    /// The numeric IDs of an owner and group name.
    pub(crate) fn ids(&self, owner: &str, group: &str) -> Result<(u32, u32)> {
        if let Some(creator) = self.creator.get() {
            return Ok(creator);
        }
        let database = self.database()?;
        let database = database.as_ref().expect("loaded account database");
        Ok((
            database.users.id(owner, FILES[0])?,
            database.groups.id(group, FILES[1])?,
        ))
    }

    /// The owner and group names of numeric IDs.
    pub(crate) fn names(&self, uid: u32, gid: u32) -> Result<(String, String)> {
        if let Some((owner, group)) = self.creator.get() {
            let name = |id: u32, expected: u32| {
                if id == expected {
                    ROOT.to_owned()
                } else {
                    id.to_string()
                }
            };
            return Ok((name(uid, owner), name(gid, group)));
        }
        let database = self.database()?;
        let database = database.as_ref().expect("loaded account database");
        Ok((database.users.name(uid), database.groups.name(gid)))
    }

    fn database(&self) -> Result<std::cell::Ref<'_, Option<Database>>> {
        let stamps = [self.stamp(FILES[0].0)?, self.stamp(FILES[1].0)?];
        if self
            .database
            .borrow()
            .as_ref()
            .is_none_or(|database| database.stamps != stamps)
        {
            let database = Database {
                stamps,
                users: self.table(FILES[0].0, 2)?,
                groups: self.table(FILES[1].0, 2)?,
            };
            *self.database.borrow_mut() = Some(database);
        }
        Ok(self.database.borrow())
    }

    #[allow(clippy::unnecessary_cast)]
    fn stamp(&self, path: &str) -> Result<Option<Stamp>> {
        match statat(&self.base, path, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(info) => Ok(Some(Stamp {
                ino: info.st_ino as u64,
                size: info.st_size as i64,
                mtime: (info.st_mtime as i64, info.st_mtime_nsec as i64),
                ctime: (info.st_ctime as i64, info.st_ctime_nsec as i64),
            })),
            Err(Errno::NOENT | Errno::NOTDIR) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("checking /{path}")),
        }
    }

    /// Read the names and IDs of one database. `field` is the position of
    /// the ID in each colon-separated record.
    fn table(&self, path: &str, field: usize) -> Result<Table> {
        let file = match openat2(
            &self.base,
            path,
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        ) {
            Ok(file) => File::from(file),
            Err(Errno::NOENT | Errno::NOTDIR) => return Ok(Table::default()),
            Err(error) => return Err(error).with_context(|| format!("opening /{path}")),
        };
        let mut text = String::new();
        file.take(DATABASE_LIMIT + 1)
            .read_to_string(&mut text)
            .with_context(|| format!("reading /{path}"))?;
        ensure!(
            text.len() as u64 <= DATABASE_LIMIT,
            "/{path} is larger than 16 MiB"
        );
        let mut table = Table::default();
        for line in text.lines() {
            // NSS compat entries such as `+` or `-name` name no local account.
            if line.is_empty() || line.starts_with(['#', '+', '-']) {
                continue;
            }
            let fields: Vec<&str> = line.split(':').collect();
            let (Some(name), Some(id)) = (fields.first(), fields.get(field)) else {
                bail!("invalid record in /{path}: {line:?}");
            };
            let id: u32 = id
                .parse()
                .with_context(|| format!("invalid ID in /{path}: {line:?}"))?;
            table.ids.entry((*name).to_owned()).or_insert(id);
            table.names.entry(id).or_insert_with(|| (*name).to_owned());
        }
        Ok(table)
    }
}

impl Table {
    fn id(&self, name: &str, (path, kind): (&str, &str)) -> Result<u32> {
        if name == ROOT {
            return Ok(0);
        }
        // An unnamed ID read back from disk keeps its number.
        if let Ok(id) = name.parse() {
            return Ok(id);
        }
        self.ids.get(name).copied().with_context(|| {
            format!("no {kind} {name} in /{path}; a package's sysusers.d must create it")
        })
    }

    fn name(&self, id: u32) -> String {
        if id == 0 {
            return ROOT.to_owned();
        }
        self.names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn names_resolve_against_the_image_database() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let accounts = Accounts::new(File::open(temp.path())?);
        assert_eq!(accounts.ids(ROOT, ROOT)?, (0, 0));
        assert_eq!(accounts.names(0, 7)?, (ROOT.to_owned(), "7".to_owned()));
        let error = accounts.ids("messagebus", ROOT).unwrap_err();
        assert!(format!("{error:#}").contains("no user messagebus in /etc/passwd"));

        fs::create_dir(temp.path().join("etc"))?;
        fs::write(
            temp.path().join("etc/passwd"),
            "root:x:0:0::/root:/bin/sh\nmessagebus:x:81:81::/:/usr/bin/nologin\nalias:x:81:81::/:/bin/sh\n",
        )?;
        fs::write(temp.path().join("etc/group"), "root:x:0:\ntty:x:5:\n")?;
        assert_eq!(accounts.ids("messagebus", "tty")?, (81, 5));
        assert_eq!(accounts.ids("alias", "5")?, (81, 5));
        assert_eq!(
            accounts.names(81, 5)?,
            ("messagebus".to_owned(), "tty".to_owned())
        );

        // A changed database is read again.
        fs::write(temp.path().join("etc/group"), "root:x:0:\ntty:x:6:\n")?;
        assert_eq!(accounts.ids(ROOT, "tty")?, (0, 6));
        Ok(())
    }

    #[test]
    fn rootless_images_map_every_name_to_their_creator() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let accounts = Accounts::new(File::open(temp.path())?);
        accounts.map_to_creator(1000, 100);
        assert_eq!(accounts.ids("messagebus", "tty")?, (1000, 100));
        assert_eq!(
            accounts.names(1000, 100)?,
            (ROOT.to_owned(), ROOT.to_owned())
        );
        assert_eq!(accounts.names(0, 5)?, ("0".to_owned(), "5".to_owned()));
        Ok(())
    }

    #[test]
    fn manifests_name_accounts_sysusers_can_create() {
        for name in ["root", "systemd-network", "_apt", "machine$", "a1"] {
            assert!(validate_name(name).is_ok(), "{name}");
        }
        for name in ["", "0", "1000", "-x", "a b", "a:b", "x$y", &"a".repeat(32)] {
            assert!(validate_name(name).is_err(), "{name}");
        }
    }
}
