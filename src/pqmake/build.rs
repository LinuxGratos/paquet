//! Building a recipe: sources, build chroot and build steps.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

use super::{Pqmake, recipe_build_script};
use crate::{fetch, util};

/// Paths inside the build chroot.
pub const CH_DIR: &str = "/pqmake";
/// Build style of the recipe, copied into the recipe directory of the build.
pub const STYLE_FILE: &str = ".pqmake-style";
pub const CH_MIRROR: &str = "/pqmake/mirror";

/// Output of a build: written to its log file, and to the terminal unless
/// quiet.
pub struct Log {
    file: fs::File,
    quiet: bool,
}

impl Log {
    fn create(path: &Path, quiet: bool) -> Result<Log> {
        let file = fs::File::create(path).with_context(|| format!("unable to create {}", path.display()))?;
        Ok(Log { file, quiet })
    }

    fn write(&mut self, data: &[u8]) {
        let _ = self.file.write_all(data);
        if !self.quiet {
            let mut out = io::stdout().lock();
            let _ = out.write_all(data);
            let _ = out.flush();
        }
    }

    pub fn line(&mut self, line: &str) {
        self.write(format!("{line}\n").as_bytes());
    }

    /// Runs a command with its output (stdout and stderr) going to the log.
    /// Returns whether it succeeded.
    pub fn run(&mut self, mut cmd: Command) -> Result<bool> {
        let (mut reader, writer) = io::pipe()?;
        cmd.stdin(Stdio::null()).stdout(writer.try_clone()?).stderr(writer);
        let program = cmd.get_program().to_string_lossy().into_owned();
        let mut child = cmd.spawn().with_context(|| format!("unable to run {program}"))?;
        // Our copies of the pipe must be closed for the output to end
        drop(cmd);
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.write(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(child.wait()?.success())
    }
}

/// Last lines of a file.
fn tail(path: &Path, lines: usize) -> String {
    let content = fs::read(path).unwrap_or_default();
    let text = String::from_utf8_lossy(&content);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Removes a work directory. Work directories may hold mount points: never
/// cross into them.
pub fn remove_work(dir: &Path) -> Result<()> {
    let mounts = fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let needle = format!(" {}/", dir.display());
    if mounts.contains(&needle) {
        bail!("{} still holds mount points; not removing it.", dir.display());
    }
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("unable to remove {}", dir.display()))
        }
        _ => Ok(()),
    }
}

/// Copies a directory tree with its metadata (`cp -R` keeping modes).
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if !meta.is_dir() {
        return util::copy_preserve(src, dst);
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
    }
    fs::set_permissions(dst, meta.permissions())?;
    Ok(())
}

/// Moves a file or a tree, copying it when it crosses filesystems (from the
/// build overlay to the work directory).
pub fn move_tree(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_tree(src, dst)?;
            if util::is_real_dir(src) { fs::remove_dir_all(src) } else { fs::remove_file(src) }
        }
        other => other,
    }
}

impl Pqmake {
    /// The `paquet` program installed in the build chroot: `$PQMAKE_PAQUET`,
    /// or the `paquet` next to this program. It runs inside the chroot, so it
    /// must be compatible with its C library (or statically linked).
    fn paquet_program(&self) -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("PQMAKE_PAQUET").filter(|v| !v.is_empty()) {
            return Ok(path.into());
        }
        let exe = std::env::current_exe()?;
        let candidate = exe.with_file_name("paquet");
        if candidate.is_file() {
            return Ok(candidate);
        }
        bail!("paquet not found next to {} (set PQMAKE_PAQUET)", exe.display())
    }

    fn do_mount(&mut self, args: &[&str], target: &Path) -> Result<()> {
        fs::create_dir_all(target)?;
        let status = Command::new("mount").args(args).arg(target).status().context("unable to run mount")?;
        if !status.success() {
            bail!("unable to mount {}", target.display());
        }
        self.mounts.push(target.to_path_buf());
        Ok(())
    }

    /// Unmounts the mount points of the current build, last first.
    pub fn unmount_all(&mut self) -> Result<()> {
        let mut ok = true;
        while let Some(target) = self.mounts.pop() {
            let unmounted = Command::new("umount").arg("--").arg(&target).status().is_ok_and(|s| s.success())
                || Command::new("umount").arg("-l").arg("--").arg(&target).status().is_ok_and(|s| s.success());
            ok &= unmounted;
        }
        if ok { Ok(()) } else { Err(anyhow!("unable to unmount the build chroot")) }
    }

    /// Throwaway build root: an overlay of the chroot under `<work>/root`,
    /// with /proc, /dev and the repository (read-only) mounted inside.
    fn setup_chroot(&mut self, work: &Path) -> Result<()> {
        let chroot = self.s.chroot.clone().expect("chroot build");
        let root = work.join("root");
        for dir in [work.join("upper"), work.join("overlay-work"), root.clone()] {
            fs::create_dir_all(&dir)?;
        }
        let options = format!(
            "lowerdir={},upperdir={},workdir={}",
            chroot.display(),
            work.join("upper").display(),
            work.join("overlay-work").display()
        );
        self.do_mount(&["-t", "overlay", "overlay", "-o", &options], &root)?;
        self.do_mount(&["-t", "proc", "proc"], &root.join("proc"))?;
        self.do_mount(&["--bind", "/dev"], &root.join("dev"))?;
        fs::create_dir_all(&self.s.repo_dir)?;
        let mirror = root.join(format!("{}/stable/{}", &CH_MIRROR[1..], self.arch));
        let repo = self.s.repo_dir.display().to_string();
        self.do_mount(&["--bind", &repo], &mirror)?;
        let status = Command::new("mount").args(["-o", "remount,bind,ro"]).arg(&mirror).status()?;
        if !status.success() {
            bail!("unable to make {} read-only", mirror.display());
        }
        // The overlay is thrown away: its paquet configuration can be
        // replaced
        fs::write(root.join("etc/pq.conf"), format!("MIRROR=\"file://{CH_MIRROR}\"\nBRANCH=\"stable\"\n"))?;
        let ch_dir = root.join(&CH_DIR[1..]);
        fs::create_dir_all(&ch_dir)?;
        let paquet = ch_dir.join("paquet");
        fs::copy(self.paquet_program()?, &paquet)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&paquet, fs::Permissions::from_mode(0o755))?;
        for dir in [ch_dir.join("src"), ch_dir.join("pkg"), root.join("tmp")] {
            fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    /// A command run in the build chroot with a clean environment.
    fn in_chroot(&self, root: &Path) -> Command {
        let mut cmd = Command::new("chroot");
        cmd.arg(root).arg("/bin/bash").env_clear();
        cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .env("HOME", "/root")
            .env("TERM", std::env::var("TERM").unwrap_or_else(|_| "dumb".into()))
            .env("PKG", format!("{CH_DIR}/pkg"))
            .env("SRC", format!("{CH_DIR}/src"))
            .env("PKGMK_ROOT", format!("{CH_DIR}/recipe"))
            .env("PKGMK_ARCH", &self.arch)
            .env("PQMAKE_GROUPS", &self.s.default_groups)
            .env("MAKEFLAGS", &self.s.makeflags)
            .env("CFLAGS", &self.s.cflags)
            .env("CXXFLAGS", &self.s.cxxflags);
        cmd
    }

    /// Packages to install in the chroot before building a recipe: its
    /// makedepends that have a recipe (a bare name also brings its .devel
    /// subpackage, when there is one). paquet adds their runtime
    /// dependencies.
    fn chroot_packages(&self, name: &str) -> Vec<String> {
        let mut packages = Vec::new();
        for dep in &self.recipe(name).makedepends {
            let dep = crate::record::dep_name(dep);
            let Some(recipe) = self.dep_recipe(dep) else { continue };
            packages.push(dep.to_string());
            let devel = format!("{recipe}.devel");
            if dep == recipe && self.archive_path(&recipe, &devel).is_file() {
                packages.push(devel);
            }
        }
        packages
    }

    fn install_makedepends(&mut self, name: &str, root: &Path, log: &mut Log) -> Result<()> {
        let packages = self.chroot_packages(name);
        if packages.is_empty() {
            return Ok(());
        }
        if self.index_dirty {
            // The index is the one being built here, from local archives: it
            // is trusted as is, signed or not
            let key = std::env::var_os("PQ_SIGN_KEY").filter(|v| !v.is_empty()).map(PathBuf::from);
            let repo = self.s.repo_dir.clone();
            crate::genpqrepo::generate_quiet(&repo, &repo.join("PQREPO"), key.as_deref())?;
            self.index_dirty = false;
        }
        log.line(&format!("==> Installing makedepends: {}", packages.join(" ")));
        let mut cmd = self.in_chroot(root);
        cmd.arg("-c")
            .arg(format!(
                "{CH_DIR}/paquet -S --nosign > /dev/null || exit 1\n\
                 for package in \"$@\"; do\n    {CH_DIR}/paquet -i \"$package\" || exit 1\ndone"
            ))
            .arg("pqmake")
            .args(&packages);
        if !log.run(cmd)? {
            bail!("unable to install the makedepends of {name}");
        }
        Ok(())
    }

    fn download_source(&self, url: &str, dest: &Path, log: &mut Log) -> Result<()> {
        let mut last = None;
        for attempt in 0..3 {
            if attempt > 0 {
                log.line(&format!("Retrying {url}"));
            }
            match fetch::download(url, dest) {
                Ok(()) => return Ok(()),
                Err(e) if e.downcast_ref::<util::Interrupted>().is_some() => return Err(e),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("cannot download {url}")))
    }

    /// Finds a source file that is not in the cache among the sources kept
    /// by the `<name>.sources` package of this version: installed in
    /// `USR_SRC_DIR/<name>-<version>/`, or in its archive in the repository
    /// (any release). Copies it to `dest` (in the cache) when found.
    fn find_kept_source(&self, name: &str, file: &str, dest: &Path, log: &mut Log) -> bool {
        let r = self.recipe(name);
        let rel = format!("{name}-{}/{file}", r.version);
        let installed = self.s.usr_src_dir.join(&rel);
        if installed.is_file() {
            let part = util::with_suffix(dest, ".part");
            match fs::copy(&installed, &part).and_then(|_| fs::rename(&part, dest)) {
                Ok(()) => {
                    log.line(&format!(
                        "Using {file} from {}",
                        self.s.usr_src_dir.join(format!("{name}-{}", r.version)).display()
                    ));
                    return true;
                }
                Err(err) => {
                    let _ = fs::remove_file(&part);
                    log.line(&format!("Warning: unable to copy {}: {err}", installed.display()));
                }
            }
        }
        let entry = format!("usr/src/{rel}");
        for archive in self.sources_archives(name) {
            match crate::archive::extract_file(&archive, entry.as_bytes(), dest) {
                Ok(true) => {
                    log.line(&format!("Using {file} from {}", archive.display()));
                    return true;
                }
                Ok(false) => {}
                Err(err) => log.line(&format!("Warning: {err:#}")),
            }
        }
        false
    }

    /// Archives of the `sources` subpackage of a recipe's version in the
    /// repository, newest release first.
    fn sources_archives(&self, name: &str) -> Vec<PathBuf> {
        let r = self.recipe(name);
        let dir = self.s.repo_dir.join(&self.recipe_set[name]);
        let wanted = format!("{name}.sources");
        let mut found: Vec<(String, PathBuf)> = util::files_with_suffix(&dir, "-sources.tar.zst")
            .into_iter()
            .filter_map(|path| {
                let base = path.file_name()?.to_str()?.strip_suffix(".tar.zst")?.to_string();
                let (pkg, version, release) = util::split_archive_base(&base)?;
                (pkg == wanted && version == r.version).then(|| (release.to_string(), path.clone()))
            })
            .collect();
        found.sort_by(|a, b| crate::version::compare(&b.0, &a.0));
        found.into_iter().map(|(_, p)| p).collect()
    }

    /// Gets the recipe's sources to `SOURCES_DIR/<recipe>` (once: from the
    /// sources kept by its `sources` package, else by downloading them) or
    /// picks them up in the recipe, and unpacks the archives into `src`.
    /// Other files are copied as they are.
    fn prepare_sources(&self, name: &str, src: &Path, log: &mut Log) -> Result<()> {
        let recipe_dir = self.recipe_path[name].parent().unwrap_or(Path::new(".")).to_path_buf();
        for entry in &self.recipe(name).sources {
            let file = entry.rsplit('/').next().unwrap_or("");
            if file.is_empty() || file == "." || file == ".." {
                bail!("cannot tell the file name of source {entry}");
            }
            let path = if entry.contains("://") {
                // One directory per recipe: GitHub archives are all named
                // v<version>.tar.gz
                let dir = self.s.sources_dir.join(name);
                let path = dir.join(file);
                if !path.is_file() {
                    fs::create_dir_all(&dir)?;
                    if !self.find_kept_source(name, file, &path, log) {
                        log.line(&format!("Downloading {entry}"));
                        self.download_source(entry, &path, log).with_context(|| format!("cannot download {entry}"))?;
                    }
                }
                path
            } else {
                let path = recipe_dir.join(entry);
                if !path.is_file() {
                    bail!("source {entry} not found in {}", recipe_dir.display());
                }
                path
            };
            let is_tar = file.ends_with(".tar")
                || file.contains(".tar.")
                || [".tgz", ".tbz2", ".txz"].iter().any(|s| file.ends_with(s));
            if is_tar || file.ends_with(".zip") {
                let mut cmd = if is_tar {
                    let mut c = Command::new("tar");
                    c.arg("--no-same-owner").arg("-xf").arg(&path).arg("-C").arg(src);
                    c
                } else {
                    let mut c = Command::new("unzip");
                    c.arg("-q").arg(&path).arg("-d").arg(src);
                    c
                };
                cmd.current_dir(src);
                if !log.run(cmd)? {
                    bail!("cannot unpack {file}");
                }
            } else {
                fs::copy(&path, src.join(file)).with_context(|| format!("cannot copy {file}"))?;
            }
        }
        Ok(())
    }

    /// Builds a recipe and writes its archives. Returns false when the
    /// build failed (the error is reported).
    pub fn build_recipe(&mut self, name: &str) -> Result<bool> {
        let r = self.recipe(name).clone();
        let pkgfile = self.recipe_path[name].clone();
        let work = self.s.work_dir.join(name);
        let log_path = self.s.log_dir.join(format!("{name}-{}-{}.log", r.version, r.release));
        println!("==> Building {name} {}-{}", r.version, r.release);

        // The build gets a copy of the recipe directory (PKGMK_ROOT)
        let recipe_dir = pkgfile.parent().unwrap_or(Path::new(".")).to_path_buf();
        let chrooted = self.s.chroot.is_some();
        let root = work.join("root");
        let ch = root.join(&CH_DIR[1..]);
        let (src, pkg) = if chrooted { (ch.join("src"), ch.join("pkg")) } else { (work.join("src"), work.join("pkg")) };
        let setup = (|| -> Result<()> {
            remove_work(&work)?;
            fs::create_dir_all(&work)?;
            fs::create_dir_all(&self.s.log_dir)?;
            let recipe_copy = if chrooted {
                self.setup_chroot(&work).with_context(|| format!("cannot set up the build chroot for {name}"))?;
                ch.join("recipe")
            } else {
                fs::create_dir_all(&src)?;
                fs::create_dir_all(&pkg)?;
                work.join("recipe")
            };
            copy_tree(&recipe_dir, &recipe_copy).context("unable to copy the recipe")?;
            if let Some(style) = &r.style {
                fs::copy(style, recipe_copy.join(STYLE_FILE))
                    .with_context(|| format!("unable to copy the build style {}", style.display()))?;
            }
            Ok(())
        })();
        if let Err(err) = setup {
            let _ = self.unmount_all();
            eprintln!("Error: {err:#}");
            eprintln!("Error: build of {name} failed (work: {})", work.display());
            return Ok(false);
        }

        // The steps of the recipe install the package into $PKG
        let mut log = match Log::create(&log_path, self.s.quiet) {
            Ok(log) => log,
            Err(err) => {
                let _ = self.unmount_all();
                return Err(err);
            }
        };
        let script = recipe_build_script();
        let steps = (|| -> Result<()> {
            if chrooted {
                self.install_makedepends(name, &root, &mut log)?;
                self.prepare_sources(name, &src, &mut log)?;
                let mut cmd = self.in_chroot(&root);
                cmd.arg("-c")
                    .arg(format!("cd \"$SRC\"; {script}"))
                    .arg("pqmake")
                    .arg(format!("{CH_DIR}/recipe/Pkgfile"));
                if r.style.is_some() {
                    cmd.env("PQMAKE_STYLE_FILE", format!("{CH_DIR}/recipe/{STYLE_FILE}"));
                }
                if !log.run(cmd)? {
                    bail!("the build steps failed");
                }
            } else {
                self.prepare_sources(name, &src, &mut log)?;
                let mut cmd = Command::new("bash");
                cmd.arg("-c")
                    .arg(&script)
                    .arg("pqmake")
                    .arg(work.join("recipe/Pkgfile"))
                    .current_dir(&src)
                    .env("PKG", &pkg)
                    .env("SRC", &src)
                    .env("PKGMK_ROOT", work.join("recipe"))
                    .env("PKGMK_ARCH", &self.arch)
                    .env("PQMAKE_GROUPS", &self.s.default_groups)
                    .env("MAKEFLAGS", &self.s.makeflags)
                    .env("CFLAGS", &self.s.cflags)
                    .env("CXXFLAGS", &self.s.cxxflags)
                    .env_remove("PQMAKE_STYLE_FILE");
                if r.style.is_some() {
                    cmd.env("PQMAKE_STYLE_FILE", work.join("recipe").join(STYLE_FILE));
                }
                if !log.run(cmd)? {
                    bail!("the build steps failed");
                }
            }
            if fs::read_dir(&pkg).map(|mut d| d.next().is_none()).unwrap_or(true) {
                bail!("build() of {name} installed nothing in $PKG");
            }
            Ok(())
        })();

        let mut ok = match steps {
            Ok(()) => true,
            Err(err) if err.downcast_ref::<util::Interrupted>().is_some() => {
                let _ = self.unmount_all();
                return Err(err);
            }
            Err(err) => {
                log.line(&format!("Error: {err:#}"));
                false
            }
        };
        drop(log);
        if ok && crate::signals::pending().is_none() {
            println!("==> Packaging {name} {}-{}", r.version, r.release);
            let root_opt = chrooted.then_some(root.as_path());
            if let Err(err) = self.package_recipe(name, &work, &pkg, root_opt) {
                eprintln!("Error: {err:#}");
                ok = false;
            }
        }
        if chrooted && self.unmount_all().is_err() {
            bail!("cannot unmount the build chroot of {name}");
        }
        if !ok {
            if self.s.quiet {
                println!("{}", tail(&log_path, 20));
            }
            eprintln!("Error: build of {name} failed (log: {}, work: {})", log_path.display(), work.display());
            return Ok(false);
        }
        if !self.s.keep_work {
            if let Err(err) = remove_work(&work) {
                eprintln!("Warning: {err:#}");
            }
        }
        Ok(true)
    }
}
