//! File lists of the installed packages (`/var/lib/pq/files/<package>`).
//!
//! A list starts with a `# root: <dir>` header and holds one path per line,
//! relative to that root, which is itself relative to the target root (so
//! protected directories are matched regardless of any `--chroot`).
//! Directories are only recorded when the package creates them (or already
//! owned them, for upgrades).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::archive;
use crate::util::{bytes_path, concat_path, dirname_bytes, exists_or_symlink, is_real_dir};

/// Directories whose files are never removed, nor overwritten when they
/// already exist (configuration, package database).
pub const PROTECTED_DIRS: [&[u8]; 2] = [b"/etc", b"/var/lib/pq"];

/// True if a root-relative path (`/etc/foo`) is protected.
pub fn is_protected(path: &[u8]) -> bool {
    PROTECTED_DIRS.iter().any(|dir| path == *dir || (path.starts_with(dir) && path.get(dir.len()) == Some(&b'/')))
}

fn lines(content: &[u8]) -> impl Iterator<Item = &[u8]> {
    content.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l))
}

fn is_entry(line: &[u8]) -> bool {
    !line.is_empty() && line[0] != b'#'
}

fn collapse_slashes(mut path: &[u8]) -> &[u8] {
    while path.starts_with(b"//") {
        path = &path[1..];
    }
    path
}

/// Entries of a file list, as (path relative to the target root, real
/// path). `/` itself is skipped.
pub fn entries(content: &[u8], target_root: &[u8]) -> Vec<(Vec<u8>, PathBuf)> {
    let mut root = target_root.to_vec();
    let mut out = Vec::new();
    for line in lines(content) {
        if let Some(header_root) = line.strip_prefix(b"# root: ") {
            root = target_root.to_vec();
            root.extend_from_slice(header_root.strip_suffix(b"/").unwrap_or(header_root));
            continue;
        }
        if !is_entry(line) {
            continue;
        }
        let mut path = collapse_slashes(line).to_vec();
        if !path.starts_with(b"/") {
            path.insert(0, b'/');
        }
        if path == b"/" {
            continue;
        }
        let real = concat_path(&root, &path);
        out.push((path, real));
    }
    out
}

/// Builds the file list of an archive before extracting it, and checks that
/// it does not overwrite files of other packages. Paths listed in `owned`
/// (the list of the version being upgraded) are not conflicts, and neither
/// are protected files: those are kept, the new version going to
/// `<file>.pqnew`.
pub fn prepare(archive_path: &Path, owned: Option<&[u8]>, target_root: &[u8]) -> Result<Vec<u8>> {
    let mut owned_set: HashSet<Vec<u8>> = HashSet::new();
    if let Some(content) = owned {
        for line in lines(content).filter(|l| is_entry(l)) {
            let mut l = line;
            while let Some(rest) = l.strip_prefix(b"/") {
                l = rest;
            }
            owned_set.insert(l.to_vec());
        }
    }

    let listing = archive::list(archive_path)?;
    let mut out = b"# root: /\n".to_vec();
    let mut recorded: HashSet<Vec<u8>> = HashSet::new();
    let mut existing: Vec<PathBuf> = Vec::new();
    let real = |entry: &[u8]| {
        let mut p = target_root.to_vec();
        p.push(b'/');
        p.extend_from_slice(entry);
        bytes_path(&p)
    };

    for item in &listing {
        let entry = item.name.as_slice();
        if entry.is_empty() || entry == b"." || archive::is_metadata(entry) {
            continue;
        }
        if archive::escapes_root(entry) {
            bail!("archive entry '{}' escapes the target root.", String::from_utf8_lossy(entry));
        }
        let real_path = real(entry);
        let is_owned = owned_set.contains(entry);
        if item.is_dir {
            if real_path.is_dir() {
                if !is_owned {
                    continue;
                }
            } else if exists_or_symlink(&real_path) && !is_owned {
                existing.push(real_path);
            }
        } else if exists_or_symlink(&real_path) && !is_owned && !is_protected(&[b"/", entry].concat()) {
            existing.push(real_path);
        }
        if recorded.insert(entry.to_vec()) {
            out.push(b'/');
            out.extend_from_slice(entry);
            out.push(b'\n');
        }
        // Parent directories that the extraction creates implicitly
        let mut parent = dirname_bytes(entry);
        while parent != b"." && !recorded.contains(parent) {
            if real(parent).exists() && !owned_set.contains(parent) {
                break;
            }
            recorded.insert(parent.to_vec());
            out.push(b'/');
            out.extend_from_slice(parent);
            out.push(b'\n');
            parent = dirname_bytes(parent);
        }
    }

    if !existing.is_empty() {
        let list: Vec<String> = existing.iter().map(|p| p.display().to_string()).collect();
        bail!("the following files already exist:\n{}", list.join("\n"));
    }
    Ok(out)
}

/// Removes the files of a list. Protected files are kept; directories are
/// only removed when empty, deepest first.
pub fn remove_files(content: &[u8], target_root: &[u8]) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for (path, real) in entries(content, target_root) {
        if is_protected(&path) {
            println!("Keeping protected file: {}", real.display());
            continue;
        }
        if is_real_dir(&real) {
            dirs.push(real);
        } else if exists_or_symlink(&real) {
            println!("Removing file: {}", real.display());
            if let Err(err) = fs::remove_file(&real) {
                eprintln!("Warning: unable to remove {}: {err}", real.display());
            }
        }
    }
    remove_empty_dirs(dirs, true);
}

/// Removes the directories that are empty, deepest first.
pub fn remove_empty_dirs(mut dirs: Vec<PathBuf>, verbose: bool) {
    use std::os::unix::ffi::OsStrExt;
    dirs.sort_by(|a, b| b.as_os_str().as_bytes().cmp(a.as_os_str().as_bytes()));
    dirs.dedup();
    for dir in dirs {
        if fs::remove_dir(&dir).is_ok() && verbose {
            println!("Removing directory: {}", dir.display());
        }
    }
}

/// Entries of `old` that are missing from `new`, keeping the root header of
/// `old`.
pub fn stale(old: &[u8], new: &[u8]) -> Vec<u8> {
    let kept: HashSet<&[u8]> = lines(new).filter(|l| is_entry(l)).collect();
    let mut out = Vec::new();
    for line in lines(old) {
        if line.starts_with(b"# root: ") {
            out.extend_from_slice(line);
            out.push(b'\n');
            continue;
        }
        if !is_entry(line) {
            continue;
        }
        let mut path = line;
        while path.starts_with(b"//") {
            path = &path[1..];
        }
        if !kept.contains(path) {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_paths() {
        assert!(is_protected(b"/etc"));
        assert!(is_protected(b"/etc/fstab"));
        assert!(is_protected(b"/var/lib/pq/PQREPO"));
        assert!(!is_protected(b"/etcetera"));
        assert!(!is_protected(b"/usr/etc"));
    }

    #[test]
    fn list_entries() {
        let content = b"# root: /\n/usr\n//usr/bin\nusr/bin/nano\n/\n\n";
        let e = entries(content, b"/mnt");
        let paths: Vec<_> = e.iter().map(|(p, r)| (p.clone(), r.clone())).collect();
        assert_eq!(
            paths,
            vec![
                (b"/usr".to_vec(), PathBuf::from("/mnt/usr")),
                (b"/usr/bin".to_vec(), PathBuf::from("/mnt/usr/bin")),
                (b"/usr/bin/nano".to_vec(), PathBuf::from("/mnt/usr/bin/nano")),
            ]
        );
    }

    #[test]
    fn stale_entries() {
        let old = b"# root: /\n/usr\n/usr/bin/a\n//usr/bin/b\n";
        let new = b"# root: /\n/usr\n/usr/bin/b\n";
        assert_eq!(stale(old, new), b"# root: /\n/usr/bin/a\n");
    }
}
