//! Package records, shared by the repository index (`PQREPO`) and the local
//! state (`PQREPOSTATE`):
//!
//! ```text
//! checksum;timestamp;folder;name;version;release;description;groups;set;status;depends
//! ```
//!
//! In the state, installed packages keep the fields of the installed
//! version. `depends` is a space-separated list of package names; a name may
//! carry a version constraint (`name>=1.2`), reserved for later and not
//! enforced yet.
//!
//! Status: `N` not installed, `I` installed explicitly, `A` installed
//! automatically (as a dependency); flags: `U` update available, `X` no
//! longer provided by the repository.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::util::{is_valid_name, is_valid_version};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub checksum: String,
    pub timestamp: String,
    pub folder: String,
    pub name: String,
    pub version: String,
    pub release: String,
    pub description: String,
    pub groups: String,
    pub set: String,
    pub status: String,
    pub depends: String,
}

impl Record {
    /// Splits a line on `;`. Missing fields are empty; `depends`, the last
    /// field, keeps any further `;`.
    pub fn parse(line: &str) -> Record {
        let mut f = line.splitn(11, ';').map(str::to_string);
        let mut next = || f.next().unwrap_or_default();
        Record {
            checksum: next(),
            timestamp: next(),
            folder: next(),
            name: next(),
            version: next(),
            release: next(),
            description: next(),
            groups: next(),
            set: next(),
            status: next(),
            depends: next(),
        }
    }

    /// The first nine fields, which describe an archive.
    pub fn base(&self) -> String {
        [
            &self.checksum,
            &self.timestamp,
            &self.folder,
            &self.name,
            &self.version,
            &self.release,
            &self.description,
            &self.groups,
            &self.set,
        ]
        .map(String::as_str)
        .join(";")
    }

    pub fn line(&self) -> String {
        format!("{};{};{}", self.base(), self.status, self.depends)
    }

    /// The same archive fields with another status and dependencies.
    pub fn with_status(&self, status: &str) -> Record {
        Record { status: status.to_string(), ..self.clone() }
    }

    /// Names, versions and folders end up in paths: they must be safe.
    pub fn is_valid(&self) -> bool {
        is_valid_name(&self.name)
            && is_valid_version(&self.version)
            && is_valid_version(&self.release)
            && self.folder.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '/' | '-'))
            && !format!("/{}/", self.folder).contains("/../")
    }

    pub fn is_installed(&self) -> bool {
        is_installed_status(&self.status)
    }

    /// Dependency entries (`foo`, `foo>=1.2`).
    pub fn deps(&self) -> impl Iterator<Item = &str> {
        self.depends.split_whitespace()
    }

    /// Archive file name: `<name>-<version>-<release>.tar.zst`, or
    /// `<name>-<version>-<release>-<group>.tar.zst` for a subpackage.
    pub fn archive_name(&self) -> String {
        crate::util::archive_file_name(&self.name, &self.version, &self.release, self.groups.trim())
    }
}

pub fn is_installed_status(status: &str) -> bool {
    status.contains('I') || status.contains('A')
}

/// Package name of a dependency entry, without its version constraint.
pub fn dep_name(dep: &str) -> &str {
    match dep.find(['<', '>', '=']) {
        Some(i) => &dep[..i],
        None => dep,
    }
}

/// A record file loaded in memory, by package name (sorted like
/// `LC_ALL=C sort`).
#[derive(Clone, Debug, Default)]
pub struct Records {
    pub map: BTreeMap<String, Record>,
}

impl Records {
    /// Loads a record file. Records with unsafe names or paths are skipped;
    /// for duplicated names the first record wins. A missing file gives no
    /// records. The status of packages that are not installed is normalized
    /// to `N` (older states used an empty status), keeping their `X` flag.
    pub fn load(path: &Path) -> Records {
        let mut records = Records::default();
        let Ok(content) = fs::read(path) else {
            return records;
        };
        for line in String::from_utf8_lossy(&content).lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut record = Record::parse(line);
            if !record.is_valid() {
                eprintln!("Warning: ignoring invalid record for '{}' in {}", record.name, path.display());
                continue;
            }
            if records.map.contains_key(&record.name) {
                continue;
            }
            if !record.is_installed() {
                let flags: String = record.status.chars().filter(|c| !matches!(c, 'N' | 'I' | 'A' | 'U')).collect();
                record.status = format!("N{flags}");
            }
            records.map.insert(record.name.clone(), record);
        }
        records
    }

    pub fn get(&self, name: &str) -> Option<&Record> {
        self.map.get(name)
    }

    /// Status of a package, `N` when unknown.
    pub fn status(&self, name: &str) -> &str {
        self.map.get(name).map(|r| r.status.as_str()).unwrap_or("N")
    }

    pub fn is_installed(&self, name: &str) -> bool {
        self.map.get(name).is_some_and(Record::is_installed)
    }
}

/// Replaces the record of the same name in a state file, keeping the other
/// lines (comments included), or adds it.
pub fn set_record_in_file(path: &Path, record: &Record) -> anyhow::Result<()> {
    let content = fs::read(path).unwrap_or_default();
    let mut out = String::new();
    let mut found = false;
    for line in String::from_utf8_lossy(&content).lines() {
        if !line.is_empty() && !line.starts_with('#') && Record::parse(line).name == record.name {
            out.push_str(&record.line());
            found = true;
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !found {
        out.push_str(&record.line());
        out.push('\n');
    }
    crate::util::atomic_write(path, out.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format() {
        let line = "abc;123;base;nano;8.0;1;Text editor;;base;I;ncurses libc";
        let r = Record::parse(line);
        assert_eq!(r.name, "nano");
        assert_eq!(r.status, "I");
        assert_eq!(r.deps().collect::<Vec<_>>(), ["ncurses", "libc"]);
        assert_eq!(r.line(), line);
        assert_eq!(r.archive_name(), "nano-8.0-1.tar.zst");
        let short = Record::parse("abc;123;;foo;1;1;desc;;");
        assert_eq!(short.line(), "abc;123;;foo;1;1;desc;;;;");
    }

    #[test]
    fn validity() {
        assert!(Record::parse("a;1;base/sub;foo;1.0;1;;;;;").is_valid());
        assert!(!Record::parse("a;1;../etc;foo;1.0;1;;;;;").is_valid());
        assert!(!Record::parse("a;1;base;../foo;1.0;1;;;;;").is_valid());
        assert!(!Record::parse("a;1;base;foo;1/0;1;;;;;").is_valid());
    }

    #[test]
    fn dependency_names() {
        assert_eq!(dep_name("foo>=1.2"), "foo");
        assert_eq!(dep_name("foo"), "foo");
    }
}
