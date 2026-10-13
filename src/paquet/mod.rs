//! `paquet`: installs, upgrades and removes the packages of a system.
//!
//! Files (paths relative to the target root, see `--chroot`):
//!
//! | Path | Content |
//! |---|---|
//! | `/etc/pq.conf` | `MIRROR` and `BRANCH` of the repository |
//! | `/etc/pq/keys/*.pub` | public keys trusted to sign the index |
//! | `/var/lib/pq/PQREPO` | index of the repository, as last fetched |
//! | `/var/lib/pq/PQREPOSTATE` | state of every known package |
//! | `/var/lib/pq/files/<package>` | files installed by a package |
//! | `/var/lib/pq/cache/` | downloaded archives |
//! | `/var/lib/pq/transaction/` | journals of the operations in progress |
//! | `/var/lib/pq/scripts/<package>/` | scripts of an installed package |
//! | `/etc/pq/hooks/*.hook` | hooks run after installs and removals |
//!
//! The configuration and the trusted keys always come from the running
//! system; `PQ_CONFIG` and `PQ_KEYS_DIR` override their paths.

pub mod cli;
pub mod download;
pub mod filelist;
pub mod scripts;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::record::{Record, Records, dep_name, is_installed_status, set_record_in_file};
use crate::util::{
    self, Interrupted, concat_path, copy_preserve, exists_or_symlink, is_real_dir, is_real_file, move_path,
    sha256_file, with_suffix,
};
use crate::{archive, fetch, sign, signals, version};
use scripts::Op;

/// Locations of the files of a managed system.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config_file: PathBuf,
    pub keys_dir: PathBuf,
    pub repo_file: PathBuf,
    pub local_repo_file: PathBuf,
    pub files_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub lock_file: PathBuf,
    pub txn_dir: PathBuf,
    pub scripts_dir: PathBuf,
    pub hooks_dir: PathBuf,
    /// Root of the managed system: empty for the running system.
    pub target_root: Vec<u8>,
}

impl Paths {
    pub fn system() -> Paths {
        let env_path = |var: &str, default: &str| {
            std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| default.into())
        };
        Paths {
            config_file: env_path("PQ_CONFIG", "/etc/pq.conf"),
            keys_dir: env_path("PQ_KEYS_DIR", "/etc/pq/keys"),
            repo_file: "/var/lib/pq/PQREPO".into(),
            local_repo_file: "/var/lib/pq/PQREPOSTATE".into(),
            files_dir: "/var/lib/pq/files".into(),
            cache_dir: "/var/lib/pq/cache".into(),
            lock_file: "/var/lib/pq/lock".into(),
            txn_dir: "/var/lib/pq/transaction".into(),
            scripts_dir: "/var/lib/pq/scripts".into(),
            hooks_dir: "/etc/pq/hooks".into(),
            target_root: Vec::new(),
        }
    }

    /// Points the package database at another root (`--chroot`).
    pub fn with_root(mut self, dir: &Path) -> Result<Paths> {
        if !dir.is_dir() {
            bail!("chroot directory {} not found.", dir.display());
        }
        let bytes = dir.as_os_str().as_bytes();
        let root = bytes.strip_suffix(b"/").unwrap_or(bytes).to_vec();
        for path in [
            &mut self.repo_file,
            &mut self.local_repo_file,
            &mut self.files_dir,
            &mut self.cache_dir,
            &mut self.lock_file,
            &mut self.txn_dir,
            &mut self.scripts_dir,
            &mut self.hooks_dir,
        ] {
            *path = concat_path(&root, path.as_os_str().as_bytes());
        }
        self.target_root = root;
        Ok(self)
    }

    /// Directory the archives are extracted into.
    pub fn root_dir(&self) -> PathBuf {
        if self.target_root.is_empty() { PathBuf::from("/") } else { util::bytes_path(&self.target_root) }
    }
}

/// A managed system and the repository it installs from.
pub struct Paquet {
    pub paths: Paths,
    pub mirror: String,
    pub branch: String,
    pub arch: String,
    /// `--nosign`: `--sync` skips the signature check of the index.
    pub nosign: bool,
    /// `--noscripts`: package scripts and hooks are not run.
    pub noscripts: bool,
    /// `-y`: questions are answered yes.
    pub assume_yes: bool,
    /// Subpackages installed with every package (`GROUP=` lines of the
    /// configuration): languages (`fr`) or other groups (`man`).
    pub groups: Vec<String>,
    /// Simultaneous downloads (`PARALLEL_DOWNLOADS`).
    pub jobs: usize,
    /// Archives downloaded or found in the cache by this command, checked,
    /// by file name.
    prefetched: HashMap<String, PathBuf>,
    /// Warnings already given, so planning and doing give them once.
    warned: std::cell::RefCell<HashSet<String>>,
    /// Name the program was called with, for messages.
    pub prog: String,
    /// Paths installed or removed by the command, for the hooks.
    touched: Vec<(Vec<u8>, Op)>,
    /// A script or a hook failed after its package was committed.
    script_failed: bool,
    repo: Records,
    state: Records,
    /// Journal of the operation in progress.
    txn: Option<PathBuf>,
    lock: Option<fs::File>,
}

impl Drop for Paquet {
    fn drop(&mut self) {
        // An operation interrupted by an error is rolled back
        if let Some(jdir) = self.txn.take() {
            self.rollback(&jdir);
            signals::defer(false);
        }
    }
}

impl Paquet {
    pub fn new(paths: Paths, mirror: String, branch: String, arch: String, prog: String) -> Paquet {
        Paquet {
            paths,
            mirror,
            branch,
            arch,
            nosign: false,
            noscripts: false,
            assume_yes: false,
            groups: Vec::new(),
            jobs: download::DEFAULT_JOBS,
            prefetched: HashMap::new(),
            warned: Default::default(),
            prog,
            touched: Vec::new(),
            script_failed: false,
            repo: Records::default(),
            state: Records::default(),
            txn: None,
            lock: None,
        }
    }

    pub fn load_repo(&mut self) {
        self.repo = Records::load(&self.paths.repo_file);
    }

    pub fn load_state(&mut self) {
        self.state = Records::load(&self.paths.local_repo_file);
    }

    /// Stores a package record in the local state, replacing the record of
    /// the same name or adding it.
    fn set_state(&mut self, record: Record) -> Result<()> {
        set_record_in_file(&self.paths.local_repo_file, &record)?;
        self.state.map.insert(record.name.clone(), record);
        Ok(())
    }

    /// Makes sure only one instance modifies a system at a time. The lock is
    /// released when the process exits.
    pub fn acquire_lock(&mut self) -> Result<()> {
        if let Some(dir) = self.paths.lock_file.parent() {
            fs::create_dir_all(dir).with_context(|| format!("unable to create {}", dir.display()))?;
        }
        fs::create_dir_all(&self.paths.files_dir)
            .with_context(|| format!("unable to create {}", self.paths.files_dir.display()))?;
        match util::try_lock(&self.paths.lock_file)
            .with_context(|| format!("unable to lock {}", self.paths.lock_file.display()))?
        {
            Some(file) => {
                self.lock = Some(file);
                Ok(())
            }
            None => bail!("another instance of {} is running (lock: {}).", self.prog, self.paths.lock_file.display()),
        }
    }

    fn mirror_url(&self, rest: &str) -> Result<String> {
        if self.mirror.is_empty() {
            bail!("MIRROR is not set in {}.", self.paths.config_file.display());
        }
        Ok(format!("{}/{}/{}/{}", self.mirror.trim_end_matches('/'), self.branch, self.arch, rest))
    }

    fn require_state_file(&self) -> Result<()> {
        if !self.paths.local_repo_file.is_file() {
            bail!("No package state found. Run '{} --sync' first.", self.prog);
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Repository index

    /// Fetches the index from the mirror, checks its signature and merges it
    /// with the local state. The previous index is kept when the new one
    /// cannot be verified.
    pub fn sync(&mut self) -> Result<()> {
        let url = self.mirror_url("PQREPO")?;
        println!("Fetching repository file from {url}");
        let data =
            fetch::download_bytes(&url).with_context(|| format!("failed to fetch repository file from {url}"))?;
        if self.nosign {
            println!("Warning: signature check disabled (--nosign), the index is not authenticated.");
        } else {
            let sig_url = format!("{url}.sig");
            let sig = fetch::download_bytes(&sig_url).map_err(|_| {
                anyhow!(
                    "no signature found at {sig_url}\n  The repository is not signed. \
                     Use --nosign to skip the check (not recommended)."
                )
            })?;
            let key = sign::verify_with_keys(&self.paths.keys_dir, &data, &sig)
                .context("repository file rejected, the local index is unchanged.")?;
            println!("Signature verified with {}", key.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
        }
        if let Some(dir) = self.paths.repo_file.parent() {
            fs::create_dir_all(dir)?;
        }
        util::atomic_write(&self.paths.repo_file, &data)?;
        println!("Repository file fetched successfully!");

        println!("Merging with local state...");
        self.load_state();
        self.load_repo();
        let mut merged = String::new();
        // Installed packages keep their local record (the installed version)
        // and get U when the archive changed; others take the remote one
        for (name, remote) in &self.repo.map {
            let line = match self.state.get(name) {
                Some(local) if local.is_installed() => {
                    let mut status: String = local.status.chars().filter(|c| !matches!(c, 'U' | 'X')).collect();
                    if remote.checksum != local.checksum {
                        status.push('U');
                    }
                    local.with_status(&status).line()
                }
                _ => remote.with_status("N").line(),
            };
            merged.push_str(&line);
            merged.push('\n');
        }
        // Installed packages the repository no longer provides are flagged
        // X; packages that are neither installed nor provided are dropped
        for (name, local) in &self.state.map {
            if self.repo.map.contains_key(name) || !local.is_installed() {
                continue;
            }
            let mut status: String = local.status.chars().filter(|c| !matches!(c, 'U' | 'X')).collect();
            status.push('X');
            merged.push_str(&local.with_status(&status).line());
            merged.push('\n');
        }
        util::atomic_write(&self.paths.local_repo_file, merged.as_bytes())?;
        self.load_state();
        println!("Merge completed!");
        Ok(())
    }

    fn state_records(&self) -> Result<Vec<Record>> {
        self.require_state_file()?;
        let content = fs::read(&self.paths.local_repo_file)?;
        Ok(String::from_utf8_lossy(&content)
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(Record::parse)
            .collect())
    }

    pub fn list(&self) -> Result<()> {
        let records = self.state_records()?;
        println!("Listing all packages:");
        for r in records {
            println!(
                "Name: {}, Version: {}-{}, Description: {}, Status: {}",
                r.name,
                r.version,
                r.release,
                r.description,
                if r.status.is_empty() { "N" } else { &r.status }
            );
        }
        Ok(())
    }

    /// Searches the packages by name, description, folder or set.
    pub fn search(&self, term: &str) -> Result<()> {
        let records = self.state_records()?;
        println!("Searching for packages matching: {term}");
        let term = term.to_lowercase();
        for r in records {
            if [&r.name, &r.description, &r.folder, &r.set].iter().any(|f| f.to_lowercase().contains(&term)) {
                println!("Name: {}", r.name);
                println!("Version: {}-{}", r.version, r.release);
                println!("Description: {}", r.description);
                println!("Folder: {}", r.folder);
                println!("Groups: {}", r.groups);
                println!("Set: {}", r.set);
                println!("Depends: {}", r.depends);
                println!("Status: {}", if r.status.is_empty() { "N" } else { &r.status });
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Dependencies

    /// Packages to install so that the given packages (names or dependency
    /// entries) and their dependencies are present, dependencies first.
    fn resolve_install(&self, required_by: &str, deps: &[&str]) -> Result<Vec<String>> {
        let mut resolved = HashSet::new();
        let mut visiting = HashSet::new();
        let mut order = Vec::new();
        for dep in deps {
            self.resolve_package(dep, required_by, &mut resolved, &mut visiting, &mut order)?;
        }
        Ok(order)
    }

    fn resolve_package(
        &self,
        entry: &str,
        required_by: &str,
        resolved: &mut HashSet<String>,
        visiting: &mut HashSet<String>,
        order: &mut Vec<String>,
    ) -> Result<()> {
        let pkg = dep_name(entry);
        if entry != pkg && self.warned.borrow_mut().insert(format!("{required_by} {entry}")) {
            println!("Warning: version constraint '{entry}' of {required_by} is not enforced yet.");
        }
        if resolved.contains(pkg) || visiting.contains(pkg) {
            return Ok(());
        }
        if self.state.is_installed(pkg) {
            resolved.insert(pkg.to_string());
            return Ok(());
        }
        let Some(record) = self.repo.get(pkg) else {
            let by = if required_by.is_empty() { String::new() } else { format!(" (required by {required_by})") };
            bail!("package {pkg} not found in the repository{by}.");
        };
        visiting.insert(pkg.to_string());
        for dep in record.deps() {
            self.resolve_package(dep, pkg, resolved, visiting, order)?;
        }
        visiting.remove(pkg);
        resolved.insert(pkg.to_string());
        order.push(pkg.to_string());
        Ok(())
    }

    /// Installed packages that depend on a package, sorted.
    fn installed_dependents(&self, target: &str) -> Vec<String> {
        self.state
            .map
            .values()
            .filter(|r| r.name != target && r.is_installed() && r.deps().any(|d| dep_name(d) == target))
            .map(|r| r.name.clone())
            .collect()
    }

    /// Orphans: automatically installed packages that no explicitly
    /// installed package requires, directly or not.
    pub fn find_orphans(&mut self) -> Vec<String> {
        self.load_state();
        let state = &self.state;
        let mut reached: HashSet<&str> = HashSet::new();
        fn visit<'a>(state: &'a Records, roots: Vec<&'a str>, reached: &mut HashSet<&'a str>) {
            let mut queue: std::collections::VecDeque<&str> = roots.into_iter().collect();
            while let Some(pkg) = queue.pop_front() {
                let Some((name, record)) = state.map.get_key_value(pkg) else { continue };
                if !reached.insert(name.as_str()) {
                    continue;
                }
                for dep in record.deps() {
                    let dep = dep_name(dep);
                    if !reached.contains(dep) && state.is_installed(dep) {
                        queue.push_back(dep);
                    }
                }
            }
        }
        let explicit: Vec<&str> =
            state.map.values().filter(|r| r.status.contains('I')).map(|r| r.name.as_str()).collect();
        visit(state, explicit, &mut reached);
        // The subpackages of the configured groups (nano.fr) stay with their
        // package, without keeping it: they depend on it
        let kept: Vec<&str> = state
            .map
            .values()
            .filter(|r| r.is_installed() && !reached.contains(r.name.as_str()))
            .filter(|r| {
                util::split_group(&r.name)
                    .is_some_and(|(base, group)| self.groups.iter().any(|g| g == group) && reached.contains(base))
            })
            .map(|r| r.name.as_str())
            .collect();
        visit(state, kept, &mut reached);
        state
            .map
            .values()
            .filter(|r| r.status.contains('A') && !reached.contains(r.name.as_str()))
            .map(|r| r.name.clone())
            .collect()
    }

    /// Subpackages of the configured groups (`GROUP=`) that go with a
    /// package and are not installed yet: `nano.fr`, `nano.man`...
    fn missing_companions(&self, pkg: &str) -> Vec<String> {
        if util::split_group(pkg).is_some() {
            return Vec::new();
        }
        self.groups
            .iter()
            .map(|g| format!("{pkg}.{g}"))
            .filter(|c| self.repo.get(c).is_some() && !self.state.is_installed(c))
            .collect()
    }

    /// Installed subpackages that only make sense with their package, and
    /// go away with it: the automatically installed `<pkg>.<language>` and
    /// `<pkg>.<configured group>`.
    fn installed_companions(&self, pkg: &str) -> Vec<String> {
        self.state
            .map
            .values()
            .filter(|r| r.is_installed() && r.status.contains('A'))
            .filter(|r| {
                util::split_group(&r.name).is_some_and(|(base, group)| {
                    base == pkg && (util::is_language(group) || self.groups.iter().any(|g| g == group))
                })
            })
            .map(|r| r.name.clone())
            .collect()
    }

    /// Installs the missing subpackages of the configured groups of an
    /// installed package (`nano.fr` for `GROUP=fr`).
    fn install_companions(&mut self, pkg: &str) -> Result<()> {
        for companion in self.missing_companions(pkg) {
            for p in self.resolve_install(pkg, &[companion.as_str()])? {
                self.install_package(&p, "A")?;
            }
        }
        Ok(())
    }

    /// Orders packages to upgrade so that dependencies come first, following
    /// the dependencies of the new versions (also through packages that are
    /// not upgraded).
    fn upgrade_order(&self, candidates: &[String]) -> Vec<String> {
        let pending: HashSet<&str> = candidates.iter().map(String::as_str).collect();
        let mut visited = HashSet::new();
        let mut order = Vec::new();
        for pkg in candidates {
            self.visit_upgrade(pkg, &pending, &mut visited, &mut order);
        }
        order
    }

    fn visit_upgrade(
        &self,
        pkg: &str,
        pending: &HashSet<&str>,
        visited: &mut HashSet<String>,
        order: &mut Vec<String>,
    ) {
        if !visited.insert(pkg.to_string()) {
            return;
        }
        let deps = match self.repo.get(pkg) {
            Some(r) if !r.depends.is_empty() => r.depends.clone(),
            _ => self.state.get(pkg).map(|r| r.depends.clone()).unwrap_or_default(),
        };
        for dep in deps.split_whitespace() {
            self.visit_upgrade(dep_name(dep), pending, visited, order);
        }
        if pending.contains(pkg) {
            order.push(pkg.to_string());
        }
    }

    // ---------------------------------------------------------------------
    // Archives

    /// Gets the archive of a package into the cache and checks its SHA-256
    /// checksum. A cached archive with the right checksum is used as is.
    fn fetch_archive(&self, record: &Record) -> Result<PathBuf> {
        let file = record.archive_name();
        if let Some(path) = self.prefetched.get(&file) {
            return Ok(path.clone());
        }
        let dest = self.paths.cache_dir.join(&file);
        if record.checksum.is_empty() {
            bail!("no checksum for {file} in the repository index.");
        }
        if dest.is_file() && sha256_file(&dest).ok().as_deref() == Some(record.checksum.as_str()) {
            println!("Using cached {file}");
            return Ok(dest);
        }
        let url = self.archive_url(record)?;
        fs::create_dir_all(&self.paths.cache_dir)
            .with_context(|| format!("unable to create {}", self.paths.cache_dir.display()))?;
        let new = with_suffix(&dest, ".new");
        fetch::download(&url, &new)?;
        let actual = sha256_file(&new)?;
        if actual != record.checksum {
            let _ = fs::remove_file(&new);
            bail!("checksum mismatch for {file}\n  expected: {}\n  actual:   {actual}", record.checksum);
        }
        fs::rename(&new, &dest)?;
        Ok(dest)
    }

    fn archive_url(&self, record: &Record) -> Result<String> {
        let file = record.archive_name();
        self.mirror_url(&if record.folder.is_empty() { file } else { format!("{}/{file}", record.folder) })
    }

    /// Archives needed to install or upgrade some packages. Those already
    /// in the cache, with the right checksum, are kept for the command; the
    /// others are returned, with their size when the mirror tells it. Also
    /// returns how many archives come from the cache.
    fn plan_downloads(&mut self, names: &[String]) -> Result<(Vec<download::Item>, usize)> {
        let mut wanted: Vec<(Record, String)> = Vec::new();
        for name in names {
            let Some(record) = self.repo.get(name).cloned() else {
                bail!("Package {name} not found in the repository.");
            };
            let file = record.archive_name();
            if record.checksum.is_empty() {
                bail!("no checksum for {file} in the repository index.");
            }
            if self.prefetched.contains_key(&file) || wanted.iter().any(|(r, _)| r.archive_name() == file) {
                continue;
            }
            let url = self.archive_url(&record)?;
            wanted.push((record, url));
        }
        let cache = self.paths.cache_dir.clone();
        fs::create_dir_all(&cache).with_context(|| format!("unable to create {}", cache.display()))?;
        // Leftovers of interrupted downloads
        for part in util::files_with_suffix(&cache, ".part") {
            let _ = fs::remove_file(part);
        }
        let checked = download::parallel_map(&wanted, self.jobs, |(record, url)| {
            let dest = cache.join(record.archive_name());
            let cached = dest.is_file() && sha256_file(&dest).ok().as_deref() == Some(record.checksum.as_str());
            (!cached).then(|| fetch::size(url).ok().flatten())
        });
        let mut items = Vec::new();
        let mut cached = 0;
        for ((record, url), size) in wanted.into_iter().zip(checked) {
            let dest = cache.join(record.archive_name());
            match size {
                None => {
                    self.prefetched.insert(record.archive_name(), dest);
                    cached += 1;
                }
                Some(size) => items.push(download::Item {
                    label: format!("{} {}-{}", record.name, record.version, record.release),
                    url,
                    dest,
                    checksum: record.checksum.clone(),
                    size,
                }),
            }
        }
        Ok((items, cached))
    }

    /// Downloads planned archives, several at a time.
    fn fetch_planned(&mut self, items: &[download::Item]) -> Result<()> {
        download::download_all(items, self.jobs)?;
        for item in items {
            let file = item.dest.file_name().unwrap_or_default().to_string_lossy().into_owned();
            self.prefetched.insert(file, item.dest.clone());
        }
        Ok(())
    }

    /// Gets the archives of some packages into the cache before anything is
    /// installed: a missing or damaged archive stops the command first.
    fn download_archives(&mut self, names: &[String]) -> Result<()> {
        let (items, _) = self.plan_downloads(names)?;
        self.fetch_planned(&items)
    }

    /// Test hook: simulates a crash (kill -9), a Ctrl-C or a failure (such
    /// as a full disk) at a given step when `PQ_TEST_CRASH` is
    /// `kill:<step>`, `int:<step>` or `fail:<step>`. Only used by the tests.
    fn test_crash_point(&self, step: &str) -> Result<()> {
        let Ok(spec) = std::env::var("PQ_TEST_CRASH") else { return Ok(()) };
        match spec.split_once(':') {
            Some(("kill", s)) if s == step => signals::raise(libc::SIGKILL),
            Some(("int", s)) if s == step => {
                signals::raise(libc::SIGINT);
                signals::check()?;
            }
            Some(("fail", s)) if s == step => bail!("simulated failure at {step}"),
            _ => {}
        }
        Ok(())
    }

    /// Same, for steps where only a crash or a Ctrl-C matters.
    fn test_interrupt_point(&self, step: &str) -> Result<()> {
        match self.test_crash_point(step) {
            Err(err) if err.downcast_ref::<Interrupted>().is_some() => Err(err),
            _ => Ok(()),
        }
    }

    // ---------------------------------------------------------------------
    // Transactions
    //
    // Transactions make installs and upgrades all-or-nothing. Before
    // touching the system, <txn_dir>/<package>/ records:
    //   backup/<n>, backed_up  every file that may be replaced or removed,
    //                          saved as a hard link (same inode, no copy) or
    //                          a copy
    //   created                every path the operation may create
    //   filelist, state        the previous file list and state record
    // The journal is deleted once the operation has succeeded. After an
    // error or a Ctrl-C it is rolled back at once; after a crash, at the
    // next run.

    fn begin_transaction(&mut self, pkg: &str, new_list: &[u8], old_list: Option<&[u8]>) -> Result<()> {
        let jdir = self.paths.txn_dir.join(pkg);
        if exists_or_symlink(&jdir) {
            bail!("an interrupted operation on {pkg} is pending. Run '{} --recover'.", self.prog);
        }
        signals::defer(true);
        fs::create_dir_all(jdir.join("backup")).with_context(|| format!("unable to create {}", jdir.display()))?;
        self.txn = Some(jdir.clone());

        fs::write(jdir.join("package"), format!("{pkg}\n"))?;
        let list_path = self.paths.files_dir.join(pkg);
        let mut path_line = list_path.as_os_str().as_bytes().to_vec();
        path_line.push(b'\n');
        fs::write(jdir.join("filelist.path"), path_line)?;
        if list_path.is_file() {
            copy_preserve(&list_path, &jdir.join("filelist"))?;
        }
        let mut state_path = self.paths.local_repo_file.as_os_str().as_bytes().to_vec();
        state_path.push(b'\n');
        fs::write(jdir.join("state.path"), state_path)?;
        let state = match (self.state.get(pkg), self.repo.get(pkg)) {
            (Some(r), _) => r.line(),
            (None, Some(r)) => r.with_status("N").line(),
            (None, None) => String::new(),
        };
        fs::write(jdir.join("state"), format!("{state}\n"))?;

        let mut backed_up = fs::File::create(jdir.join("backed_up"))?;
        let mut created = fs::File::create(jdir.join("created"))?;

        // Paths of the new version (and the .pqnew files it may add), then
        // those of the installed version
        let mut paths: Vec<(bool, PathBuf)> = Vec::new();
        for (path, real) in filelist::entries(new_list, &self.paths.target_root) {
            let pqnew = filelist::is_protected(&path).then(|| with_suffix(&real, ".pqnew"));
            paths.push((true, real));
            if let Some(pqnew) = pqnew {
                paths.push((true, pqnew));
            }
        }
        if let Some(old) = old_list {
            for (_, real) in filelist::entries(old, &self.paths.target_root) {
                paths.push((false, real));
            }
        }
        // The stored scripts of the package
        let scripts_dir = scripts::package_dir(&self.paths.scripts_dir, pkg);
        for (name, _) in archive::SCRIPTS {
            paths.push((true, scripts_dir.join(name)));
        }
        paths.push((true, scripts_dir));

        let mut seen = HashSet::new();
        let mut n = 0;
        for (is_new, real) in paths {
            signals::check()?;
            if !seen.insert(real.clone()) || is_real_dir(&real) {
                continue;
            }
            if exists_or_symlink(&real) {
                n += 1;
                let backup = jdir.join("backup").join(n.to_string());
                if !(is_real_file(&real) && fs::hard_link(&real, &backup).is_ok()) {
                    copy_preserve(&real, &backup).with_context(|| format!("unable to back up {}", real.display()))?;
                }
                let mut line = format!("{n}\t").into_bytes();
                line.extend_from_slice(real.as_os_str().as_bytes());
                line.push(b'\n');
                backed_up.write_all(&line)?;
            } else if is_new {
                let mut line = real.as_os_str().as_bytes().to_vec();
                line.push(b'\n');
                created.write_all(&line)?;
            }
        }
        backed_up.sync_all()?;
        created.sync_all()?;
        Ok(())
    }

    /// Ends the active transaction successfully: its backups go.
    fn commit_transaction(&mut self) -> Result<()> {
        if let Some(jdir) = self.txn.take() {
            let _ = fs::remove_dir_all(&jdir);
        }
        signals::defer(false);
        // A Ctrl-C during the operation still ends the program
        signals::check()?;
        Ok(())
    }

    /// Rolls back the active transaction, if any.
    fn abort_transaction(&mut self) {
        if let Some(jdir) = self.txn.take() {
            self.rollback(&jdir);
        }
        signals::defer(false);
    }

    /// Undoes an operation from its journal: created paths are removed,
    /// backups restored, and the previous file list and state record put
    /// back.
    fn rollback(&mut self, jdir: &Path) {
        let read_line = |name: &str| -> Option<Vec<u8>> {
            let mut content = fs::read(jdir.join(name)).ok()?;
            while content.last() == Some(&b'\n') {
                content.pop();
            }
            Some(content)
        };
        let pkg = read_line("package")
            .map(|p| String::from_utf8_lossy(&p).into_owned())
            .unwrap_or_else(|| jdir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        println!("Rolling back the operation on {pkg}...");

        let journal_lines = |name: &str| -> Vec<Vec<u8>> {
            fs::read(jdir.join(name))
                .map(|c| c.split(|&b| b == b'\n').filter(|l| !l.is_empty()).map(<[u8]>::to_vec).collect())
                .unwrap_or_default()
        };
        let mut dirs = Vec::new();
        for real in journal_lines("created") {
            let real = util::bytes_path(&real);
            if is_real_dir(&real) {
                dirs.push(real);
            } else if exists_or_symlink(&real) {
                let _ = fs::remove_file(&real);
            }
        }
        for line in journal_lines("backed_up") {
            let Some(tab) = line.iter().position(|&b| b == b'\t') else { continue };
            let n = String::from_utf8_lossy(&line[..tab]).into_owned();
            let real = util::bytes_path(&line[tab + 1..]);
            let backup = jdir.join("backup").join(&n);
            if n.is_empty() || !exists_or_symlink(&backup) {
                continue;
            }
            if is_real_dir(&real) {
                let _ = fs::remove_dir(&real);
            } else {
                let _ = fs::remove_file(&real);
            }
            if let Some(parent) = real.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if move_path(&backup, &real).is_err() {
                println!("Warning: unable to restore {}", real.display());
            }
        }
        filelist::remove_empty_dirs(dirs, false);

        if let Some(list_path) = read_line("filelist.path").filter(|p| !p.is_empty()) {
            let list_path = util::bytes_path(&list_path);
            let saved = jdir.join("filelist");
            if saved.is_file() {
                let _ = fs::remove_file(&list_path);
                let _ = copy_preserve(&saved, &list_path);
            } else {
                let _ = fs::remove_file(&list_path);
            }
        }
        if let (Some(state_path), Some(state)) = (read_line("state.path"), read_line("state")) {
            if !state.is_empty() {
                let record = Record::parse(&String::from_utf8_lossy(&state));
                if set_record_in_file(&util::bytes_path(&state_path), &record).is_err() {
                    println!("Warning: unable to restore the state of {pkg}");
                }
            }
        }
        let _ = fs::remove_dir_all(jdir);
        self.load_state();
        println!("{pkg} restored to its previous state.");
    }

    /// Rolls back the operations left unfinished by a crash.
    pub fn recover(&mut self, verbose: bool) {
        let mut journals: Vec<PathBuf> = fs::read_dir(&self.paths.txn_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_real_dir(p))
            .collect();
        journals.sort();
        if journals.is_empty() && verbose {
            println!("Nothing to recover.");
        }
        for jdir in journals {
            self.rollback(&jdir);
        }
    }

    /// True if an interrupted operation waits for `--recover`.
    pub fn has_pending_transaction(&self) -> bool {
        fs::read_dir(&self.paths.txn_dir).map(|mut d| d.next().is_some()).unwrap_or(false)
    }

    /// Extracts an archive whose file list was prepared, inside the active
    /// transaction. Existing protected files (configuration) are never
    /// overwritten: the original is restored from the transaction backup and
    /// the packaged version is saved next to it as `<file>.pqnew`.
    fn extract_package(&self, archive_path: &Path) -> Result<()> {
        archive::extract(archive_path, &self.paths.root_dir())?;
        self.test_crash_point("after-extract")?;

        let Some(jdir) = &self.txn else { return Ok(()) };
        let backed_up = fs::read(jdir.join("backed_up")).unwrap_or_default();
        for line in backed_up.split(|&b| b == b'\n') {
            let Some(tab) = line.iter().position(|&b| b == b'\t') else { continue };
            let (n, real_bytes) = (&line[..tab], &line[tab + 1..]);
            if n.is_empty() || real_bytes.ends_with(b".pqnew") {
                continue;
            }
            let rel = real_bytes.strip_prefix(self.paths.target_root.as_slice()).unwrap_or(real_bytes);
            if !filelist::is_protected(rel) {
                continue;
            }
            let real = util::bytes_path(real_bytes);
            let backup = jdir.join("backup").join(util::bytes_path(n));
            if !is_real_file(&real) || !is_real_file(&backup) {
                continue;
            }
            if sha256_file(&real)? == sha256_file(&backup)? {
                continue;
            }
            let pqnew = with_suffix(&real, ".pqnew");
            fs::rename(&real, &pqnew)?;
            if fs::hard_link(&backup, &real).is_err() {
                copy_preserve(&backup, &real)?;
            }
            println!("Keeping existing {}, new version saved as {}", real.display(), pqnew.display());
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Commands

    /// Installs a package and its missing dependencies.
    pub fn install(&mut self, target: &str) -> Result<()> {
        self.load_repo();
        self.load_state();
        let status = self.state.status(target).to_string();
        if status.contains('A') {
            return self.mark(target, 'I');
        } else if status.contains('I') {
            bail!("Package {target} is already installed.");
        }

        // With the subpackages of the configured groups (GROUP=fr) of every
        // package installed
        let mut order = self.resolve_install("", &[target])?;
        let companions: Vec<String> = order.iter().flat_map(|p| self.missing_companions(p)).collect();
        if !companions.is_empty() {
            let wanted: Vec<&str> = order.iter().chain(&companions).map(String::as_str).collect();
            order = self.resolve_install("", &wanted)?;
        }
        if order.len() > 1 {
            println!("Installing {target} and its dependencies: {}", order.join(" "));
        }
        self.download_archives(&order)?;
        for pkg in &order {
            let reason = if pkg == target { "I" } else { "A" };
            self.install_package(pkg, reason).with_context(|| format!("installation of {target} stopped at {pkg}."))?;
        }
        Ok(())
    }

    /// Installs a single package whose dependencies are present. `reason`
    /// is I (explicit) or A (installed as a dependency).
    fn install_package(&mut self, name: &str, reason: &str) -> Result<()> {
        println!("Installing package: {name}");
        let Some(record) = self.repo.get(name).cloned() else {
            bail!("Package {name} not found in the repository.");
        };
        fs::create_dir_all(&self.paths.files_dir)?;
        let list_path = self.paths.files_dir.join(name);
        if list_path.is_file() {
            bail!("Package {name} is already installed.");
        }
        let archive_path = self.fetch_archive(&record)?;

        // The file list is built aside: it only replaces the real one once
        // the package has been extracted, so an aborted install leaves
        // nothing behind.
        let new_list = filelist::prepare(&archive_path, None, &self.paths.target_root)
            .with_context(|| format!("Aborting installation of {name}."))?;
        let package_scripts = read_scripts(&archive_path)?;
        let version = format!("{}-{}", record.version, record.release);

        // From here on, any failure restores the system as it was
        let result = (|| -> Result<()> {
            self.begin_transaction(name, &new_list, None)?;
            self.run_script(name, &package_scripts, "pre-install", &["install", &version])
                .with_context(|| format!("installation of {name} cancelled"))?;
            println!("Extracting package files...");
            self.extract_package(&archive_path).with_context(|| format!("extraction failed, rolling back {name}"))?;
            self.store_scripts(name, &package_scripts)?;
            util::atomic_write(&list_path, &new_list)?;
            self.set_state(record.with_status(reason))?;
            Ok(())
        })();
        if let Err(err) = result {
            self.abort_transaction();
            return Err(err);
        }
        self.commit_transaction()?;
        self.touch(&new_list, Op::Install);
        println!("{name} installed successfully!");
        self.run_late_script(name, &package_scripts, "post-install", &["install", &version]);
        Ok(())
    }

    /// Changes the installation reason (I or A) of an installed package.
    pub fn mark(&mut self, name: &str, reason: char) -> Result<()> {
        if self.state.map.is_empty() {
            self.load_state();
        }
        let Some(record) = self.state.get(name).filter(|r| r.is_installed()).cloned() else {
            bail!("Package {name} is not installed.");
        };
        let flags: String = record.status.chars().filter(|c| !matches!(c, 'N' | 'I' | 'A')).collect();
        self.set_state(record.with_status(&format!("{reason}{flags}")))?;
        if reason == 'I' {
            println!("{name} is now marked as explicitly installed.");
        } else {
            println!("{name} is now marked as automatically installed.");
        }
        Ok(())
    }

    /// Upgrades a package, installing the dependencies its new version adds.
    pub fn upgrade(&mut self, name: &str) -> Result<()> {
        println!("Upgrading package: {name}");
        self.load_repo();
        self.load_state();

        let list_path = self.paths.files_dir.join(name);
        let local_status = self.state.status(name).to_string();
        if !list_path.is_file() || !is_installed_status(&local_status) {
            bail!("Package {name} is not installed. Use --install instead.");
        }
        let Some(record) = self.repo.get(name).cloned() else {
            bail!("Package {name} not found in the repository.");
        };

        let deps: Vec<&str> = record.deps().collect();
        let order = self.resolve_install(name, &deps)?;
        let mut needed = order.clone();
        needed.push(name.to_string());
        let companions = self.missing_companions(name);
        for c in &companions {
            needed.extend(self.resolve_install(name, &[c.as_str()]).unwrap_or_default());
        }
        self.download_archives(&needed)?;
        for pkg in &order {
            self.install_package(pkg, "A")
                .with_context(|| format!("upgrade of {name} stopped at dependency {pkg}."))?;
        }

        let archive_path = self.fetch_archive(&record)?;
        // Files of the installed version are not conflicts
        let old_list = fs::read(&list_path)?;
        let new_list = filelist::prepare(&archive_path, Some(&old_list), &self.paths.target_root)
            .with_context(|| format!("Aborting upgrade of {name}."))?;
        let package_scripts = read_scripts(&archive_path)?;
        let version = format!("{}-{}", record.version, record.release);
        let old_version = self.state.get(name).map(|r| format!("{}-{}", r.version, r.release)).unwrap_or_default();
        let stale = filelist::stale(&old_list, &new_list);

        // From here on, any failure restores the previous version
        let result = (|| -> Result<()> {
            self.begin_transaction(name, &new_list, Some(&old_list))?;
            self.run_script(name, &package_scripts, "pre-install", &["upgrade", &version, &old_version])
                .with_context(|| format!("upgrade of {name} cancelled"))?;
            println!("Extracting package files...");
            self.extract_package(&archive_path)
                .with_context(|| format!("extraction failed while upgrading {name}, rolling back"))?;
            // Drop the files the new version no longer ships
            filelist::remove_files(&stale, &self.paths.target_root);
            self.test_interrupt_point("after-stale")?;
            self.store_scripts(name, &package_scripts)?;
            // Keep I or A
            let status: String = local_status.chars().filter(|c| !matches!(c, 'U' | 'X')).collect();
            util::atomic_write(&list_path, &new_list)?;
            self.set_state(record.with_status(&status))?;
            Ok(())
        })();
        if let Err(err) = result {
            self.abort_transaction();
            return Err(err);
        }
        self.commit_transaction()?;
        self.touch(&new_list, Op::Install);
        self.touch(&stale, Op::Remove);
        println!("{name} upgraded successfully!");
        self.run_late_script(name, &package_scripts, "post-install", &["upgrade", &version, &old_version]);
        self.install_companions(name).with_context(|| format!("installing the subpackages of {name}"))
    }

    /// Upgrades every package with an update available, dependencies first.
    pub fn upgrade_all(&mut self) -> Result<()> {
        self.require_state_file()?;
        self.load_state();
        self.load_repo();
        let candidates: Vec<String> = self
            .state
            .map
            .values()
            .filter(|r| r.status.contains('U') && r.is_installed())
            .map(|r| r.name.clone())
            .collect();
        // Subpackages of the configured groups that installed packages lack
        // (GROUP= added since, or a new translation)
        let installed: Vec<String> =
            self.state.map.values().filter(|r| r.is_installed()).map(|r| r.name.clone()).collect();
        let companions: Vec<String> = installed.iter().flat_map(|p| self.missing_companions(p)).collect();
        if candidates.is_empty() && companions.is_empty() {
            println!("No packages need upgrading.");
            return Ok(());
        }
        let order = self.upgrade_order(&candidates);
        let version = |r: Option<&Record>| r.map(|r| format!("{}-{}", r.version, r.release)).unwrap_or_default();
        if !order.is_empty() {
            println!("Packages available for upgrade (in upgrade order):");
            let width = order.iter().map(String::len).max().unwrap_or(0);
            for pkg in &order {
                println!("  {pkg:<width$}  {} -> {}", version(self.state.get(pkg)), version(self.repo.get(pkg)));
            }
        }
        if !companions.is_empty() {
            println!("Packages to add for GROUP {}: {}", self.groups.join(" "), companions.join(" "));
        }
        // Dependencies the new versions add (a package whose dependencies
        // cannot be resolved fails on its own later)
        let mut new_deps: Vec<String> = Vec::new();
        for pkg in &order {
            let deps: Vec<String> =
                self.repo.get(pkg).map(|r| r.deps().map(String::from).collect()).unwrap_or_default();
            let deps: Vec<&str> = deps.iter().map(String::as_str).collect();
            for dep in self.resolve_install(pkg, &deps).unwrap_or_default() {
                if !order.contains(&dep) && !new_deps.contains(&dep) {
                    new_deps.push(dep);
                }
            }
        }
        if !new_deps.is_empty() {
            println!("New dependencies to install: {}", new_deps.join(" "));
        }
        let all: Vec<String> = order.iter().chain(&new_deps).chain(&companions).cloned().collect();
        let all: Vec<String> = all.into_iter().filter(|p| self.repo.get(p).is_some()).collect();
        let (items, cached) = self.plan_downloads(&all)?;
        let (known, unknown) = download::total_size(&items);
        let in_cache = match (cached, items.is_empty()) {
            (0, _) => String::new(),
            (_, true) => " (all archives are in the cache)".to_string(),
            (n, false) => format!(" ({} already in the cache)", download::plural(n, "archive")),
        };
        println!("Download size: {}{in_cache}", download::describe_total(known, unknown));
        if !self.assume_yes && !util::ask("Do you want to upgrade these packages? [Y/n]", true) {
            println!("Upgrade cancelled.");
            return Ok(());
        }
        self.fetch_planned(&items)?;
        if !order.is_empty() {
            println!("Upgrading all packages with status 'U'...");
        }
        let mut failed = false;
        for pkg in &order {
            if let Err(err) = self.upgrade(pkg) {
                if err.downcast_ref::<Interrupted>().is_some() {
                    return Err(err);
                }
                util::report(&err);
                failed = true;
            }
        }
        // Those an upgrade did not already bring
        for pkg in installed.iter().filter(|p| !order.contains(p)) {
            if let Err(err) = self.install_companions(pkg) {
                if err.downcast_ref::<Interrupted>().is_some() {
                    return Err(err);
                }
                util::report(&err);
                failed = true;
            }
        }
        if failed { Err(crate::Reported.into()) } else { Ok(()) }
    }

    /// Removes a package. Packages that other installed packages depend on
    /// are refused, unless `skip_check` is set (autoremove).
    pub fn remove(&mut self, name: &str, skip_check: bool) -> Result<()> {
        println!("Removing package: {name}");
        let list_path = self.paths.files_dir.join(name);
        if !list_path.is_file() {
            bail!("File list for package {name} not found.");
        }
        self.load_state();
        self.load_repo();
        // Its translations and configured subpackages go with it
        let companions: Vec<String> =
            self.installed_companions(name).into_iter().filter(|c| self.installed_dependents(c).is_empty()).collect();
        if !skip_check {
            let dependents: Vec<String> =
                self.installed_dependents(name).into_iter().filter(|d| !companions.contains(d)).collect();
            if !dependents.is_empty() {
                bail!("Cannot remove {name}: required by the following installed packages:\n{}", dependents.join("\n"));
            }
        }
        for companion in &companions {
            self.remove(companion, true)?;
        }
        self.load_state();

        // A failing pre-remove script cancels the removal; the post-remove
        // one runs once the files are gone
        let scripts_dir = scripts::package_dir(&self.paths.scripts_dir, name);
        let mut package_scripts = HashMap::new();
        for script in ["pre-remove", "post-remove"] {
            if let Ok(content) = fs::read(scripts_dir.join(script)) {
                package_scripts.insert(script.to_string(), content);
            }
        }
        let version = self.state.get(name).map(|r| format!("{}-{}", r.version, r.release)).unwrap_or_default();
        self.run_script(name, &package_scripts, "pre-remove", &["remove", &version])
            .with_context(|| format!("removal of {name} cancelled"))?;

        let list = fs::read(&list_path)?;
        filelist::remove_files(&list, &self.paths.target_root);
        if exists_or_symlink(&scripts_dir) {
            fs::remove_dir_all(&scripts_dir).with_context(|| format!("unable to remove {}", scripts_dir.display()))?;
        }

        // Not installed any more, keeping the repository record
        if let Some(local) = self.state.get(name).cloned() {
            let status = if local.status.contains('X') { "NX" } else { "N" };
            let record = match self.repo.get(name) {
                Some(remote) => remote.with_status(status),
                None => local.with_status(status),
            };
            self.set_state(record)?;
        }
        self.test_interrupt_point("before-list-removal")?;

        // The file list goes last: an interrupted removal can simply be run
        // again
        fs::remove_file(&list_path)?;
        self.touch(&list, Op::Remove);
        println!("{name} removed successfully!");
        self.run_late_script(name, &package_scripts, "post-remove", &["remove", &version]);

        if !skip_check {
            let orphans = self.find_orphans();
            if !orphans.is_empty() {
                println!(
                    "{} orphan package(s) left: {}. Run '{} autoremove' to remove them.",
                    orphans.len(),
                    orphans.join(" "),
                    self.prog
                );
            }
        }
        Ok(())
    }

    pub fn list_orphans(&mut self) {
        let orphans = self.find_orphans();
        if orphans.is_empty() {
            println!("No orphan packages.");
        }
        for pkg in orphans {
            println!("{pkg}");
        }
    }

    /// Removes every orphan. Packages that depend on an orphan are orphans
    /// themselves, so no dependency check is needed.
    pub fn autoremove(&mut self) -> Result<()> {
        let orphans = self.find_orphans();
        if orphans.is_empty() {
            println!("No orphan packages.");
            return Ok(());
        }
        println!("Orphan packages:");
        for pkg in &orphans {
            println!("{pkg}");
        }
        if !self.assume_yes && !util::ask("Do you want to remove these packages? [Y/n]", true) {
            println!("Autoremove cancelled.");
            return Ok(());
        }
        let mut failed = false;
        for pkg in &orphans {
            // Already gone with its package (nano.fr with nano)
            if !self.paths.files_dir.join(pkg).is_file() {
                continue;
            }
            if let Err(err) = self.remove(pkg, true) {
                if err.downcast_ref::<Interrupted>().is_some() {
                    return Err(err);
                }
                util::report(&err);
                failed = true;
            }
        }
        if failed { Err(crate::Reported.into()) } else { Ok(()) }
    }

    // ---------------------------------------------------------------------
    // Scripts and hooks

    /// Stores the scripts of a package in `/var/lib/pq/scripts/<package>/`,
    /// replacing those of the previous version (inside its transaction).
    fn store_scripts(&self, pkg: &str, package_scripts: &HashMap<String, Vec<u8>>) -> Result<()> {
        let dir = scripts::package_dir(&self.paths.scripts_dir, pkg);
        for (name, _) in archive::SCRIPTS {
            let path = dir.join(name);
            if exists_or_symlink(&path) {
                fs::remove_file(&path).with_context(|| format!("unable to remove {}", path.display()))?;
            }
        }
        if package_scripts.is_empty() {
            let _ = fs::remove_dir(&dir);
            return Ok(());
        }
        fs::create_dir_all(&dir).with_context(|| format!("unable to create {}", dir.display()))?;
        for (name, content) in package_scripts {
            util::atomic_write(&dir.join(name), content)?;
        }
        Ok(())
    }

    /// Runs a script of a package, if it has one. Fails if the script fails.
    /// Scripts are skipped with `--noscripts`, or when the managed system has
    /// no `/bin/sh` yet (while it is being bootstrapped).
    fn run_script(
        &self,
        pkg: &str,
        package_scripts: &HashMap<String, Vec<u8>>,
        kind: &str,
        args: &[&str],
    ) -> Result<()> {
        let Some(content) = package_scripts.get(kind) else { return Ok(()) };
        if self.noscripts {
            println!("Skipping the {kind} script of {pkg} (--noscripts)");
            return Ok(());
        }
        let root = self.paths.root_dir();
        let Some(cmd) = scripts::shell(&self.paths.target_root, &root) else {
            println!("Warning: no /bin/sh in {}, the {kind} script of {pkg} is skipped", root.display());
            return Ok(());
        };
        println!("Running the {kind} script of {pkg}...");
        scripts::run(cmd, content, &format!("{pkg}.{kind}"), args, &[("PQ_PACKAGE", pkg)])
            .with_context(|| format!("the {kind} script of {pkg} failed"))
    }

    /// Runs a script once its package is committed: a failure is reported,
    /// and makes the command fail, but the operation stays done.
    fn run_late_script(&mut self, pkg: &str, package_scripts: &HashMap<String, Vec<u8>>, kind: &str, args: &[&str]) {
        if let Err(err) = self.run_script(pkg, package_scripts, kind, args) {
            util::report(&err);
            self.script_failed = true;
        }
    }

    /// Records the paths of a file list for the hooks.
    fn touch(&mut self, list: &[u8], op: Op) {
        for (path, _) in filelist::entries(list, &self.paths.target_root) {
            self.touched.push((path, op));
        }
    }

    /// Runs the hooks triggered by the files installed or removed so far,
    /// once each.
    pub fn run_hooks(&mut self) {
        let touched = std::mem::take(&mut self.touched);
        if touched.is_empty() || self.noscripts {
            return;
        }
        let root = self.paths.root_dir();
        for hook in scripts::load_hooks(&self.paths.hooks_dir) {
            if !hook.triggered_by(&touched) {
                continue;
            }
            let label = if hook.description.is_empty() { hook.name.clone() } else { hook.description.clone() };
            let Some(cmd) = scripts::shell(&self.paths.target_root, &root) else {
                println!("Warning: no /bin/sh in {}, hook {} skipped", root.display(), hook.name);
                continue;
            };
            println!("Running hook: {label}");
            if let Err(err) = scripts::run(cmd, hook.exec.as_bytes(), &hook.name, &[], &[]) {
                util::report(&err.context(format!("hook {} failed", hook.name)));
                self.script_failed = true;
            }
        }
    }

    /// True if a script or a hook failed after its package was committed.
    pub fn scripts_failed(&self) -> bool {
        self.script_failed
    }

    // ---------------------------------------------------------------------
    // Queries

    /// A path given on the command line, absolute and normalized, relative
    /// to the managed system (`--chroot`).
    fn system_path(&self, arg: &str) -> Vec<u8> {
        let mut joined = Vec::new();
        if !arg.starts_with('/') && self.paths.target_root.is_empty() {
            if let Ok(cwd) = std::env::current_dir() {
                joined.extend_from_slice(cwd.as_os_str().as_bytes());
            }
        }
        joined.push(b'/');
        joined.extend_from_slice(arg.as_bytes());
        let mut parts: Vec<&[u8]> = Vec::new();
        for part in joined.split(|&b| b == b'/') {
            match part {
                b"" | b"." => {}
                b".." => {
                    parts.pop();
                }
                _ => parts.push(part),
            }
        }
        let mut out = Vec::new();
        for part in &parts {
            out.push(b'/');
            out.extend_from_slice(part);
        }
        if out.is_empty() {
            out.push(b'/');
        }
        out
    }

    /// Shows the packages owning a file.
    pub fn query(&mut self, arg: &str) -> Result<()> {
        self.load_state();
        let wanted = self.system_path(arg);
        let mut candidates = vec![wanted.clone()];
        // Also through symlinks: /bin/ls may be /usr/bin/ls in the lists
        let root = self.paths.root_dir();
        if let (Ok(real), Ok(root_real)) =
            (fs::canonicalize(concat_path(&self.paths.target_root, &wanted)), fs::canonicalize(&root))
        {
            if let Ok(rel) = real.strip_prefix(&root_real) {
                let mut path = b"/".to_vec();
                path.extend_from_slice(rel.as_os_str().as_bytes());
                if !candidates.contains(&path) {
                    candidates.push(path);
                }
            }
        }
        let mut owners = Vec::new();
        for list in util::files_with_suffix(&self.paths.files_dir, "") {
            let pkg = list.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let content = fs::read(&list).unwrap_or_default();
            if let Some((path, _)) =
                filelist::entries(&content, &self.paths.target_root).into_iter().find(|(p, _)| candidates.contains(p))
            {
                owners.push((pkg, path));
            }
        }
        if owners.is_empty() {
            bail!("no package owns {arg}.");
        }
        for (pkg, path) in owners {
            let version = self.state.get(&pkg).map(|r| format!(" {}-{}", r.version, r.release)).unwrap_or_default();
            println!("{} is owned by {pkg}{version}", concat_path(&self.paths.target_root, &path).display());
        }
        Ok(())
    }

    /// Lists the files of an installed package.
    pub fn files(&self, name: &str) -> Result<()> {
        let list =
            fs::read(self.paths.files_dir.join(name)).map_err(|_| anyhow!("Package {name} is not installed."))?;
        for (_, real) in filelist::entries(&list, &self.paths.target_root) {
            println!("{}", real.display());
        }
        Ok(())
    }

    /// Shows what is known about a package, installed or not.
    pub fn info(&mut self, name: &str) -> Result<()> {
        self.load_state();
        self.load_repo();
        let local = self.state.get(name).filter(|r| r.is_installed()).cloned();
        let remote = self.repo.get(name).cloned();
        let Some(record) = local.clone().or_else(|| remote.clone()) else {
            bail!("Package {name} not found.");
        };
        let status = self.state.get(name).map(|r| r.status.clone()).unwrap_or_else(|| "N".into());
        let or_dash = |s: &str| if s.is_empty() { "-".to_string() } else { s.to_string() };
        let row = |label: &str, value: &str| println!("{label:<15}: {value}");
        row("Name", &record.name);
        row("Version", &format!("{}-{}", record.version, record.release));
        row("Description", &or_dash(&record.description));
        row("Set", &or_dash(&record.set));
        row("Folder", &or_dash(&record.folder));
        row("Groups", &or_dash(&record.groups));
        row("Depends", &or_dash(&record.depends));
        row("Status", &describe_status(&status));
        if let (true, Some(remote)) = (status.contains('U'), &remote) {
            row("Available", &format!("{}-{}", remote.version, remote.release));
        }
        if local.is_some() {
            row("Required by", &or_dash(&self.installed_dependents(name).join(" ")));
            let list = fs::read(self.paths.files_dir.join(name)).unwrap_or_default();
            let (mut files, mut size) = (0, 0);
            for (_, real) in filelist::entries(&list, &self.paths.target_root) {
                if let Ok(meta) = fs::symlink_metadata(&real) {
                    if !meta.is_dir() {
                        files += 1;
                        size += meta.len();
                    }
                }
            }
            row("Files", &files.to_string());
            row("Installed size", &if size < 1024 { format!("{size} B") } else { util::human_size(size) });
            let dir = scripts::package_dir(&self.paths.scripts_dir, name);
            let stored: Vec<&str> =
                archive::SCRIPTS.iter().map(|(n, _)| *n).filter(|n| dir.join(n).is_file()).collect();
            row("Scripts", &or_dash(&stored.join(" ")));
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Cache

    fn cached_archives(&self) -> Vec<PathBuf> {
        util::files_with_suffix(&self.paths.cache_dir, ".tar.zst")
    }

    /// Cached archives superseded by a newer cached version of the same
    /// package.
    fn find_old_archives(&self) -> Vec<PathBuf> {
        let mut versions: BTreeMap<String, Vec<(String, PathBuf)>> = BTreeMap::new();
        for path in self.cached_archives() {
            let Some(file) = path.file_name().and_then(|f| f.to_str()) else { continue };
            let Some(base) = file.strip_suffix(".tar.zst") else { continue };
            let Some((name, version, release)) = util::split_archive_base(base) else { continue };
            if util::is_valid_name(&name) {
                versions.entry(name).or_default().push((format!("{version}-{release}"), path.clone()));
            }
        }
        let mut old = Vec::new();
        for list in versions.values() {
            let newest = version::newest(list.iter().map(|(v, _)| v.as_str())).unwrap_or_default();
            old.extend(list.iter().filter(|(v, _)| *v != newest).map(|(_, p)| p.clone()));
        }
        old
    }

    pub fn cache_info(&self) {
        let cache = &self.paths.cache_dir;
        println!("Cache directory: {}", cache.display());
        let archives = self.cached_archives();
        if archives.is_empty() {
            println!("The cache is empty.");
            return;
        }
        for path in &archives {
            println!(
                "{}\t{}",
                util::human_size(util::disk_usage(std::slice::from_ref(path))),
                path.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        println!("Total: {} archive(s), {}", archives.len(), util::human_size(util::disk_usage(&archives)));
        let old = self.find_old_archives();
        if !old.is_empty() {
            println!(
                "Old versions: {} archive(s), {} (run '{} --purge-old-cache' to remove them)",
                old.len(),
                util::human_size(util::disk_usage(&old)),
                self.prog
            );
        }
    }

    pub fn purge_old_cache(&self) {
        let old = self.find_old_archives();
        if old.is_empty() {
            println!("No old archives in the cache.");
            return;
        }
        let freed = util::human_size(util::disk_usage(&old));
        for path in &old {
            println!("Removing {}", path.file_name().unwrap_or_default().to_string_lossy());
            let _ = fs::remove_file(path);
        }
        println!("Removed {} old archive(s), {freed} freed.", old.len());
    }

    /// Shows the cache size and offers to clear it.
    pub fn manage_cache(&self) {
        let cache = &self.paths.cache_dir;
        let entries: Vec<PathBuf> = fs::read_dir(cache).into_iter().flatten().flatten().map(|e| e.path()).collect();
        let mut size = util::disk_usage(std::slice::from_ref(&cache.to_path_buf()));
        for entry in &entries {
            size += dir_usage(entry);
        }
        println!("Cache size: {}", util::human_size(size));
        if !self.assume_yes && !util::ask("Do you want to clear the cache? (y/N):", false) {
            println!("Cache not cleared.");
            return;
        }
        if !cache.is_dir() {
            println!("Cache directory {} is not accessible; nothing was removed.", cache.display());
            return;
        }
        for entry in entries {
            let _ = if is_real_dir(&entry) { fs::remove_dir_all(&entry) } else { fs::remove_file(&entry) };
        }
        println!("Cache cleared successfully!");
    }
}

/// Disk usage of a file or a directory tree.
fn dir_usage(path: &Path) -> u64 {
    let mut total = util::disk_usage(&[path.to_path_buf()]);
    if is_real_dir(path) {
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            total += dir_usage(&entry.path());
        }
    }
    total
}

/// Scripts of an archive, by name (`pre-install`...).
fn read_scripts(archive_path: &Path) -> Result<HashMap<String, Vec<u8>>> {
    let entries: Vec<&str> = archive::SCRIPTS.iter().map(|(_, e)| *e).collect();
    let found = archive::read_metadata(archive_path, &entries)?;
    Ok(archive::SCRIPTS
        .iter()
        .filter_map(|(name, entry)| found.get(*entry).map(|c| (name.to_string(), c.clone())))
        .collect())
}

/// A status in words: "I (installed explicitly, update available)".
fn describe_status(status: &str) -> String {
    let mut words = Vec::new();
    if status.contains('I') {
        words.push("installed explicitly");
    } else if status.contains('A') {
        words.push("installed as a dependency");
    } else {
        words.push("not installed");
    }
    if status.contains('U') {
        words.push("update available");
    }
    if status.contains('X') {
        words.push("no longer in the repository");
    }
    format!("{status} ({})", words.join(", "))
}
