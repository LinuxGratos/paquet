//! Helpers shared by the three tools.

use std::ffi::{CStr, OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Error of an operation whose failure was already reported to the user:
/// the command only has to exit with a failure status.
#[derive(Debug)]
pub struct Reported;

impl fmt::Display for Reported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("failed")
    }
}

impl std::error::Error for Reported {}

/// Error of an operation interrupted by a signal (Ctrl-C, kill).
#[derive(Debug)]
pub struct Interrupted(pub i32);

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "interrupted by signal {}", self.0)
    }
}

impl std::error::Error for Interrupted {}

/// Prints an error and its causes on stderr.
pub fn report(err: &anyhow::Error) {
    if err.downcast_ref::<Reported>().is_some() || err.downcast_ref::<Interrupted>().is_some() {
        return;
    }
    let mut chain = err.chain();
    if let Some(first) = chain.next() {
        eprintln!("Error: {first}");
    }
    for cause in chain {
        eprintln!("  {cause}");
    }
}

/// Names end up in paths: letters, digits and `. _ + -`, not starting with
/// a punctuation sign.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// Versions and releases: letters, digits and `. _ + ~`.
pub fn is_valid_version(version: &str) -> bool {
    !version.is_empty() && version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '~'))
}

/// Subpackages a recipe can split off: `<name>.<group>`, whose archive is
/// `<name>-<version>-<release>-<group>.tar.zst`. Translations also go to
/// one subpackage per language, `<name>.<language>` (see [`is_language`]).
pub const PACKAGE_GROUPS: [&str; 5] = ["devel", "doc", "man", "service", "sources"];

/// Language codes, as in the names of locale directories: two or three
/// lowercase letters (`fr`, `pt`, `ast`).
pub fn is_language(code: &str) -> bool {
    (2..=3).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_lowercase())
}

/// Base language of a locale directory: `fr_CA`, `sr@latin` and `de.UTF-8`
/// give `fr`, `sr` and `de`. None for other names (`C`, `man1`...).
pub fn locale_language(dir: &str) -> Option<&str> {
    let end = dir.find(['_', '@', '.']).unwrap_or(dir.len());
    let (code, mut rest) = dir.split_at(end);
    if !is_language(code) {
        return None;
    }
    // Territory or script: _CA, _419, _Hans
    if let Some(after) = rest.strip_prefix('_') {
        let end = after.find(['@', '.']).unwrap_or(after.len());
        let (territory, tail) = after.split_at(end);
        let b = territory.as_bytes();
        let valid = (b.len() == 2 && b.iter().all(u8::is_ascii_uppercase))
            || (b.len() == 3 && b.iter().all(u8::is_ascii_digit))
            || (b.len() == 4 && b[0].is_ascii_uppercase() && b[1..].iter().all(u8::is_ascii_lowercase));
        if !valid {
            return None;
        }
        rest = tail;
    }
    // Then a charset (.UTF-8) or a modifier (@latin)
    (rest.is_empty() || rest.len() > 1).then_some(code)
}

/// True for the group part of a subpackage name: a known group or a
/// language.
pub fn is_package_group(group: &str) -> bool {
    PACKAGE_GROUPS.contains(&group) || is_language(group)
}

/// Splits the name of a subpackage: `nano.devel` gives (nano, devel),
/// `nano.fr` gives (nano, fr). None for other packages.
pub fn split_group(name: &str) -> Option<(&str, &str)> {
    name.rsplit_once('.').filter(|(base, group)| !base.is_empty() && is_package_group(group))
}

/// Archive file name of a package: `nano-8.0-1.tar.zst`, and for the
/// subpackages `nano.devel` and `nano.fr`, `nano-8.0-1-devel.tar.zst` and
/// `nano-8.0-1-fr.tar.zst`.
pub fn archive_file_name(name: &str, version: &str, release: &str) -> String {
    match split_group(name) {
        Some((base, group)) => format!("{base}-{version}-{release}-{group}.tar.zst"),
        None => format!("{name}-{version}-{release}.tar.zst"),
    }
}

/// Splits an archive name (the `.tar.zst` already removed) into package
/// name, version and release: `nano-8.0-1` gives (nano, 8.0, 1), and
/// `nano-8.0-1-devel` gives (nano.devel, 8.0, 1). Returns None when a part
/// is missing.
pub fn split_archive_base(base: &str) -> Option<(String, &str, &str)> {
    fn split(base: &str) -> Option<(String, &str, &str)> {
        let (rest, release) = base.rsplit_once('-')?;
        let (name, version) = rest.rsplit_once('-')?;
        (!name.is_empty()).then(|| (name.to_string(), version, release))
    }
    if let Some((rest, group)) = base.rsplit_once('-') {
        if is_package_group(group) {
            if let Some((name, version, release)) = split(rest) {
                return Some((format!("{name}.{group}"), version, release));
            }
        }
    }
    split(base)
}

/// True for the archive name of a subpackage in the former form
/// (`nano.devel-8.0-1`), which no longer matches its index record.
pub fn is_old_group_archive(base: &str) -> bool {
    match split_archive_base(base) {
        Some((name, version, release)) => {
            name.rsplit_once('.').is_some_and(|(_, g)| PACKAGE_GROUPS.contains(&g))
                && base == format!("{name}-{version}-{release}")
        }
        None => false,
    }
}

/// SHA-256 of a file, in lowercase hexadecimal.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Appends a suffix to a path: `foo` + `.part` gives `foo.part`.
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Writes a file atomically, readable by everyone: the content goes to
/// `<path>.pqtmp` first, then replaces the file in a single rename, so a
/// crash leaves either the old or the new file, never a partial one.
pub fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let staged = with_suffix(path, ".pqtmp");
    let result = (|| -> io::Result<()> {
        let mut file = fs::File::create(&staged)?;
        file.write_all(content)?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        file.sync_all()?;
        fs::rename(&staged, path)
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&staged);
        return Err(err).with_context(|| format!("unable to write {}", path.display()));
    }
    Ok(())
}

/// Path made of a root (possibly empty, meaning `/`) and bytes.
pub fn bytes_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

pub fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

/// Concatenates two byte strings into a path, like "$root$path" in shell.
pub fn concat_path(root: &[u8], path: &[u8]) -> PathBuf {
    let mut v = Vec::with_capacity(root.len() + path.len());
    v.extend_from_slice(root);
    v.extend_from_slice(path);
    PathBuf::from(OsString::from_vec(v))
}

/// `dirname` of a relative path made of bytes: "a/b" gives "a", "a" gives ".".
pub fn dirname_bytes(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) => b"/",
        Some(i) => &path[..i],
        None => b".",
    }
}

/// True if something (even a dangling symlink) exists at `path`.
pub fn exists_or_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// True for a directory, not a symlink to one.
pub fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// True for a regular file, not a symlink to one.
pub fn is_real_file(path: &Path) -> bool {
    fs::symlink_metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

/// Copies a file, a symlink or a special file with its metadata (`cp -a`).
pub fn copy_preserve(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dst)?;
        let _ = lchown(dst, meta.uid(), meta.gid());
        return Ok(());
    }
    if ft.is_file() {
        fs::copy(src, dst)?;
    } else if ft.is_dir() {
        fs::create_dir(dst)?;
    } else if ft.is_char_device() || ft.is_block_device() || ft.is_fifo() || ft.is_socket() {
        mknod(dst, meta.mode(), meta.rdev())?;
    }
    let _ = lchown(dst, meta.uid(), meta.gid());
    fs::set_permissions(dst, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    set_times(dst, meta.mtime());
    Ok(())
}

/// Renames a path, falling back to a copy when it crosses filesystems.
pub fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Err(err) if err.raw_os_error() == Some(libc::EXDEV) => {
            copy_preserve(src, dst)?;
            fs::remove_file(src)
        }
        other => other,
    }
}

fn cstring(path: &Path) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

pub fn lchown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c = cstring(path)?;
    // SAFETY: c is a valid NUL-terminated string
    if unsafe { libc::lchown(c.as_ptr(), uid, gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn mknod(path: &Path, mode: u32, dev: u64) -> io::Result<()> {
    let c = cstring(path)?;
    // SAFETY: c is a valid NUL-terminated string
    if unsafe { libc::mknod(c.as_ptr(), mode as libc::mode_t, dev as libc::dev_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sets the access and modification times of a path (not following
/// symlinks). Errors are ignored: times are cosmetic.
pub fn set_times(path: &Path, mtime: i64) {
    let Ok(c) = cstring(path) else { return };
    let t = libc::timespec { tv_sec: mtime as libc::time_t, tv_nsec: 0 };
    let times = [t, t];
    // SAFETY: c is valid and times holds two timespecs
    unsafe {
        libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW);
    }
}

pub fn euid() -> u32 {
    // SAFETY: geteuid cannot fail
    unsafe { libc::geteuid() }
}

pub fn set_umask(mask: u32) {
    // SAFETY: umask cannot fail
    unsafe {
        libc::umask(mask as libc::mode_t);
    }
}

/// Machine architecture, as printed by `uname -m`.
pub fn machine_arch() -> String {
    // SAFETY: utsname is plain data, filled by uname
    unsafe {
        let mut uts: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut uts) == 0 {
            return CStr::from_ptr(uts.machine.as_ptr()).to_string_lossy().into_owned();
        }
    }
    std::env::consts::ARCH.to_string()
}

/// Name the program was called with, for messages ("paquet", "pq"...).
pub fn program_name(default: &str) -> String {
    std::env::args_os()
        .next()
        .and_then(|a| Path::new(&a).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| default.to_string())
}

/// Takes an exclusive, non-blocking lock on a file. The lock lasts as long
/// as the returned file stays open; Ok(None) means another process holds it.
pub fn try_lock(path: &Path) -> io::Result<Option<fs::File>> {
    use std::os::fd::AsRawFd;
    let file = fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    // SAFETY: the descriptor is valid while file lives
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(err);
    }
    Ok(Some(file))
}

/// Asks a yes/no question. An empty answer, or no answer at all (end of
/// input), takes `default`.
pub fn ask(question: &str, default: bool) -> bool {
    print!("{question} ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    match io::stdin().lock().read_line(&mut answer) {
        Ok(0) | Err(_) => {
            println!();
            default
        }
        Ok(_) => match answer.trim_end_matches(['\n', '\r']) {
            "" => default,
            "y" | "Y" => true,
            _ => false,
        },
    }
}

/// Disk usage of files in bytes (allocated blocks, like `du`).
pub fn disk_usage(paths: &[PathBuf]) -> u64 {
    paths.iter().filter_map(|p| fs::symlink_metadata(p).ok()).map(|m| m.blocks() * 512).sum()
}

/// Size in a human readable form, rounded up like `du -h`: 0, 512, 4.0K,
/// 12M...
pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        return bytes.to_string();
    }
    let units = ["K", "M", "G", "T", "P", "E"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        let tenths = (value * 10.0).ceil() / 10.0;
        if tenths < 10.0 {
            return format!("{tenths:.1}{}", units[unit]);
        }
    }
    let rounded = value.ceil();
    if rounded >= 1024.0 && unit + 1 < units.len() {
        return format!("1.0{}", units[unit + 1]);
    }
    format!("{rounded:.0}{}", units[unit])
}

/// Files of a directory whose name ends with `suffix`, sorted by name.
pub fn files_with_suffix(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().map(|n| n.as_bytes().ends_with(suffix.as_bytes())).unwrap_or(false)
                && fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
        })
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_versions() {
        assert!(is_valid_name("nano"));
        assert!(is_valid_name("gtk+3.devel"));
        assert!(is_valid_name("lib-foo_2"));
        assert!(!is_valid_name("-foo"));
        assert!(!is_valid_name(".foo"));
        assert!(!is_valid_name("foo/bar"));
        assert!(!is_valid_name(""));
        assert!(is_valid_version("1.2~rc1+git_3"));
        assert!(!is_valid_version("1-2"));
        assert!(!is_valid_version(""));
    }

    #[test]
    fn archive_names() {
        let split = |b| split_archive_base(b).map(|(n, v, r)| (n, v.to_string(), r.to_string()));
        let parts = |n: &str, v: &str, r: &str| Some((n.to_string(), v.to_string(), r.to_string()));
        assert_eq!(split("nano-8.0-1"), parts("nano", "8.0", "1"));
        assert_eq!(split("gtk-doc-1.33-2"), parts("gtk-doc", "1.33", "2"));
        assert_eq!(split("nano-8.0-1-devel"), parts("nano.devel", "8.0", "1"));
        assert_eq!(split("gtk-doc-1.33-2-sources"), parts("gtk-doc.sources", "1.33", "2"));
        // Without a release, -devel is the release
        assert_eq!(split("foo-1.0-devel"), parts("foo", "1.0", "devel"));
        assert_eq!(split("nano-8.0"), None);
        assert_eq!(split("-8.0-1"), None);
        assert_eq!(archive_file_name("nano", "8.0", "1"), "nano-8.0-1.tar.zst");
        assert_eq!(archive_file_name("nano.devel", "8.0", "1"), "nano-8.0-1-devel.tar.zst");
        assert_eq!(archive_file_name("python3.11", "3.11.9", "1"), "python3.11-3.11.9-1.tar.zst");
        assert_eq!(split("nano-8.0-1-fr"), parts("nano.fr", "8.0", "1"));
        assert_eq!(archive_file_name("nano.fr", "8.0", "1"), "nano-8.0-1-fr.tar.zst");
        assert_eq!(split("hunspell-fr-1.0-1"), parts("hunspell-fr", "1.0", "1"));
        assert!(is_old_group_archive("nano.devel-8.0-1"));
        assert!(!is_old_group_archive("nano-8.0-1-devel"));
        assert!(!is_old_group_archive("nano-8.0-1"));
    }

    #[test]
    fn languages() {
        for (dir, lang) in [
            ("fr", "fr"),
            ("fr_CA", "fr"),
            ("sr@latin", "sr"),
            ("de.UTF-8", "de"),
            ("ast", "ast"),
            ("es_419", "es"),
            ("zh_Hans", "zh"),
            ("ca_ES@valencia", "ca"),
        ] {
            assert_eq!(locale_language(dir), Some(lang), "{dir}");
        }
        for dir in
            ["C", "POSIX", "man1", "locale.alias", "all_languages", "kf5_all_languages", "f", "fr_", "fr_ca", "French"]
        {
            assert_eq!(locale_language(dir), None, "{dir}");
        }
        assert_eq!(split_group("nano.fr"), Some(("nano", "fr")));
        assert_eq!(split_group("nano.devel"), Some(("nano", "devel")));
        assert_eq!(split_group("python3.11"), None);
        assert_eq!(split_group(".fr"), None);
    }

    #[test]
    fn dirnames() {
        assert_eq!(dirname_bytes(b"usr/bin/nano"), b"usr/bin");
        assert_eq!(dirname_bytes(b"usr"), b".");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0");
        assert_eq!(human_size(4096), "4.0K");
        assert_eq!(human_size(4097), "4.1K");
        assert_eq!(human_size(12 * 1024 * 1024), "12M");
        assert_eq!(human_size(10 * 1024 - 1), "10K");
    }
}
