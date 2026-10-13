//! Package scripts and hooks.
//!
//! Scripts come with a package (`.PRE-INSTALL`, `.POST-INSTALL`,
//! `.PRE-REMOVE` and `.POST-REMOVE` in its archive) and run around its own
//! installation, upgrade or removal. Hooks belong to the managed system
//! (`/etc/pq/hooks/*.hook`, usually installed by the package providing the
//! tool they call) and run once at the end of a command, when the files it
//! installed or removed match one of their paths.
//!
//! Both run with `/bin/sh` inside the managed system (chrooted with
//! `--chroot`), from `/`, with no input.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config;

/// Whether files were installed or removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Install,
    Remove,
}

/// A hook: a shell command run when installed or removed files match one of
/// its paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    /// File name without `.hook`.
    pub name: String,
    pub description: String,
    /// Patterns of paths relative to `/`, with `*` (any characters, `/`
    /// included) and `?` (one character).
    pub paths: Vec<String>,
    pub exec: String,
    pub on_install: bool,
    pub on_remove: bool,
}

impl Hook {
    /// Reads a hook file:
    ///
    /// ```text
    /// Description=Updating the shared library cache
    /// Path=usr/lib/*.so*
    /// Path=lib/*.so*
    /// On=install remove
    /// Exec=/sbin/ldconfig
    /// ```
    ///
    /// Returns None (with a warning) for an incomplete hook.
    pub fn parse(name: &str, content: &str, path: &Path) -> Option<Hook> {
        let mut hook = Hook {
            name: name.to_string(),
            description: String::new(),
            paths: Vec::new(),
            exec: String::new(),
            on_install: true,
            on_remove: true,
        };
        let mut on: Option<String> = None;
        for (key, value) in config::parse(content, path) {
            match key.as_str() {
                "Description" => hook.description = value,
                "Path" => hook.paths.push(value.trim_start_matches('/').to_string()),
                "Exec" => hook.exec = value,
                "On" => on = Some(value),
                _ => eprintln!("Warning: ignoring unknown setting {key} in {}", path.display()),
            }
        }
        if let Some(on) = on {
            let words: Vec<&str> = on.split_whitespace().collect();
            hook.on_install = words.contains(&"install");
            hook.on_remove = words.contains(&"remove");
            if words.iter().any(|w| !matches!(*w, "install" | "remove")) {
                eprintln!("Warning: {}: On= takes 'install' and/or 'remove'", path.display());
            }
        }
        if hook.paths.is_empty() || hook.exec.is_empty() {
            eprintln!("Warning: {} ignored: it needs at least one Path= and an Exec=", path.display());
            return None;
        }
        Some(hook)
    }

    /// True if the hook is triggered by these changes.
    pub fn triggered_by(&self, touched: &[(Vec<u8>, Op)]) -> bool {
        touched.iter().any(|(path, op)| {
            let wanted = match op {
                Op::Install => self.on_install,
                Op::Remove => self.on_remove,
            };
            let rel = path.strip_prefix(b"/").unwrap_or(path);
            wanted && self.paths.iter().any(|p| glob_match(p.as_bytes(), rel))
        })
    }
}

/// Hooks of a system (`<dir>/*.hook`), sorted by file name.
pub fn load_hooks(dir: &Path) -> Vec<Hook> {
    crate::util::files_with_suffix(dir, ".hook")
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().strip_suffix(".hook")?.to_string();
            let content = fs::read_to_string(&path).ok()?;
            Hook::parse(&name, &content, &path)
        })
        .collect()
}

/// Matches a path against a pattern: `*` stands for any characters, `/`
/// included, and `?` for one character.
pub fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((sp, st)) = star {
            p = sp + 1;
            t = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

/// A `/bin/sh` command run inside the managed system, or None when it has
/// no shell (a system being bootstrapped).
pub fn shell(target_root: &[u8], root_dir: &Path) -> Option<Command> {
    if !root_dir.join("bin/sh").exists() {
        return None;
    }
    let mut cmd = if target_root.is_empty() {
        let mut c = Command::new("/bin/sh");
        c.current_dir("/");
        c
    } else {
        let mut c = Command::new("chroot");
        c.arg(root_dir).arg("/bin/sh");
        c
    };
    cmd.stdin(Stdio::null());
    Some(cmd)
}

/// Runs a script with `sh -c <script> <name> <args>...`: the script sees
/// its arguments as `$1`, `$2`... Fails if it exits with an error.
pub fn run(mut cmd: Command, script: &[u8], name: &str, args: &[&str], env: &[(&str, &str)]) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    cmd.arg("-c").arg(std::ffi::OsStr::from_bytes(script)).arg(name).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let status = cmd.status().context("unable to run /bin/sh")?;
    if !status.success() {
        match status.code() {
            Some(code) => bail!("exit status {code}"),
            None => bail!("{status}"),
        }
    }
    Ok(())
}

/// Directory of the stored scripts of a package.
pub fn package_dir(scripts_dir: &Path, pkg: &str) -> PathBuf {
    scripts_dir.join(pkg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match(b"usr/lib/*.so*", b"usr/lib/libz.so.1"));
        assert!(glob_match(b"usr/lib/*.so*", b"usr/lib/gio/modules/libgio.so"));
        assert!(glob_match(b"usr/share/icons/*", b"usr/share/icons/hicolor/index.theme"));
        assert!(glob_match(b"etc/fo?", b"etc/foo"));
        assert!(glob_match(b"*", b"anything/at/all"));
        assert!(!glob_match(b"usr/lib/*.so*", b"usr/bin/ls"));
        assert!(!glob_match(b"usr/lib/*.so", b"usr/lib/libz.so.1"));
        assert!(!glob_match(b"etc/fo?", b"etc/fo"));
    }

    #[test]
    fn hooks() {
        let content =
            "Description=Library cache\nPath=/usr/lib/*.so*\nPath=lib/*.so*\nOn=install\nExec=/sbin/ldconfig\n";
        let hook = Hook::parse("ldconfig", content, Path::new("ldconfig.hook")).unwrap();
        assert_eq!(hook.paths, ["usr/lib/*.so*", "lib/*.so*"]);
        assert!(hook.on_install && !hook.on_remove);
        assert!(hook.triggered_by(&[(b"/usr/lib/libz.so.1".to_vec(), Op::Install)]));
        assert!(!hook.triggered_by(&[(b"/usr/lib/libz.so.1".to_vec(), Op::Remove)]));
        assert!(!hook.triggered_by(&[(b"/usr/bin/ls".to_vec(), Op::Install)]));
        assert!(Hook::parse("x", "Path=a\n", Path::new("x.hook")).is_none());
    }
}
