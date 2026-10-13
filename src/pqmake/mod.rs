//! `pqmake`: builds Great-OS packages from their recipes, dependencies first.
//!
//! A recipe is a directory holding a `Pkgfile` (in the style of CRUX):
//! `<recipes>/<set>/<name>/Pkgfile`. Each build produces
//! `<name>-<version>-<release>.tar.zst` archives, ready for `genpqrepo` and
//! `paquet`, in `<repo>/<set>/`. A recipe whose archive is already there for
//! its version and release is not built again.
//!
//! With `--chroot <rootfs>`, each recipe is built in a throwaway overlay on
//! top of `<rootfs>` (a base build system, never modified), where its
//! makedepends are first installed with `paquet` from the repository being
//! built. Without it, recipes are built on the host, as a regular user.

mod build;
pub mod footprint;
mod package;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::util::{self, is_valid_name, is_valid_version};
use crate::{config, signals};

/// Subpackages a recipe can split off with `PKGMK_GROUPS`: `<name>.<group>`.
/// Translations always go to one subpackage per language, `<name>.<language>`.
pub const KNOWN_GROUPS: [&str; 5] = util::PACKAGE_GROUPS;

pub fn is_known_group(group: &str) -> bool {
    KNOWN_GROUPS.contains(&group)
}

/// Code run before a Pkgfile is used, in a separate bash: the default
/// `PKGMK_GROUPS`, and the `build()` of recipes without one: their build
/// style (`$PQMAKE_STYLE_FILE`, see [`Pqmake::find_style`]), or else the
/// built-in autotools build. `PQMAKE_GROUPS` and `PKGMK_ARCH` come from the
/// environment.
pub const RECIPE_PRELUDE: &str = r#"
read -ra PKGMK_GROUPS <<< "$PQMAKE_GROUPS"
source "$1" > /dev/null || exit 1
PQMAKE_BUILD_FUNCTION=$(type -t build || true)
if [[ "$PQMAKE_BUILD_FUNCTION" != function && -n "${PQMAKE_STYLE_FILE:-}" ]]; then
    build() {
        source "$PQMAKE_STYLE_FILE"
    }
elif [[ "$PQMAKE_BUILD_FUNCTION" != function ]]; then
    build() {
        cd "$name-$version"
        ./configure --prefix=/usr
        make
        make DESTDIR="$PKG" install
        rm -f "$PKG/usr/share/info/dir"
    }
fi
"#;

/// Prints the variables of a recipe, one per line.
const RECIPE_FIELDS: &str = r#"
printf "%s\n" "${name:-}" "${version:-}" "${release:-1}" \
    "${description//$'\n'/ }" "${makedepends[*]}" "${run[*]}" \
    "${run_devel[*]}" "${PKGMK_GROUPS[*]}" "${source[*]}" \
    "${PKGMK_IGNORE_RUNTIMEDEPS:-no}" "${build:-}" "$PQMAKE_BUILD_FUNCTION" \
    "${PKGMK_IGNORE_GROUPS[*]}"
"#;

/// Build of a recipe: prepare(), build() and package(), when defined, each
/// run from $SRC with set -e, like pkgmk.
pub fn recipe_build_script() -> String {
    format!(
        "set -e\numask 022\n{RECIPE_PRELUDE}\n\
         for step in prepare build package; do\n    \
         [[ \"$(type -t \"$step\")\" == function ]] || continue\n    \
         echo \"==> $step()\"\n    \
         (cd \"$SRC\"; \"$step\")\n\
         done\n"
    )
}

/// Settings, from the configuration file then the command line.
#[derive(Clone, Debug)]
pub struct Settings {
    pub recipes_dir: PathBuf,
    pub repo_dir: PathBuf,
    pub work_dir: PathBuf,
    pub sources_dir: PathBuf,
    pub log_dir: PathBuf,
    pub makeflags: String,
    pub cflags: String,
    pub cxxflags: String,
    /// Base build system for `--chroot` builds.
    pub chroot: Option<PathBuf>,
    /// Groups of a recipe that does not set `PKGMK_GROUPS`.
    pub default_groups: String,
    /// Shared build styles (`build=<style>`), after `<recipes>/styles`.
    pub styles_dir: PathBuf,
    /// Sources installed by `<name>.sources` packages, looked up before
    /// downloading.
    pub usr_src_dir: PathBuf,
    pub dry_run: bool,
    pub keep_going: bool,
    pub quiet: bool,
    pub keep_work: bool,
    pub update_index: bool,
    pub build_all: bool,
    pub force_targets: bool,
    /// Rewrite the footprints of the targets from their archives.
    pub update_footprint: bool,
    pub targets: Vec<String>,
}

impl Settings {
    pub fn defaults(arch: &str) -> Settings {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let base = home.join("pqmake");
        let jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        Settings {
            recipes_dir: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            repo_dir: base.join("repo/stable").join(arch),
            work_dir: base.join("work"),
            sources_dir: base.join("sources"),
            log_dir: base.join("logs"),
            makeflags: std::env::var("MAKEFLAGS").ok().filter(|v| !v.is_empty()).unwrap_or(format!("-j{jobs}")),
            cflags: "-O2 -pipe".into(),
            cxxflags: String::new(),
            chroot: None,
            default_groups: "devel man doc service sources".into(),
            styles_dir: "/etc/pqmake/styles".into(),
            usr_src_dir: "/usr/src".into(),
            dry_run: false,
            keep_going: false,
            quiet: false,
            keep_work: false,
            update_index: false,
            build_all: false,
            force_targets: false,
            update_footprint: false,
            targets: Vec::new(),
        }
    }

    /// Applies a configuration file (read, never executed).
    pub fn read_config(&mut self, path: &Path) -> Result<()> {
        let home = std::env::var("HOME").unwrap_or_default();
        for (key, mut value) in config::read(path)? {
            // ~ is expanded for convenience
            if value == "~" || value.starts_with("~/") {
                value = format!("{home}{}", &value[1..]);
            }
            match key.as_str() {
                "RECIPES_DIR" => self.recipes_dir = value.into(),
                "REPO_DIR" => self.repo_dir = value.into(),
                "WORK_DIR" => self.work_dir = value.into(),
                "SOURCES_DIR" => self.sources_dir = value.into(),
                "LOG_DIR" => self.log_dir = value.into(),
                "MAKEFLAGS" => self.makeflags = value,
                "CFLAGS" => self.cflags = value,
                "CXXFLAGS" => self.cxxflags = value,
                "PKGMK_GROUPS" => self.default_groups = value,
                "STYLES_DIR" => self.styles_dir = value.into(),
                "USR_SRC_DIR" => self.usr_src_dir = value.into(),
                "CHROOT" => self.chroot = Some(value.into()).filter(|p: &PathBuf| !p.as_os_str().is_empty()),
                _ => eprintln!("Warning: ignoring unknown setting {key} in {}", path.display()),
            }
        }
        Ok(())
    }
}

/// Variables of a recipe.
#[derive(Clone, Debug, Default)]
pub struct Recipe {
    pub version: String,
    pub release: String,
    pub description: String,
    pub makedepends: Vec<String>,
    pub run: Vec<String>,
    pub run_devel: Vec<String>,
    pub groups: Vec<String>,
    /// Languages kept in the main package (`PKGMK_IGNORE_GROUPS=(de)`)
    pub ignored_languages: Vec<String>,
    pub sources: Vec<String>,
    pub no_rtdeps: bool,
    /// Build style file, for a recipe without build() function
    pub style: Option<PathBuf>,
}

pub struct Pqmake {
    pub s: Settings,
    pub arch: String,
    /// Recipe name -> Pkgfile path
    pub recipe_path: BTreeMap<String, PathBuf>,
    /// Recipe name -> set (folder in the repository)
    pub recipe_set: HashMap<String, String>,
    pub recipes: HashMap<String, Recipe>,
    /// Recipes to consider, in discovery order
    pub needed: Vec<String>,
    /// Recipe -> recipes that must be built before it
    pub before: HashMap<String, Vec<String>>,
    /// Dependencies without a recipe
    pub external: BTreeSet<String>,
    /// Build order
    pub order: Vec<String>,
    pub force: HashSet<String>,
    /// Mount points of the current chroot build, unmounted on exit
    mounts: Vec<PathBuf>,
    /// The repository index must be regenerated before installing from it
    index_dirty: bool,
    /// Recipes whose new archives differ from their footprints
    footprint_changed: Vec<String>,
    _lock: Option<fs::File>,
}

impl Drop for Pqmake {
    fn drop(&mut self) {
        let _ = self.unmount_all();
    }
}

fn die(msg: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{msg}")
}

impl Pqmake {
    pub fn new(s: Settings, arch: String) -> Pqmake {
        Pqmake {
            s,
            arch,
            recipe_path: BTreeMap::new(),
            recipe_set: HashMap::new(),
            recipes: HashMap::new(),
            needed: Vec::new(),
            before: HashMap::new(),
            external: BTreeSet::new(),
            order: Vec::new(),
            force: HashSet::new(),
            mounts: Vec::new(),
            index_dirty: true,
            footprint_changed: Vec::new(),
            _lock: None,
        }
    }

    /// Indexes the recipes: `<recipes>/<set>/<name>/Pkgfile`, or
    /// `<set>/<name>/Pkgfile` when the recipes directory is itself a set. The
    /// directory name is the recipe name; the directory above it is the set.
    pub fn index_recipes(&mut self) -> Result<()> {
        let dir = &self.s.recipes_dir;
        if !dir.is_dir() {
            bail!("recipes directory {} not found.", dir.display());
        }
        let mut pkgfiles = Vec::new();
        for first in read_dir_sorted(dir) {
            if !first.is_dir() {
                continue;
            }
            let candidate = first.join("Pkgfile");
            if candidate.is_file() {
                pkgfiles.push(candidate);
            }
            for second in read_dir_sorted(&first) {
                let candidate = second.join("Pkgfile");
                if second.is_dir() && candidate.is_file() {
                    pkgfiles.push(candidate);
                }
            }
        }
        pkgfiles.sort_by(|a, b| util::path_bytes(a).cmp(util::path_bytes(b)));
        for pkgfile in pkgfiles {
            let recipe_dir = pkgfile.parent().unwrap_or(Path::new("."));
            let name = recipe_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !is_valid_name(&name) {
                eprintln!("Warning: ignoring {} (invalid recipe name '{name}')", pkgfile.display());
                continue;
            }
            if let Some(other) = self.recipe_path.get(&name) {
                bail!("two recipes are named {name}: {} and {}", other.display(), pkgfile.display());
            }
            let set = recipe_dir
                .parent()
                .and_then(|p| fs::canonicalize(p).ok().or(Some(p.to_path_buf())))
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();
            self.recipe_set.insert(name.clone(), set);
            self.recipe_path.insert(name, pkgfile);
        }
        if self.recipe_path.is_empty() {
            bail!("no recipe (*/*/Pkgfile) found in {}.", dir.display());
        }
        Ok(())
    }

    /// Reads the variables of a recipe. The Pkgfile is sourced by a separate
    /// bash, from its own directory, so that it cannot change our state.
    pub fn read_recipe(&mut self, name: &str) -> Result<()> {
        if self.recipes.contains_key(name) {
            return Ok(());
        }
        let pkgfile = self.recipe_path[name].clone();
        let dir = pkgfile.parent().unwrap_or(Path::new("."));
        let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let output = Command::new("bash")
            .arg("-c")
            .arg(format!("{RECIPE_PRELUDE}{RECIPE_FIELDS}"))
            .arg("pqmake")
            .arg("./Pkgfile")
            .current_dir(&dir)
            .env("PQMAKE_GROUPS", &self.s.default_groups)
            .env("PKGMK_ARCH", &self.arch)
            .env("PKGMK_ROOT", &dir)
            .env_remove("PQMAKE_STYLE_FILE")
            .stdin(std::process::Stdio::null())
            .output()
            .context("unable to run bash")?;
        let text = String::from_utf8_lossy(&output.stdout);
        let fields: Vec<&str> = text.lines().collect();
        if fields.len() != 13 {
            bail!("cannot read {}.", pkgfile.display());
        }
        // "-" separates the fields of an archive name: 1.3-20240619 becomes
        // 1.3_20240619 in the archive and the index
        let version = fields[1].replace('-', "_");
        if fields[0] != name {
            bail!("{}: name={} does not match its directory ({name}).", pkgfile.display(), fields[0]);
        }
        if !is_valid_version(&version) {
            bail!("{}: invalid version '{version}'.", pkgfile.display());
        }
        if !is_valid_version(fields[2]) {
            bail!("{}: invalid release '{}'.", pkgfile.display(), fields[2]);
        }
        let mut groups = Vec::new();
        for group in fields[7].split_whitespace() {
            if is_known_group(group) {
                groups.push(group.to_string());
            } else {
                eprintln!(
                    "Warning: {}: group '{group}' is not supported, ignored (known: {}).",
                    pkgfile.display(),
                    KNOWN_GROUPS.join(" ")
                );
            }
        }
        // PKGMK_IGNORE_GROUPS=(sources) removes groups from the default (or
        // the recipe's) PKGMK_GROUPS; a language stays in the main package
        let mut ignored_languages = Vec::new();
        for group in fields[12].split_whitespace() {
            if is_known_group(group) {
                groups.retain(|g| g != group);
            } else if util::is_language(group) {
                ignored_languages.push(group.to_string());
            } else {
                eprintln!(
                    "Warning: {}: PKGMK_IGNORE_GROUPS: unknown group '{group}' (known: {}, or a language).",
                    pkgfile.display(),
                    KNOWN_GROUPS.join(" ")
                );
            }
        }
        let style = if fields[11] == "function" { None } else { self.find_style(fields[10], &pkgfile)? };
        let words = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        self.recipes.insert(
            name.to_string(),
            Recipe {
                version,
                release: fields[2].to_string(),
                description: fields[3].to_string(),
                makedepends: words(fields[4]),
                run: words(fields[5]),
                run_devel: words(fields[6]),
                groups,
                ignored_languages,
                sources: words(fields[8]),
                no_rtdeps: fields[9] == "yes",
                style,
            },
        );
        Ok(())
    }

    /// Build style of a recipe without build() function: `build=<style>`, or
    /// `default` when unset. The style is a bash file whose commands make the
    /// build() of the recipe, looked up in `<recipes>/styles/` then in
    /// `STYLES_DIR`. Without a style file, `default` and `autotools` use the
    /// built-in autotools build.
    pub fn find_style(&self, build: &str, pkgfile: &Path) -> Result<Option<PathBuf>> {
        let style = if build.is_empty() { "default" } else { build };
        if !is_valid_name(style) {
            bail!("{}: invalid build style '{style}'.", pkgfile.display());
        }
        let dirs = [self.s.recipes_dir.join("styles"), self.s.styles_dir.clone()];
        if let Some(file) = dirs.iter().map(|d| d.join(style)).find(|f| f.is_file()) {
            return Ok(Some(file));
        }
        if matches!(style, "default" | "autotools") {
            return Ok(None);
        }
        bail!(
            "{}: build style '{style}' not found (looked in {} and {}).",
            pkgfile.display(),
            dirs[0].display(),
            dirs[1].display()
        )
    }

    pub fn recipe(&self, name: &str) -> &Recipe {
        &self.recipes[name]
    }

    /// Recipe providing a dependency: curl, curl.devel, curl>=8 all map to
    /// curl.
    pub fn dep_recipe(&self, dep: &str) -> Option<String> {
        let dep = crate::record::dep_name(dep);
        if self.recipe_path.contains_key(dep) {
            return Some(dep.to_string());
        }
        let (base, _) = util::split_group(dep)?;
        self.recipe_path.contains_key(base).then(|| base.to_string())
    }

    /// Every recipe the targets need, at build time or at run time.
    pub fn collect_needed(&mut self, targets: &[String]) -> Result<()> {
        let mut queue: VecDeque<String> = targets.iter().cloned().collect();
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(name) = queue.pop_front() {
            if !seen.insert(name.clone()) {
                continue;
            }
            self.needed.push(name.clone());
            self.read_recipe(&name)?;
            let r = self.recipe(&name);
            let deps: Vec<String> = r.makedepends.iter().chain(&r.run).chain(&r.run_devel).cloned().collect();
            for dep in deps {
                match self.dep_recipe(&dep) {
                    Some(recipe) => queue.push_back(recipe),
                    None => {
                        self.external.insert(crate::record::dep_name(&dep).to_string());
                    }
                }
            }
        }
        Ok(())
    }

    /// Recipes a recipe needs at run time, directly or not (run and
    /// run_devel), itself included.
    fn run_closure(&self, start: &str) -> Vec<String> {
        let mut queue = VecDeque::from([start.to_string()]);
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        while let Some(name) = queue.pop_front() {
            if !seen.insert(name.clone()) {
                continue;
            }
            out.push(name.clone());
            let r = self.recipe(&name);
            for dep in r.run.iter().chain(&r.run_devel) {
                if let Some(d) = self.dep_recipe(dep) {
                    queue.push_back(d);
                }
            }
        }
        out
    }

    /// A recipe is built after its makedepends and everything they need at
    /// run time: they must all be installable when it is built. Its own run
    /// dependencies only have to be built too, in any order, so that a
    /// runtime cycle (a needs b, b needs a) does not prevent building.
    pub fn compute_before(&mut self) {
        let mut before = HashMap::new();
        for name in &self.needed {
            let mut list: Vec<String> = Vec::new();
            for make in &self.recipe(name).makedepends {
                let Some(make) = self.dep_recipe(make) else { continue };
                for dep in self.run_closure(&make) {
                    if &dep != name && !list.contains(&dep) {
                        list.push(dep);
                    }
                }
            }
            before.insert(name.clone(), list);
        }
        self.before = before;
    }

    /// Depth-first topological sort on `before`; reports the first cycle
    /// found.
    pub fn sort_recipes(&mut self) -> Result<()> {
        #[derive(PartialEq)]
        enum Mark {
            Active,
            Done,
        }
        fn visit(
            name: &str,
            before: &HashMap<String, Vec<String>>,
            marks: &mut HashMap<String, Mark>,
            stack: &mut Vec<String>,
            order: &mut Vec<String>,
        ) -> Result<()> {
            match marks.get(name) {
                Some(Mark::Done) => return Ok(()),
                Some(Mark::Active) => {
                    let start = stack.iter().rposition(|n| n == name).unwrap_or(0);
                    let cycle = stack[start..].join(" -> ");
                    bail!(
                        "dependency cycle: {cycle} -> {name}\n\
                         (each recipe needs the next one installed to be built)"
                    );
                }
                None => {}
            }
            marks.insert(name.to_string(), Mark::Active);
            stack.push(name.to_string());
            for dep in before.get(name).into_iter().flatten() {
                visit(dep, before, marks, stack, order)?;
            }
            stack.pop();
            marks.insert(name.to_string(), Mark::Done);
            order.push(name.to_string());
            Ok(())
        }
        let mut marks = HashMap::new();
        let mut stack = Vec::new();
        let mut order = Vec::new();
        for name in &self.needed {
            visit(name, &self.before, &mut marks, &mut stack, &mut order)?;
        }
        self.order = order;
        Ok(())
    }

    /// Archive of a recipe (or of one of its groups) in the repository.
    pub fn archive_path(&self, name: &str, pkgname: &str) -> PathBuf {
        let r = self.recipe(name);
        self.s.repo_dir.join(&self.recipe_set[name]).join(util::archive_file_name(pkgname, &r.version, &r.release))
    }

    /// Subpackages of a recipe's current version in the repository, as
    /// (group or language, archive).
    pub fn existing_subpackages(&self, name: &str) -> Vec<(String, PathBuf)> {
        let r = self.recipe(name);
        let dir = self.s.repo_dir.join(&self.recipe_set[name]);
        let prefix = format!("{name}-{}-{}-", r.version, r.release);
        util::files_with_suffix(&dir, ".tar.zst")
            .into_iter()
            .filter_map(|path| {
                let file = path.file_name()?.to_str()?;
                let group = file.strip_prefix(&prefix)?.strip_suffix(".tar.zst")?;
                util::is_package_group(group).then(|| (group.to_string(), path.clone()))
            })
            .collect()
    }

    /// The main archive is written last: its presence means the build is
    /// complete.
    pub fn is_built(&self, name: &str) -> bool {
        self.archive_path(name, name).is_file()
    }

    pub fn needs_build(&self, name: &str) -> bool {
        self.force.contains(name) || !self.is_built(name)
    }

    pub fn print_plan(&self) {
        let to_build = self.order.iter().filter(|n| self.needs_build(n)).count();
        println!("Build plan: {} recipe(s), {to_build} to build", self.order.len());
        for name in &self.order {
            let (action, note) = if self.needs_build(name) {
                ("build", if self.force.contains(name) && self.is_built(name) { "(forced)" } else { "" })
            } else {
                ("skip", "(up to date)")
            };
            let r = self.recipe(name);
            let note = if note.is_empty() { String::new() } else { format!(" {note}") };
            println!("  {action:<6} {name} {}-{}{note}", r.version, r.release);
        }
        if !self.external.is_empty() {
            println!(
                "Without recipe, assumed provided by the build system: {}",
                self.external.iter().cloned().collect::<Vec<_>>().join(" ")
            );
        }
    }

    fn acquire_lock(&mut self) -> Result<()> {
        fs::create_dir_all(&self.s.work_dir)
            .with_context(|| format!("unable to create {}", self.s.work_dir.display()))?;
        let lock = self.s.work_dir.join(".lock");
        match util::try_lock(&lock)? {
            Some(file) => self._lock = Some(file),
            None => bail!("another pqmake is running (lock: {}).", lock.display()),
        }
        Ok(())
    }

    fn check_setup(&mut self) -> Result<()> {
        if !command_exists("tar") {
            bail!("tar is not available (needed to unpack the sources).");
        }
        if let Some(chroot) = self.s.chroot.clone() {
            if util::euid() != 0 {
                bail!("building in a chroot needs root.");
            }
            if !chroot.join("bin/bash").is_file() {
                bail!("{} does not look like a build system (no /bin/bash).", chroot.display());
            }
            let chroot = fs::canonicalize(&chroot)?;
            for (label, dir) in [("WORK_DIR", &mut self.s.work_dir), ("REPO_DIR", &mut self.s.repo_dir)] {
                fs::create_dir_all(&*dir).with_context(|| format!("unable to create {}", dir.display()))?;
                *dir = fs::canonicalize(&*dir)?;
                if dir.starts_with(&chroot) {
                    bail!("{label} must be outside the chroot.");
                }
            }
            self.s.chroot = Some(chroot);
        } else if util::euid() == 0 && std::env::var("PQMAKE_ALLOW_ROOT").as_deref() != Ok("1") {
            // A mistake in build() (a missing $PKG, say) must not reach the
            // system
            bail!("packages are not built as root on the host; use --chroot <rootfs>.");
        }
        Ok(())
    }

    /// Builds the recipes of the plan that need it. After a failure, goes on
    /// with the recipes that do not need the failed one when `keep_going`.
    fn build_all(&mut self) -> Result<()> {
        let mut built = 0;
        let mut failed: Vec<String> = Vec::new();
        let mut broken: BTreeSet<String> = BTreeSet::new();
        for name in self.order.clone() {
            if !self.needs_build(&name) {
                continue;
            }
            if let Some(dep) = self.before[&name].iter().find(|d| broken.contains(*d)) {
                eprintln!("==> Skipping {name}: {dep} was not built");
                broken.insert(name);
                continue;
            }
            let ok = self.build_recipe(&name)?;
            signals::check()?;
            if ok {
                built += 1;
            } else {
                broken.insert(name.clone());
                failed.push(name);
                if !self.s.keep_going {
                    break;
                }
            }
        }
        println!("==> {built} recipe(s) built");
        if !self.footprint_changed.is_empty() {
            println!("==> Footprint changed: {} (see above)", self.footprint_changed.join(" "));
        }
        if !broken.is_empty() {
            bail!("failed: {}; not built: {}", failed.join(" "), broken.iter().cloned().collect::<Vec<_>>().join(" "));
        }
        Ok(())
    }
}

pub(crate) fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
    entries.sort_by(|a, b| util::path_bytes(a).cmp(util::path_bytes(b)));
    entries
}

pub fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").map(|paths| std::env::split_paths(&paths).any(|p| p.join(name).is_file())).unwrap_or(false)
}

pub fn display_help(prog: &str, config_file: &Path) {
    println!("Usage: {prog} [OPTION]... <recipe>...");
    println!("Build packages from their recipes, dependencies first.");
    println!();
    println!("Options:");
    println!("  -a, --all             Build every recipe");
    println!("  -n, --dry-run         Only show the build plan");
    println!("  -f, --force           Rebuild the named recipes even if their archive exists");
    println!("  -u, --update-footprint");
    println!("                        Rewrite the footprints of the named recipes from their");
    println!("                        archives (accepts the changes shown by the last build)");
    println!("  -k, --keep-going      After a failure, go on with the recipes that do not need it");
    println!("  -q, --quiet           Only write the build output to the log files");
    println!("  -c, --chroot <rootfs> Build in an overlay of <rootfs>, with the makedepends");
    println!("                        installed by paquet (needs root)");
    println!("  -R, --recipes <dir>   Recipes directory (default: current directory)");
    println!("  -o, --output <dir>    Repository directory receiving the archives");
    println!("      --index           Regenerate the repository PQREPO afterwards (genpqrepo)");
    println!("      --keep-work       Keep the work directory of successful builds");
    println!("  -h, --help            Display this help message");
    println!();
    println!("Settings are read from {} (KEY=value lines):", config_file.display());
    println!("  RECIPES_DIR, REPO_DIR, WORK_DIR, SOURCES_DIR, LOG_DIR, MAKEFLAGS, CFLAGS,");
    println!("  CXXFLAGS, PKGMK_GROUPS, STYLES_DIR, USR_SRC_DIR, CHROOT");
}

/// Entry point of the `pqmake` command.
pub fn main() -> i32 {
    signals::install();
    util::set_umask(0o022);
    let prog = util::program_name("pqmake");
    let args: Vec<String> = std::env::args().skip(1).collect();
    crate::run_main(|| run(&prog, args))
}

fn run(prog: &str, args: Vec<String>) -> Result<()> {
    let arch = util::machine_arch();
    let config_file = std::env::var_os("PQMAKE_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/pqmake"));
    let mut s = Settings::defaults(&arch);
    if config_file.is_file() {
        s.read_config(&config_file)?;
    }
    if s.cxxflags.is_empty() {
        s.cxxflags = s.cflags.clone();
    }

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-a" | "--all" => s.build_all = true,
            "-n" | "--dry-run" => s.dry_run = true,
            "-f" | "--force" => s.force_targets = true,
            "-u" | "--update-footprint" => s.update_footprint = true,
            "-k" | "--keep-going" => s.keep_going = true,
            "-q" | "--quiet" => s.quiet = true,
            "--index" => s.update_index = true,
            "--keep-work" => s.keep_work = true,
            "-c" | "--chroot" | "-R" | "--recipes" | "-o" | "--output" => {
                let Some(dir) = iter.next().filter(|d| !d.is_empty()) else {
                    bail!("{arg} requires a directory.");
                };
                match arg.as_str() {
                    "-c" | "--chroot" => s.chroot = Some(dir.into()),
                    "-R" | "--recipes" => s.recipes_dir = dir.into(),
                    _ => s.repo_dir = dir.into(),
                }
            }
            "-h" | "--help" => {
                display_help(prog, &config_file);
                return Ok(());
            }
            a if a.starts_with('-') => {
                display_help(prog, &config_file);
                return Err(crate::Reported.into());
            }
            _ => s.targets.push(arg),
        }
    }

    let mut pm = Pqmake::new(s, arch);
    pm.index_recipes()?;
    if pm.s.build_all {
        if pm.s.force_targets {
            bail!("--force needs recipe names, not --all.");
        }
        pm.s.targets = pm.recipe_path.keys().cloned().collect();
    }
    if pm.s.targets.is_empty() {
        display_help(prog, &config_file);
        return Err(crate::Reported.into());
    }
    for name in pm.s.targets.clone() {
        if !pm.recipe_path.contains_key(&name) {
            return Err(die(format!("no recipe named {name} in {}.", pm.s.recipes_dir.display())));
        }
        if pm.s.force_targets {
            pm.force.insert(name);
        }
    }

    if pm.s.update_footprint {
        for name in pm.s.targets.clone() {
            pm.read_recipe(&name)?;
            pm.update_footprints(&name)?;
        }
        return Ok(());
    }

    let targets = pm.s.targets.clone();
    pm.collect_needed(&targets)?;
    pm.compute_before();
    pm.sort_recipes()?;
    pm.print_plan();
    if pm.s.dry_run {
        return Ok(());
    }

    pm.check_setup()?;
    pm.acquire_lock()?;
    // Ctrl-C stops the build in progress, then the mounts are released
    signals::defer(true);
    pm.build_all()?;
    if pm.s.update_index {
        let key = std::env::var_os("PQ_SIGN_KEY").filter(|v| !v.is_empty()).map(PathBuf::from);
        crate::genpqrepo::generate(&pm.s.repo_dir, &pm.s.repo_dir.join("PQREPO"), None, key.as_deref())?;
    }
    Ok(())
}
