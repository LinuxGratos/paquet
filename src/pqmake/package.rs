//! Packaging a built recipe: subpackages, runtime dependencies, archives.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::Pqmake;
use super::build::move_tree;
use crate::{archive, util};

/// Paths (relative to the package root) that belong to a group.
pub fn group_paths(group: &str, root: &Path) -> Vec<String> {
    let existing = |paths: &[&str]| -> Vec<String> {
        paths.iter().filter(|p| util::exists_or_symlink(&root.join(p))).map(|p| p.to_string()).collect()
    };
    match group {
        "devel" => {
            let mut paths = existing(&[
                "usr/include",
                "usr/lib/pkgconfig",
                "usr/share/pkgconfig",
                "usr/lib/cmake",
                "usr/share/aclocal",
            ]);
            // Static libraries, libtool files and the unversioned .so links
            let mut libs: Vec<String> = fs::read_dir(root.join("usr/lib"))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let is_link = e.file_type().map(|t| t.is_symlink()).unwrap_or(false);
                    (name.ends_with(".a") || name.ends_with(".la") || (name.ends_with(".so") && is_link))
                        .then(|| format!("usr/lib/{name}"))
                })
                .collect();
            libs.sort();
            paths.extend(libs);
            paths
        }
        "doc" => existing(&["usr/share/doc", "usr/share/gtk-doc", "usr/share/info"]),
        "man" => existing(&["usr/share/man"]),
        "service" => existing(&["etc/rc.d", "lib/systemd/system", "usr/lib/systemd/system"]),
        "sources" => existing(&["usr/src"]),
        "locale" => existing(&["usr/share/locale"]),
        _ => Vec::new(),
    }
}

/// Moves a group's files from `root` to `dest`, then removes the directories
/// the move left empty.
pub fn split_group(group: &str, root: &Path, dest: &Path) -> io::Result<()> {
    for path in group_paths(group, root) {
        let target = dest.join(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        move_tree(&root.join(&path), &target)?;
        let mut parent = Path::new(&path).parent();
        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
            if fs::remove_dir(root.join(p)).is_err() {
                break;
            }
            parent = p.parent();
        }
    }
    Ok(())
}

/// Words of a list without repetitions, in order.
pub fn unique_words<'a>(words: impl IntoIterator<Item = &'a str>) -> String {
    let mut out: Vec<&str> = Vec::new();
    for word in words {
        if !out.contains(&word) {
            out.push(word);
        }
    }
    out.join(" ")
}

/// Files (not directories) of a tree, relative to it.
fn tree_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for entry in fs::read_dir(root.join(&rel)).into_iter().flatten().flatten() {
            let rel = rel.join(entry.file_name());
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(rel);
            } else {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

/// Shared libraries the ELF files of a tree need (their NEEDED entries):
/// executables and `*.so*` files, like `readelf -d`.
pub fn needed_libraries(root: &Path) -> BTreeSet<String> {
    let mut needed = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !meta.is_file() || (meta.mode() & 0o100 == 0 && !name.contains(".so")) {
                continue;
            }
            if let Some(libs) = elf_needed(&path) {
                needed.extend(libs);
            }
        }
    }
    needed
}

fn elf_needed(path: &Path) -> Option<Vec<String>> {
    let mut magic = [0u8; 4];
    fs::File::open(path).ok()?.read_exact(&mut magic).ok()?;
    if magic != *b"\x7fELF" {
        return None;
    }
    let data = fs::read(path).ok()?;
    let elf = goblin::elf::Elf::parse(&data).ok()?;
    Some(elf.libraries.iter().map(|l| l.to_string()).collect())
}

/// True for a library directory: lib, lib32, lib64, usr/lib..., and their
/// direct subdirectories.
fn is_library_dir(dir: &str) -> bool {
    let dir = dir.strip_prefix("usr/").unwrap_or(dir);
    let Some(rest) = dir.strip_prefix("lib") else { return false };
    let rest = rest.strip_prefix("32").or_else(|| rest.strip_prefix("64")).unwrap_or(rest);
    match rest.strip_prefix('/') {
        None => rest.is_empty(),
        Some(sub) => !sub.is_empty() && !sub.contains('/'),
    }
}

/// For each library, the package holding it in a library directory, or
/// `?<library>` when none does. The first package listed in `files` wins.
pub fn providers_of(files: &[(String, String)], needed: &BTreeSet<String>) -> Vec<String> {
    let mut provider: HashMap<&str, &str> = HashMap::new();
    for (package, path) in files {
        let (dir, base) = path.rsplit_once('/').unwrap_or(("", path));
        if is_library_dir(dir) {
            provider.entry(base).or_insert(package);
        }
    }
    needed
        .iter()
        .map(|lib| match provider.get(lib.as_str()) {
            Some(p) => p.to_string(),
            None => format!("?{lib}"),
        })
        .collect()
}

impl Pqmake {
    /// Files of the repository's archives, as (package, path). The listing
    /// of each archive is cached in `WORK_DIR/.lists`.
    fn repo_files(&self) -> Result<Vec<(String, String)>> {
        let lists = self.s.work_dir.join(".lists");
        fs::create_dir_all(&lists)?;
        let mut archives = Vec::new();
        for set in fs::read_dir(&self.s.repo_dir).into_iter().flatten().flatten() {
            if !set.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            archives.extend(util::files_with_suffix(&set.path(), ".tar.zst"));
        }
        archives.sort_by(|a, b| util::path_bytes(a).cmp(util::path_bytes(b)));

        let mut out = Vec::new();
        for path in archives {
            let base = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let list = lists.join(format!("{base}.list"));
            let stale = match (fs::metadata(&list), fs::metadata(&path)) {
                (Ok(l), Ok(a)) => a.modified().ok() > l.modified().ok(),
                _ => true,
            };
            if stale {
                let Ok(entries) = archive::list(&path) else { continue };
                let mut content = String::new();
                for e in entries {
                    content.push_str(&String::from_utf8_lossy(&e.name));
                    if e.is_dir {
                        content.push('/');
                    }
                    content.push('\n');
                }
                let part = util::with_suffix(&list, ".part");
                if fs::write(&part, content).and_then(|_| fs::rename(&part, &list)).is_err() {
                    let _ = fs::remove_file(&part);
                    continue;
                }
            }
            let package = base
                .strip_suffix(".tar.zst")
                .and_then(util::split_archive_base)
                .map(|(n, _, _)| n.to_string())
                .unwrap_or_default();
            let content = fs::read_to_string(&list).unwrap_or_default();
            for line in content.lines() {
                let line = line.strip_prefix("./").unwrap_or(line);
                out.push((package.clone(), line.to_string()));
            }
        }
        Ok(out)
    }

    /// Files of the packages installed in the build chroot, from paquet's
    /// lists.
    fn chroot_files(&self, root: Option<&Path>) -> Vec<(String, String)> {
        let Some(root) = root else { return Vec::new() };
        let mut out = Vec::new();
        for list in util::files_with_suffix(&root.join("var/lib/pq/files"), "") {
            let package = list.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let content = fs::read_to_string(&list).unwrap_or_default();
            for (i, line) in content.lines().enumerate() {
                if i == 0 && line.starts_with("# root:") {
                    continue;
                }
                out.push((package.clone(), line.strip_prefix('/').unwrap_or(line).to_string()));
            }
        }
        out
    }

    /// Splits the groups off `pkg`, finds the runtime dependencies of each
    /// package and writes the archives, the main one last.
    pub fn package_recipe(&mut self, name: &str, work: &Path, pkg: &Path, root: Option<&Path>) -> Result<()> {
        let r = self.recipe(name).clone();
        let mut packages: Vec<String> = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut depends: Vec<Vec<String>> = Vec::new();
        for group in &r.groups {
            let dest = work.join(group);
            fs::create_dir_all(&dest)?;
            split_group(group, pkg, &dest).with_context(|| format!("unable to split the {group} files"))?;
            if fs::read_dir(&dest)?.next().is_none() {
                continue;
            }
            packages.push(format!("{name}.{group}"));
            dirs.push(dest);
            depends.push(match group.as_str() {
                "devel" => std::iter::once(name.to_string()).chain(r.run_devel.iter().cloned()).collect(),
                "service" => vec![name.to_string()],
                _ => Vec::new(),
            });
        }
        packages.push(name.to_string());
        dirs.push(pkg.to_path_buf());
        depends.push(r.run.clone());

        // Runtime dependencies: the packages holding the libraries the ELF
        // files need, looked up in this recipe's packages, then in the build
        // chroot, then in the repository
        if r.no_rtdeps {
            println!("Runtime dependencies not detected (PKGMK_IGNORE_RUNTIMEDEPS)");
        } else {
            let mut files: Vec<(String, String)> = Vec::new();
            for (package, dir) in packages.iter().zip(&dirs) {
                files.extend(tree_files(dir).into_iter().map(|f| (package.clone(), f)));
            }
            files.extend(self.chroot_files(root));
            files.extend(self.repo_files()?);
            let mut unknown: Vec<String> = Vec::new();
            for (i, dir) in dirs.iter().enumerate() {
                for dep in providers_of(&files, &needed_libraries(dir)) {
                    if let Some(lib) = dep.strip_prefix('?') {
                        if !unknown.iter().any(|u| u == lib) {
                            unknown.push(lib.to_string());
                        }
                    } else if dep != packages[i] {
                        depends[i].push(dep);
                    }
                }
            }
            if !unknown.is_empty() {
                println!("Libraries provided by no package (ignored): {}", unknown.join(" "));
            }
        }

        for (i, dir) in dirs.iter().enumerate() {
            let group = if packages[i] == name { "" } else { packages[i].rsplit('.').next().unwrap_or("") };
            let description =
                if group.is_empty() { r.description.clone() } else { format!("{} ({group})", r.description) };
            let deps = unique_words(depends[i].iter().map(String::as_str));
            self.write_archive(name, &packages[i], dir, group, &description, &deps)?;
        }
        // A subpackage this build no longer produces must not stay next to
        // the new main package (rebuild of the same version)
        for group in super::KNOWN_GROUPS {
            let pkgname = format!("{name}.{group}");
            let stale = self.archive_path(name, &pkgname);
            if !packages.contains(&pkgname) && stale.is_file() {
                fs::remove_file(&stale).with_context(|| format!("unable to remove {}", stale.display()))?;
                println!("  {} removed (no longer produced)", stale.display());
            }
        }
        self.index_dirty = true;
        Ok(())
    }

    fn write_archive(
        &self,
        name: &str,
        pkgname: &str,
        root: &Path,
        groups: &str,
        description: &str,
        depends: &str,
    ) -> Result<()> {
        let archive_path = self.archive_path(name, pkgname);
        let mut meta = format!("Description: {description}\nGroups: {groups}\nSet: {}\n", self.recipe_set[name]);
        if !depends.is_empty() {
            meta.push_str(&format!("Depends: {depends}\n"));
        }
        fs::write(root.join(".META"), meta)?;
        fs::set_permissions(root.join(".META"), fs::Permissions::from_mode(0o644))?;
        // The scripts of the recipe (pre-install, post-install, pre-remove,
        // post-remove) go with its main package
        if pkgname == name {
            let recipe_dir = self.recipe_path[name].parent().unwrap_or(Path::new("."));
            for (script, entry) in archive::SCRIPTS {
                if recipe_dir.join(script).is_file() {
                    fs::copy(recipe_dir.join(script), root.join(entry))
                        .with_context(|| format!("unable to add the {script} script"))?;
                    fs::set_permissions(root.join(entry), fs::Permissions::from_mode(0o644))?;
                    println!("  with its {script} script");
                }
            }
        }
        if let Some(dir) = archive_path.parent() {
            fs::create_dir_all(dir)?;
        }
        // On the host, files belong to root in the archive, whoever built
        // them; in a chroot, build() ran as root and set the ownership itself
        let part = util::with_suffix(&archive_path, ".part");
        if let Err(err) = archive::create(root, &part, self.s.chroot.is_none()) {
            let _ = fs::remove_file(&part);
            return Err(err);
        }
        fs::rename(&part, &archive_path)?;
        println!("  {}", archive_path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_dirs() {
        for dir in ["lib", "lib64", "usr/lib", "usr/lib32", "usr/lib/x86_64-linux-gnu"] {
            assert!(is_library_dir(dir), "{dir}");
        }
        for dir in ["usr/libexec", "usr/lib/a/b", "usr/bin", "", "usr/lib/"] {
            assert!(!is_library_dir(dir), "{dir}");
        }
    }

    #[test]
    fn providers() {
        let files = vec![
            ("foo".to_string(), "usr/lib/libfoo.so.1".to_string()),
            ("bar".to_string(), "usr/lib/libfoo.so.1".to_string()),
            ("bar".to_string(), "usr/share/libbar.so.2".to_string()),
        ];
        let needed: BTreeSet<String> = ["libfoo.so.1", "libbar.so.2"].iter().map(|s| s.to_string()).collect();
        assert_eq!(providers_of(&files, &needed), ["?libbar.so.2", "foo"]);
    }

    #[test]
    fn words() {
        assert_eq!(unique_words(["a", "b", "a", "c"]), "a b c");
    }
}
