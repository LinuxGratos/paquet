//! Package archives: `<name>-<version>-<release>.tar.zst`, holding the files
//! of the package relative to `/`, plus metadata entries at the root of the
//! archive, which are never extracted: `.META`, describing the package, and
//! the optional scripts `.PRE-INSTALL`, `.POST-INSTALL`, `.PRE-REMOVE` and
//! `.POST-REMOVE`.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::signals;
use crate::util::{bytes_path, lchown, mknod, set_times};

fn open(path: &Path) -> Result<tar::Archive<zstd::stream::read::Decoder<'static, io::BufReader<fs::File>>>> {
    let file = fs::File::open(path).with_context(|| format!("unable to open {}", path.display()))?;
    let decoder =
        zstd::stream::read::Decoder::new(file).with_context(|| format!("unable to read archive {}", path.display()))?;
    Ok(tar::Archive::new(decoder))
}

/// Package scripts: (name in the recipe directory, entry in the archive).
pub const SCRIPTS: [(&str, &str); 4] = [
    ("pre-install", ".PRE-INSTALL"),
    ("post-install", ".POST-INSTALL"),
    ("pre-remove", ".PRE-REMOVE"),
    ("post-remove", ".POST-REMOVE"),
];

/// True for the metadata entries of an archive (`.META`, scripts), which
/// are not files of the package.
pub fn is_metadata(entry: &[u8]) -> bool {
    entry == b".META" || SCRIPTS.iter().any(|(_, e)| entry == e.as_bytes())
}

/// Archive entry name without its `./` prefix, leading slashes and trailing
/// slash.
pub fn normalize_entry(raw: &[u8]) -> &[u8] {
    let mut entry = raw.strip_prefix(b"./").unwrap_or(raw);
    while let Some(rest) = entry.strip_prefix(b"/") {
        entry = rest;
    }
    entry.strip_suffix(b"/").unwrap_or(entry)
}

/// True if a relative path has a `..` component.
pub fn escapes_root(entry: &[u8]) -> bool {
    entry.split(|&b| b == b'/').any(|c| c == b"..")
}

/// One entry of an archive listing.
pub struct ListEntry {
    /// Normalized name (see [`normalize_entry`]).
    pub name: Vec<u8>,
    pub is_dir: bool,
}

/// Lists the entries of an archive, in archive order.
pub fn list(path: &Path) -> Result<Vec<ListEntry>> {
    let mut archive = open(path)?;
    let mut entries = Vec::new();
    let read = (|| -> io::Result<()> {
        for entry in archive.entries()? {
            let entry = entry?;
            let raw = entry.path_bytes();
            let is_dir = entry.header().entry_type().is_dir() || raw.ends_with(b"/");
            entries.push(ListEntry { name: normalize_entry(&raw).to_vec(), is_dir });
        }
        Ok(())
    })();
    read.with_context(|| format!("unable to read archive {}", path.display()))?;
    Ok(entries)
}

/// Type and attributes of an archive entry, as stored in its header.
pub struct EntryInfo {
    /// Normalized name (see [`normalize_entry`]).
    pub name: Vec<u8>,
    /// `-` (file or hard link), `d`, `l`, `c`, `b` or `p`, like `ls -l`.
    pub kind: char,
    /// Permission bits, setuid, setgid and sticky bits included.
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    /// Target of a symlink.
    pub link: Vec<u8>,
    /// Major and minor numbers of a device.
    pub device: Option<(u32, u32)>,
}

/// Type, permissions, owner and link target of the files of an archive,
/// metadata entries excluded, in archive order.
pub fn entry_infos(path: &Path) -> Result<Vec<EntryInfo>> {
    let mut archive = open(path)?;
    let mut infos = Vec::new();
    let read = (|| -> io::Result<()> {
        for entry in archive.entries()? {
            let entry = entry?;
            let raw = entry.path_bytes();
            let name = normalize_entry(&raw).to_vec();
            let header = entry.header();
            let t = header.entry_type();
            if name.is_empty() || name == b"." || is_metadata(&name) {
                continue;
            }
            let kind = if t.is_dir() || raw.ends_with(b"/") {
                'd'
            } else if t.is_symlink() {
                'l'
            } else if t.is_character_special() {
                'c'
            } else if t.is_block_special() {
                'b'
            } else if t.is_fifo() {
                'p'
            } else if t.is_file() || t.is_hard_link() || t == tar::EntryType::Continuous {
                '-'
            } else {
                continue;
            };
            let device = match kind {
                'c' | 'b' => Some((header.device_major()?.unwrap_or(0), header.device_minor()?.unwrap_or(0))),
                _ => None,
            };
            infos.push(EntryInfo {
                name,
                kind,
                mode: header.mode()? & 0o7777,
                uid: header.uid()?,
                gid: header.gid()?,
                link: if kind == 'l' {
                    entry.link_name_bytes().map(|l| l.into_owned()).unwrap_or_default()
                } else {
                    Vec::new()
                },
                device,
            });
        }
        Ok(())
    })();
    read.with_context(|| format!("unable to read archive {}", path.display()))?;
    Ok(infos)
}

/// Content of the `.META` file of an archive, if any.
pub fn read_meta(path: &Path) -> Result<Option<String>> {
    let mut found = read_metadata(path, &[".META"])?;
    Ok(found.remove(".META").map(|c| String::from_utf8_lossy(&c).into_owned()))
}

/// Contents of some metadata entries of an archive (`.META`, `.POST-INSTALL`...),
/// by name. Missing entries are absent from the result.
pub fn read_metadata(path: &Path, names: &[&str]) -> Result<HashMap<String, Vec<u8>>> {
    let mut archive = open(path)?;
    let mut found = HashMap::new();
    let read = (|| -> io::Result<()> {
        for entry in archive.entries()? {
            let mut entry = entry?;
            let raw = entry.path_bytes().into_owned();
            let name = String::from_utf8_lossy(raw.strip_prefix(b"./").unwrap_or(&raw)).into_owned();
            if names.contains(&name.as_str()) && !found.contains_key(&name) {
                let mut content = Vec::new();
                entry.read_to_end(&mut content)?;
                found.insert(name, content);
                if found.len() == names.len() {
                    break;
                }
            }
        }
        Ok(())
    })();
    read.with_context(|| format!("unable to read archive {}", path.display()))?;
    Ok(found)
}

/// A link whose creation waits for the end of the extraction.
struct Delayed {
    dest: PathBuf,
    target: Vec<u8>,
    uid: u64,
    gid: u64,
    mtime: u64,
}

/// Extracts an archive under `root`, like `tar -xf <archive> -C <root>
/// --exclude=.META --no-overwrite-dir` (the scripts are not extracted either):
/// - existing directories (or symlinks to directories) are left as they
///   are, with their permissions;
/// - existing files are unlinked before being replaced, so that hard links
///   to them (transaction backups) keep the old content;
/// - ownership is restored when running as root;
/// - symlinks pointing to an absolute path or through `..` are only created
///   once the files are extracted, so the archive cannot write through them.
pub fn extract(path: &Path, root: &Path) -> Result<()> {
    let mut archive = open(path)?;
    let as_root = crate::util::euid() == 0;
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_preserve_ownerships(as_root);
    archive.set_overwrite(true);
    archive.set_unpack_xattrs(false);

    let mut delayed: Vec<Delayed> = Vec::new();
    let mut retry_hard: Vec<(PathBuf, PathBuf)> = Vec::new();
    let entries = archive.entries().with_context(|| format!("unable to read archive {}", path.display()))?;
    for entry in entries {
        signals::check()?;
        let mut entry = entry.with_context(|| format!("unable to read archive {}", path.display()))?;
        let raw = entry.path_bytes().into_owned();
        let rel = normalize_entry(&raw).to_vec();
        if rel.is_empty() || rel == b"." || is_metadata(&rel) {
            continue;
        }
        if escapes_root(&rel) {
            bail!("archive entry '{}' escapes the target root.", String::from_utf8_lossy(&rel));
        }
        let dest = root.join(bytes_path(&rel));
        let header = entry.header().clone();
        let kind = header.entry_type();
        let what = || format!("unable to extract {}", dest.display());

        if kind.is_dir() {
            // --no-overwrite-dir: keep existing directories as they are
            if dest.is_dir() {
                continue;
            }
            create_parent(&dest).with_context(what)?;
            clear_dest(&dest).with_context(what)?;
            entry.unpack(&dest).with_context(what)?;
            continue;
        }
        if kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions()
            || kind.is_gnu_longname()
            || kind.is_gnu_longlink()
        {
            continue;
        }
        create_parent(&dest).with_context(what)?;
        clear_dest(&dest).with_context(what)?;

        if kind.is_symlink() {
            let target = entry.link_name_bytes().map(|t| t.into_owned()).unwrap_or_default();
            if target.starts_with(b"/") || escapes_root(&target) {
                delayed.push(Delayed {
                    dest,
                    target,
                    uid: header.uid().unwrap_or(0),
                    gid: header.gid().unwrap_or(0),
                    mtime: header.mtime().unwrap_or(0),
                });
            } else {
                entry.unpack(&dest).with_context(what)?;
            }
        } else if kind.is_hard_link() {
            let target = entry.link_name_bytes().map(|t| t.into_owned()).unwrap_or_default();
            let target_rel = normalize_entry(&target);
            if target_rel.is_empty() || escapes_root(target_rel) {
                bail!("hard link {} points outside the target root.", dest.display());
            }
            let source = root.join(bytes_path(target_rel));
            if fs::hard_link(&source, &dest).is_err() {
                retry_hard.push((source, dest));
            }
        } else if kind.is_character_special() || kind.is_block_special() || kind.is_fifo() {
            let type_bits = if kind.is_character_special() {
                libc::S_IFCHR
            } else if kind.is_block_special() {
                libc::S_IFBLK
            } else {
                libc::S_IFIFO
            };
            let mode = header.mode().unwrap_or(0o644);
            let major = header.device_major().ok().flatten().unwrap_or(0);
            let minor = header.device_minor().ok().flatten().unwrap_or(0);
            let dev = libc::makedev(major, minor);
            mknod(&dest, type_bits | (mode & 0o7777), dev as u64).with_context(what)?;
            if as_root {
                lchown(&dest, header.uid().unwrap_or(0) as u32, header.gid().unwrap_or(0) as u32).with_context(what)?;
            }
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode & 0o7777)).with_context(what)?;
        } else {
            entry.unpack(&dest).with_context(what)?;
        }
    }

    for link in delayed {
        signals::check()?;
        clear_dest(&link.dest)?;
        std::os::unix::fs::symlink(bytes_path(&link.target), &link.dest)
            .with_context(|| format!("unable to create symlink {}", link.dest.display()))?;
        if as_root {
            let _ = lchown(&link.dest, link.uid as u32, link.gid as u32);
        }
        set_times(&link.dest, link.mtime as i64);
    }
    for (source, dest) in retry_hard {
        clear_dest(&dest)?;
        fs::hard_link(&source, &dest)
            .with_context(|| format!("unable to link {} to {}", dest.display(), source.display()))?;
    }
    Ok(())
}

fn create_parent(dest: &Path) -> io::Result<()> {
    match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => fs::create_dir_all(parent),
        _ => Ok(()),
    }
}

/// Makes room for a new entry: an existing file or symlink is unlinked, an
/// existing empty directory removed.
fn clear_dest(dest: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dest) {
        Ok(meta) if meta.is_dir() => fs::remove_dir(dest),
        Ok(_) => fs::remove_file(dest),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Creates a `.tar.zst` archive of the content of `src`, like
/// `tar --zstd --sort=name --numeric-owner -cf <dest> -C <src> .`. With
/// `root_owner`, every file belongs to root (0:0) in the archive.
pub fn create(src: &Path, dest: &Path, root_owner: bool) -> Result<()> {
    let file = fs::File::create(dest).with_context(|| format!("unable to create {}", dest.display()))?;
    let encoder = zstd::stream::write::Encoder::new(file, 3)?;
    let mut builder = tar::Builder::new(encoder);
    let mut links = HashMap::new();
    add_dir(&mut builder, src, Path::new(""), root_owner, &mut links)
        .with_context(|| format!("unable to archive {}", src.display()))?;
    let encoder = builder.into_inner()?;
    let file = encoder.finish()?;
    file.sync_all()?;
    Ok(())
}

fn sorted_children(dir: &Path) -> io::Result<Vec<std::ffi::OsString>> {
    let mut names: Vec<_> = fs::read_dir(dir)?.map(|e| e.map(|e| e.file_name())).collect::<io::Result<_>>()?;
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(names)
}

fn add_dir<W: io::Write>(
    builder: &mut tar::Builder<W>,
    base: &Path,
    rel: &Path,
    root_owner: bool,
    links: &mut HashMap<(u64, u64), PathBuf>,
) -> io::Result<()> {
    for name in sorted_children(&base.join(rel))? {
        let rel = rel.join(&name);
        let full = base.join(&rel);
        let meta = fs::symlink_metadata(&full)?;
        let ft = meta.file_type();
        let mut header = tar::Header::new_gnu();
        header.set_mode(meta.mode() & 0o7777);
        header.set_mtime(meta.mtime().max(0) as u64);
        if root_owner {
            header.set_uid(0);
            header.set_gid(0);
        } else {
            header.set_uid(meta.uid() as u64);
            header.set_gid(meta.gid() as u64);
        }
        header.set_size(0);
        if ft.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            builder.append_data(&mut header, &rel, io::empty())?;
            add_dir(builder, base, &rel, root_owner, links)?;
        } else if ft.is_symlink() {
            header.set_entry_type(tar::EntryType::Symlink);
            builder.append_link(&mut header, &rel, fs::read_link(&full)?)?;
        } else if ft.is_file() {
            if meta.nlink() > 1 {
                if let Some(first) = links.get(&(meta.dev(), meta.ino())) {
                    header.set_entry_type(tar::EntryType::Link);
                    builder.append_link(&mut header, &rel, first)?;
                    continue;
                }
                links.insert((meta.dev(), meta.ino()), rel.clone());
            }
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(meta.len());
            builder.append_data(&mut header, &rel, fs::File::open(&full)?)?;
        } else if ft.is_char_device() || ft.is_block_device() || ft.is_fifo() {
            let kind = if ft.is_char_device() {
                tar::EntryType::Char
            } else if ft.is_block_device() {
                tar::EntryType::Block
            } else {
                tar::EntryType::Fifo
            };
            header.set_entry_type(kind);
            let rdev = meta.rdev() as libc::dev_t;
            header.set_device_major(libc::major(rdev))?;
            header.set_device_minor(libc::minor(rdev))?;
            builder.append_data(&mut header, &rel, io::empty())?;
        } else {
            eprintln!("Warning: {} skipped (sockets are not archived)", full.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names() {
        assert_eq!(normalize_entry(b"./usr/bin/"), b"usr/bin");
        assert_eq!(normalize_entry(b"//etc/foo"), b"etc/foo");
        assert_eq!(normalize_entry(b"./"), b"");
        assert!(escapes_root(b"usr/../../etc"));
        assert!(!escapes_root(b"usr/..foo"));
    }

    #[test]
    fn create_list_extract() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("usr/bin")).unwrap();
        fs::write(src.join("usr/bin/tool"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(src.join("usr/bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::hard_link(src.join("usr/bin/tool"), src.join("usr/bin/tool2")).unwrap();
        std::os::unix::fs::symlink("tool", src.join("usr/bin/link")).unwrap();
        fs::write(src.join(".META"), b"Description: test\n").unwrap();
        let archive = dir.path().join("t-1-1.tar.zst");
        create(&src, &archive, false).unwrap();

        let names: Vec<String> =
            list(&archive).unwrap().iter().map(|e| String::from_utf8_lossy(&e.name).into_owned()).collect();
        assert_eq!(names, [".META", "usr", "usr/bin", "usr/bin/link", "usr/bin/tool", "usr/bin/tool2"]);
        assert_eq!(read_meta(&archive).unwrap().as_deref(), Some("Description: test\n"));

        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        extract(&archive, &root).unwrap();
        assert!(!root.join(".META").exists());
        assert_eq!(fs::read(root.join("usr/bin/tool2")).unwrap(), b"#!/bin/sh\n");
        assert_eq!(fs::read_link(root.join("usr/bin/link")).unwrap(), Path::new("tool"));
        let mode = fs::metadata(root.join("usr/bin/tool")).unwrap().mode();
        assert_eq!(mode & 0o777, 0o755);

        // Replacing a file unlinks it: a hard link keeps the old content
        fs::write(root.join("usr/bin/tool"), b"old").unwrap();
        fs::remove_file(root.join("usr/bin/tool2")).unwrap();
        fs::hard_link(root.join("usr/bin/tool"), dir.path().join("backup")).unwrap();
        extract(&archive, &root).unwrap();
        assert_eq!(fs::read(dir.path().join("backup")).unwrap(), b"old");
        assert_eq!(fs::read(root.join("usr/bin/tool")).unwrap(), b"#!/bin/sh\n");
    }
}
