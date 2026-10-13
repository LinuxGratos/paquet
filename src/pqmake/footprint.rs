//! Footprints: the files of a package with their type, permissions and
//! owner, kept with its recipe, like in CRUX. `.footprint` describes the
//! main package, `.footprint.<group>` each subpackage:
//!
//! ```text
//! drwxr-xr-x  root/root  usr/bin/
//! -rwsr-xr-x  root/root  usr/bin/su
//! lrwxrwxrwx  root/root  usr/lib/libz.so -> libz.so.1
//! ```
//!
//! (the fields are separated by tabs).
//!
//! After each build, the new archives are compared with these files, and
//! the differences are shown to the packager, who decides whether the recipe
//! needs fixing. The `sources` subpackage has no footprint: it holds the
//! recipe and its sources, whose names change with every version. The files are only written when missing, or on request
//! (`pqmake --update-footprint`).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Pqmake;

use crate::archive::{self, EntryInfo};
use crate::util;

/// File name of the footprint of a package (`group` is empty for the main
/// package).
pub fn file_name(group: &str) -> String {
    if group.is_empty() { ".footprint".to_string() } else { format!(".footprint.{group}") }
}

/// User and group names, from the `passwd` and `group` files of a system.
pub struct Names {
    users: HashMap<u64, String>,
    groups: HashMap<u64, String>,
}

impl Names {
    pub fn of_system(root: &Path) -> Names {
        let read = |file: &str| -> HashMap<u64, String> {
            let content = fs::read_to_string(root.join(file)).unwrap_or_default();
            content
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split(':');
                    let name = fields.next()?;
                    let id = fields.nth(1)?.parse().ok()?;
                    Some((id, name.to_string()))
                })
                .collect()
        };
        Names { users: read("etc/passwd"), groups: read("etc/group") }
    }

    fn owner(&self, uid: u64, gid: u64) -> String {
        let user = self.users.get(&uid).cloned().unwrap_or_else(|| uid.to_string());
        let group = self.groups.get(&gid).cloned().unwrap_or_else(|| gid.to_string());
        format!("{user}/{group}")
    }
}

/// Permissions like `ls -l`: `-rwsr-xr-x`, `drwxrwxrwt`...
pub fn mode_string(kind: char, mode: u32) -> String {
    let mut s = String::with_capacity(10);
    s.push(kind);
    for (shift, special, set, unset) in [(6, 0o4000, 's', 'S'), (3, 0o2000, 's', 'S'), (0, 0o1000, 't', 'T')] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(match (bits & 1 != 0, mode & special != 0) {
            (true, true) => set,
            (false, true) => unset,
            (true, false) => 'x',
            (false, false) => '-',
        });
    }
    s
}

/// One line of a footprint, split into its parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub mode: String,
    pub owner: String,
    /// Path, with a trailing `/` for a directory.
    pub path: String,
    /// ` -> <target>` for a symlink, ` (<major>, <minor>)` for a device.
    pub extra: String,
}

impl Line {
    fn from_entry(e: &EntryInfo, names: &Names) -> Line {
        let mut path = String::from_utf8_lossy(&e.name).into_owned();
        if e.kind == 'd' {
            path.push('/');
        }
        let extra = match (e.kind, e.device) {
            ('l', _) => format!(" -> {}", String::from_utf8_lossy(&e.link)),
            (_, Some((major, minor))) => format!(" ({major}, {minor})"),
            _ => String::new(),
        };
        Line { mode: mode_string(e.kind, e.mode), owner: names.owner(e.uid, e.gid), path, extra }
    }

    /// Parses `<mode>\t<owner>\t<path>[ -> <target>]`.
    fn parse(text: &str) -> Option<Line> {
        let mut fields = text.splitn(3, '\t');
        let (mode, owner, rest) = (fields.next()?, fields.next()?, fields.next()?);
        let (path, extra) = match mode.chars().next() {
            Some('l') => rest.split_once(" -> ").map(|(p, t)| (p, format!(" -> {t}")))?,
            Some('c' | 'b') => rest.rsplit_once(" (").map(|(p, d)| (p, format!(" ({d}")))?,
            _ => (rest, String::new()),
        };
        Some(Line { mode: mode.into(), owner: owner.into(), path: path.into(), extra })
    }

    fn text(&self) -> String {
        format!("{}\t{}\t{}{}", self.mode, self.owner, self.path, self.extra)
    }
}

/// Footprint of an archive, sorted by path.
pub fn of_archive(path: &Path, names: &Names) -> Result<Vec<Line>> {
    let mut lines: Vec<Line> = archive::entry_infos(path)?.iter().map(|e| Line::from_entry(e, names)).collect();
    lines.sort_by(|a, b| a.path.cmp(&b.path));
    lines.dedup_by(|a, b| a.path == b.path);
    Ok(lines)
}

pub fn render(lines: &[Line]) -> String {
    lines.iter().map(|l| l.text() + "\n").collect()
}

/// Lines of a footprint file; unreadable lines are skipped.
pub fn parse(content: &str) -> Vec<Line> {
    content.lines().filter_map(Line::parse).collect()
}

/// A difference between two footprints.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    New(Line),
    Missing(Line),
    Changed { old: Line, new: Line },
}

/// Differences from `old` to `new`, by path.
pub fn diff(old: &[Line], new: &[Line]) -> Vec<Change> {
    let mut by_path: BTreeMap<&str, (Option<&Line>, Option<&Line>)> = BTreeMap::new();
    for line in old {
        by_path.entry(&line.path).or_default().0 = Some(line);
    }
    for line in new {
        by_path.entry(&line.path).or_default().1 = Some(line);
    }
    by_path
        .into_values()
        .filter_map(|pair| match pair {
            (None, Some(n)) => Some(Change::New(n.clone())),
            (Some(o), None) => Some(Change::Missing(o.clone())),
            (Some(o), Some(n)) if o != n => Some(Change::Changed { old: o.clone(), new: n.clone() }),
            _ => None,
        })
        .collect()
}

/// The setuid, setgid and sticky bits of a mode string.
fn special_bits(mode: &str) -> [bool; 3] {
    let at = |i: usize| mode.as_bytes().get(i).is_some_and(|c| matches!(c, b's' | b'S' | b't' | b'T'));
    [at(3), at(6), at(9)]
}

const SPECIAL_NAMES: [&str; 3] = ["setuid", "setgid", "sticky bit"];

/// Readable form of a change, as shown after a build.
pub fn describe(change: &Change) -> String {
    let line =
        |tag: &str, l: &Line, note: String| format!("{tag:<8} {} {} {}{}{note}", l.mode, l.owner, l.path, l.extra);
    match change {
        Change::New(l) | Change::Missing(l) => {
            let tag = if matches!(change, Change::New(_)) { "NEW" } else { "MISSING" };
            let bits: Vec<&str> =
                special_bits(&l.mode).iter().zip(SPECIAL_NAMES).filter(|(on, _)| **on).map(|(_, n)| n).collect();
            let note = if bits.is_empty() || l.mode.starts_with('d') {
                String::new()
            } else {
                format!("  ({})", bits.join(", "))
            };
            line(tag, l, note)
        }
        Change::Changed { old, new } => {
            let mut was = Vec::new();
            if old.mode != new.mode {
                was.push(old.mode.clone());
            }
            if old.owner != new.owner {
                was.push(old.owner.clone());
            }
            if old.extra != new.extra {
                was.push(old.extra.trim_start().to_string());
            }
            let mut notes = Vec::new();
            for ((before, after), name) in
                special_bits(&old.mode).iter().zip(special_bits(&new.mode)).zip(SPECIAL_NAMES)
            {
                match (before, after) {
                    (false, true) => notes.push(format!("{name} added")),
                    (true, false) => notes.push(format!("{name} removed")),
                    _ => {}
                }
            }
            let notes = if notes.is_empty() { String::new() } else { format!(": {}", notes.join(", ")) };
            line("CHANGED", new, format!("  (was {}{notes})", was.join(" ")))
        }
    }
}

impl Pqmake {
    /// Groups of a recipe whose archive is in the repository, for its
    /// current version: "" for the main package, then the subpackages
    /// (`sources` aside).
    fn produced_groups(&self, name: &str) -> Vec<(String, PathBuf)> {
        let main = (String::new(), self.archive_path(name, name));
        let mut groups: Vec<(String, PathBuf)> = std::iter::once(main).filter(|(_, a)| a.is_file()).collect();
        groups.extend(self.existing_subpackages(name).into_iter().filter(|(g, _)| g != "sources"));
        groups
    }

    /// Footprints of subpackages (`.footprint.<group>`) in the recipe
    /// directory, by group.
    fn subpackage_footprints(&self, name: &str) -> Vec<String> {
        let dir = self.recipe_path[name].parent().unwrap_or(Path::new("."));
        super::read_dir_sorted(dir)
            .iter()
            .filter_map(|p| p.file_name()?.to_str()?.strip_prefix(".footprint.").map(String::from))
            .filter(|g| g != "sources" && util::is_package_group(g))
            .collect()
    }

    /// Footprint file of a package, and its path as shown to the packager
    /// (relative to the recipes directory).
    fn footprint_file(&self, name: &str, group: &str) -> (PathBuf, String) {
        let dir = self.recipe_path[name].parent().unwrap_or(Path::new("."));
        let file = dir.join(file_name(group));
        let shown = file.strip_prefix(&self.s.recipes_dir).unwrap_or(&file).display().to_string();
        (file, shown)
    }

    fn names(&self) -> Names {
        Names::of_system(self.s.chroot.as_deref().unwrap_or(Path::new("/")))
    }

    /// Writes a footprint. In a chroot build, pqmake runs as root: the file
    /// gets the owner of the Pkgfile, like the rest of the recipe.
    fn write_footprint(&self, name: &str, file: &Path, lines: &[Line]) -> Result<()> {
        util::atomic_write(file, render(lines).as_bytes())?;
        if util::euid() == 0 {
            use std::os::unix::fs::MetadataExt;
            if let Ok(meta) = fs::metadata(&self.recipe_path[name]) {
                let _ = std::os::unix::fs::chown(file, Some(meta.uid()), Some(meta.gid()));
            }
        }
        Ok(())
    }

    /// Compares the new archives of a recipe, as (group, archive), with its
    /// footprints, and shows the differences. Missing footprints are
    /// written. Never fails the build: the packager decides what to do.
    pub fn check_footprints(&mut self, name: &str, produced: &[(String, PathBuf)]) {
        let names = self.names();
        let mut changed = false;
        for (group, archive) in produced {
            let (group, archive) = (group.as_str(), archive.as_path());
            let pkgname = if group.is_empty() { name.to_string() } else { format!("{name}.{group}") };
            let (file, shown) = self.footprint_file(name, group);
            let result = (|| -> Result<bool> {
                let new = of_archive(archive, &names)?;
                let old = match fs::read_to_string(&file) {
                    Ok(content) => parse(&content),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        self.write_footprint(name, &file, &new)?;
                        println!("  new footprint: {shown}");
                        return Ok(false);
                    }
                    Err(e) => return Err(e).with_context(|| format!("unable to read {}", file.display())),
                };
                let changes = diff(&old, &new);
                if !changes.is_empty() {
                    println!("==> Footprint of {pkgname} changed ({shown}):");
                    for change in &changes {
                        println!("    {}", describe(change));
                    }
                }
                Ok(!changes.is_empty())
            })();
            match result {
                Ok(c) => changed |= c,
                Err(err) => eprintln!("Warning: footprint of {pkgname} not checked: {err:#}"),
            }
        }
        for group in self.subpackage_footprints(name).iter().filter(|g| !produced.iter().any(|(p, _)| p == *g)) {
            let (file, shown) = self.footprint_file(name, group);
            if file.is_file() {
                println!("==> {name}.{group} is no longer produced ({shown})");
                changed = true;
            }
        }
        if changed {
            println!("    If these changes are expected: pqmake --update-footprint {name}");
            self.footprint_changed.push(name.to_string());
        }
    }

    /// Rewrites the footprints of a recipe from its archives, without
    /// building it, and removes those of subpackages it no longer produces.
    pub fn update_footprints(&self, name: &str) -> Result<()> {
        if !self.is_built(name) {
            let r = self.recipe(name);
            bail!(
                "{name} {}-{} is not built ({} not found).",
                r.version,
                r.release,
                self.archive_path(name, name).display()
            );
        }
        let names = self.names();
        let produced = self.produced_groups(name);
        let mut updated = false;
        for (group, archive) in &produced {
            let (file, shown) = self.footprint_file(name, group);
            let lines = of_archive(archive, &names)?;
            if fs::read_to_string(&file).ok().map(|c| parse(&c)) != Some(lines.clone()) {
                self.write_footprint(name, &file, &lines)?;
                println!("  {shown} updated");
                updated = true;
            }
        }
        for group in self.subpackage_footprints(name).iter().filter(|g| !produced.iter().any(|(p, _)| p == *g)) {
            let (file, shown) = self.footprint_file(name, group);
            if file.is_file() {
                fs::remove_file(&file).with_context(|| format!("unable to remove {}", file.display()))?;
                println!("  {shown} removed");
                updated = true;
            }
        }
        if !updated {
            println!("  footprints of {name} are up to date");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        assert_eq!(mode_string('-', 0o4755), "-rwsr-xr-x");
        assert_eq!(mode_string('d', 0o1777), "drwxrwxrwt");
        assert_eq!(mode_string('-', 0o2644), "-rw-r-Sr--");
        assert_eq!(mode_string('l', 0o777), "lrwxrwxrwx");
    }

    #[test]
    fn lines_round_trip() {
        let text = "drwxr-xr-x\troot/root\tusr/\n\
                    lrwxrwxrwx\troot/root\tusr/lib/libz.so -> libz.so.1\n\
                    crw-rw-rw-\troot/root\tdev/null (1, 3)\n\
                    -rwsr-xr-x\troot/root\tusr/bin/su\n";
        let lines = parse(text);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].path, "usr/lib/libz.so");
        assert_eq!(lines[2].extra, " (1, 3)");
        assert_eq!(render(&lines), text);
    }

    #[test]
    fn differences() {
        let old = parse(
            "-rwsr-xr-x\troot/root\tusr/bin/su\n\
             -rw-r--r--\troot/root\tusr/share/a\n\
             lrwxrwxrwx\troot/root\tusr/lib/libz.so -> libz.so.1\n",
        );
        let new = parse(
            "-rwxr-xr-x\troot/root\tusr/bin/su\n\
             -rwsr-xr-x\troot/root\tusr/bin/new\n\
             lrwxrwxrwx\troot/root\tusr/lib/libz.so -> libz.so.2\n",
        );
        let shown: Vec<String> = diff(&old, &new).iter().map(describe).collect();
        assert_eq!(
            shown,
            [
                "NEW      -rwsr-xr-x root/root usr/bin/new  (setuid)",
                "CHANGED  -rwxr-xr-x root/root usr/bin/su  (was -rwsr-xr-x: setuid removed)",
                "CHANGED  lrwxrwxrwx root/root usr/lib/libz.so -> libz.so.2  (was -> libz.so.1)",
                "MISSING  -rw-r--r-- root/root usr/share/a",
            ]
        );
        assert!(diff(&old, &old).is_empty());
    }
}
