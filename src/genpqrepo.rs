//! `genpqrepo`: generates the `PQREPO` index of a mirror tree.
//!
//! ```text
//! genpqrepo [-k <private_key>] <repo_dir> [folder]
//! genpqrepo --genkey <name>
//! ```
//!
//! `repo_dir` holds the packages of one branch and architecture (the
//! `$MIRROR/$BRANCH/$ARCH` directory `paquet` downloads from); with `folder`,
//! only the entries of that sub-folder are refreshed.
//!
//! With a key (`-k`, or `$PQ_SIGN_KEY`), the index gets a detached Ed25519
//! signature, `PQREPO.sig`; `paquet --sync` refuses an index without a valid
//! signature. Without a key, the index is left unsigned (with a warning) and
//! any stale `PQREPO.sig` is removed.
//!
//! The index is written to `$OUTPUT_FILE` (default: `<repo_dir>/PQREPO`) with
//! one record per archive (see [`crate::record`]). Archives must be named
//! `<name>-<version>-<release>.tar.zst` and contain a `.META` file with
//! `Description:`, `Groups:` and `Set:` lines, plus optional `Depends:` lines
//! listing runtime dependencies separated by spaces or commas (long lists
//! may be split over several lines). When several versions of a package are
//! present, only the highest one (as sorted by `sort -V`) is indexed and the
//! others are reported.

use std::collections::HashMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::record::Record;
use crate::util::{self, is_valid_name, is_valid_version, sha256_file};
use crate::{archive, sign, version};

/// Fields of an archive's `.META` file, as stored in the index.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub description: String,
    pub groups: String,
    pub set: String,
    pub depends: String,
}

impl Meta {
    pub fn parse(meta: &str) -> Meta {
        let first = |key: &str| {
            meta.lines()
                .find_map(|l| l.strip_prefix(key))
                .map(|v| v.trim_start_matches(' ').replace(';', ","))
                .unwrap_or_default()
        };
        let depends: Vec<&str> = meta
            .lines()
            .filter_map(|l| l.strip_prefix("Depends:"))
            .flat_map(|v| v.split([' ', ',', ';', '\t']))
            .filter(|w| !w.is_empty())
            .collect();
        Meta {
            description: first("Description:"),
            groups: first("Groups:"),
            set: first("Set:"),
            depends: depends.join(" "),
        }
    }
}

/// `*.tar.zst` files under a directory, sorted by path.
fn find_archives(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && path.as_os_str().as_bytes().ends_with(b".tar.zst") {
                found.push(path);
            }
        }
    }
    found.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    found
}

/// Index record of an archive, or None (with a warning) when its name is
/// not usable.
pub fn archive_record(repo_dir: &Path, path: &Path) -> Result<Option<Record>> {
    let file = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let base = file.strip_suffix(".tar.zst").unwrap_or(&file);
    if util::split_archive_base(base).is_none() {
        eprintln!("Warning: skipping {} (expected <name>-<version>-<release>.tar.zst)", path.display());
        return Ok(None);
    }
    let meta = archive::read_meta(path)?.unwrap_or_default();
    if meta.trim().is_empty() {
        eprintln!("Warning: no .META found in {}", path.display());
    }
    let meta = Meta::parse(&meta);
    let group = meta.groups.trim();
    let Some((name, version, release)) = util::split_archive_name(base, group) else {
        eprintln!("Warning: skipping {} (expected <name>-<version>-<release>.tar.zst)", path.display());
        return Ok(None);
    };
    // paquet finds the archive from the record: its name must follow
    let expected = util::archive_file_name(&name, version, release, group);
    if expected != file {
        eprintln!("Warning: skipping {} (the archive of {name} must be named {expected})", path.display());
        return Ok(None);
    }
    let name = name.as_str();
    if !is_valid_name(name) || !is_valid_version(version) || !is_valid_version(release) {
        eprintln!(
            "Warning: skipping {} (name may only use letters, digits and . _ + -, \
             versions letters, digits and . _ + ~)",
            path.display()
        );
        return Ok(None);
    }
    let folder = path
        .parent()
        .and_then(|p| p.strip_prefix(repo_dir).ok())
        .map(|p| p.to_string_lossy().trim_start_matches('/').to_string())
        .unwrap_or_default();

    let checksum = sha256_file(path).with_context(|| format!("unable to read {}", path.display()))?;
    // Archive modification time, so unchanged packages keep their timestamp
    let timestamp = fs::metadata(path)?.mtime();
    Ok(Some(Record {
        checksum,
        timestamp: timestamp.to_string(),
        folder,
        name: name.to_string(),
        version: version.to_string(),
        release: release.to_string(),
        description: meta.description,
        groups: meta.groups,
        set: meta.set,
        // The status is set by paquet
        status: String::new(),
        depends: meta.depends,
    }))
}

/// Keeps a single record per package name: the highest
/// `<version>-<release>`. The others are reported and dropped. The order of
/// first appearance is kept.
pub fn keep_newest_versions(records: Vec<Record>) -> Vec<Record> {
    let mut order: Vec<String> = Vec::new();
    let mut best: HashMap<String, Record> = HashMap::new();
    for record in records {
        let candidate = format!("{}-{}", record.version, record.release);
        match best.get(&record.name) {
            None => order.push(record.name.clone()),
            Some(current) => {
                let current_version = format!("{}-{}", current.version, current.release);
                if version::compare(&candidate, &current_version) != std::cmp::Ordering::Greater {
                    eprintln!(
                        "Warning: {} {candidate} ignored, {} {current_version} is newer or identical",
                        record.name, record.name
                    );
                    continue;
                }
                eprintln!("Warning: {} {current_version} ignored, {} {candidate} is newer", record.name, record.name);
            }
        }
        best.insert(record.name.clone(), record);
    }
    order.into_iter().filter_map(|name| best.remove(&name)).collect()
}

/// Generates the index of `repo_dir` into `output`, refreshing only
/// `folder` when given, and signs it with `sign_key`.
pub fn generate(repo_dir: &Path, output: &Path, folder: Option<&str>, sign_key: Option<&Path>) -> Result<()> {
    generate_impl(repo_dir, output, folder, sign_key, true)
}

/// Same as [`generate`] for the whole tree, without the progress messages
/// nor the warning about a missing key (used by pqmake for its own builds).
pub fn generate_quiet(repo_dir: &Path, output: &Path, sign_key: Option<&Path>) -> Result<()> {
    generate_impl(repo_dir, output, None, sign_key, false)
}

fn generate_impl(
    repo_dir: &Path,
    output: &Path,
    folder: Option<&str>,
    sign_key: Option<&Path>,
    verbose: bool,
) -> Result<()> {
    let folder = folder.map(|f| f.trim_end_matches('/')).filter(|f| !f.is_empty());
    let key = sign_key.map(sign::load_signing_key).transpose()?;

    // Lines of the other folders are kept when only one folder is refreshed
    let mut lines: Vec<String> = Vec::new();
    if let Some(dir) = folder {
        if let Ok(content) = fs::read(output) {
            let prefix = format!("{dir}/");
            for line in String::from_utf8_lossy(&content).lines() {
                let f = line.split(';').nth(2).unwrap_or("");
                if f != dir && !f.starts_with(&prefix) {
                    lines.push(line.to_string());
                }
            }
        }
    }
    let mut records: Vec<Record> = lines.iter().map(|l| Record::parse(l)).collect();

    let scan = match folder {
        Some(dir) => repo_dir.join(dir),
        None => repo_dir.to_path_buf(),
    };
    for path in find_archives(&scan) {
        if let Some(record) = archive_record(repo_dir, &path)? {
            records.push(record);
        }
    }

    // Only the newest version of each package is published
    let mut content = String::new();
    for record in keep_newest_versions(records) {
        content.push_str(&record.line());
        content.push('\n');
    }

    // Sign before publishing anything, so a signing error leaves the
    // previous index and signature in place
    let signature = key.as_ref().map(|k| sign::sign(k, content.as_bytes()));
    let sig_path = util::with_suffix(output, ".sig");
    util::atomic_write(output, content.as_bytes())?;
    if verbose {
        println!("PQREPO file generated successfully at {}", output.display());
    }
    match signature {
        Some(sig) => {
            util::atomic_write(&sig_path, &sig)?;
            if verbose {
                println!("PQREPO signed: {}", sig_path.display());
            }
        }
        None => {
            let _ = fs::remove_file(&sig_path);
            if verbose {
                eprintln!(
                    "Warning: the index is not signed (no -k or PQ_SIGN_KEY); \
                     paquet --sync will refuse it unless run with --nosign"
                );
            }
        }
    }
    Ok(())
}

fn usage(prog: &str) {
    println!("Usage: {prog} [-k <private_key>] <repo_dir> [folder]");
    println!("       {prog} --genkey <name>");
}

/// Entry point of the `genpqrepo` command.
pub fn main() -> i32 {
    let prog = util::program_name("genpqrepo");
    let args: Vec<String> = std::env::args().skip(1).collect();
    crate::run_main(|| run(&prog, &args))
}

fn run(prog: &str, args: &[String]) -> Result<()> {
    let mut args = args.iter().map(String::as_str);
    let mut sign_key: Option<PathBuf> = std::env::var_os("PQ_SIGN_KEY").filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut first = args.next();
    match first {
        None | Some("" | "-h" | "--help") => {
            usage(prog);
            return Err(crate::Reported.into());
        }
        Some("--genkey") => {
            let Some(name) = args.next().filter(|n| !n.is_empty()) else {
                usage(prog);
                return Err(crate::Reported.into());
            };
            let (key, public) = sign::generate_key(name)?;
            println!("Private key: {} (keep it secret, use it with -k or PQ_SIGN_KEY)", key.display());
            println!("Public key:  {} (install it in /etc/pq/keys on the clients)", public.display());
            return Ok(());
        }
        Some("-k" | "--key") => {
            let Some(key) = args.next().filter(|k| !k.is_empty()) else {
                usage(prog);
                return Err(crate::Reported.into());
            };
            sign_key = Some(key.into());
            first = args.next();
        }
        _ => {}
    }
    let Some(repo_dir) = first.filter(|d| !d.is_empty()) else {
        usage(prog);
        return Err(crate::Reported.into());
    };
    let folder = args.next().filter(|f| !f.is_empty());
    if let Some(key) = &sign_key {
        if fs::File::open(key).is_err() {
            bail!("signing key {} not found or not readable.", key.display());
        }
    }
    let repo_dir = PathBuf::from(repo_dir.trim_end_matches('/'));
    let repo_dir = if repo_dir.as_os_str().is_empty() { PathBuf::from("/") } else { repo_dir };
    let scan = repo_dir.join(folder.unwrap_or(""));
    if !scan.is_dir() {
        bail!("directory {} not found.", scan.display());
    }
    let output = std::env::var_os("OUTPUT_FILE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_dir.join("PQREPO"));
    generate(&repo_dir, &output, folder, sign_key.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_fields() {
        let meta = Meta::parse(
            "Description: A text editor; small\nGroups: \nSet: base\nDepends: ncurses, libc\nDepends:\tfoo>=1.2;bar\n",
        );
        assert_eq!(meta.description, "A text editor, small");
        assert_eq!(meta.groups, "");
        assert_eq!(meta.set, "base");
        assert_eq!(meta.depends, "ncurses libc foo>=1.2 bar");
    }

    #[test]
    fn newest_versions() {
        let rec = |name: &str, v: &str| Record::parse(&format!("x;1;f;{name};{v};1;;;;;"));
        let kept = keep_newest_versions(vec![rec("a", "1.9"), rec("b", "1"), rec("a", "1.10"), rec("a", "1.2")]);
        let names: Vec<String> = kept.iter().map(|r| format!("{}-{}", r.name, r.version)).collect();
        assert_eq!(names, ["a-1.10", "b-1"]);
    }
}
