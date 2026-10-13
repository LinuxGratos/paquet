//! Package scripts, hooks and the query commands (--query, --info, --files).
//! The tests that run scripts in a chroot need root and are skipped
//! otherwise.

mod common;

use std::fs;

use common::Env;

const LOG: &str = "scripts.log";

/// A script appending its name, arguments and package to /scripts.log.
fn logging(name: &str) -> String {
    format!("echo \"{name} $* $PQ_PACKAGE\" >> /{LOG}\n")
}

fn log(env: &Env) -> String {
    fs::read_to_string(env.rootpath(LOG)).unwrap_or_default()
}

#[test]
fn query_info_and_files() {
    let env = Env::new();
    env.package("base", "libedit", "1.0", &[("usr/lib/libedit.so.1", "lib")], "");
    env.package("base", "editor", "2.0", &[("usr/bin/editor", "editor"), ("usr/share/editor/x", "x")], "libedit");
    env.index(None);
    assert!(env.pq(&["-S", "--nosign"]).ok());

    // Not installed: what the repository knows
    let r = env.pq(&["--info", "editor"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Name           : editor\nVersion        : 2.0-1\n"), "{}", r.stdout);
    assert!(r.stdout.contains("Status         : N (not installed)"), "{}", r.stdout);
    assert!(!r.stdout.contains("Files"));
    assert!(!env.pq(&["-F", "editor"]).ok());
    assert!(!env.pq(&["-I", "nothere"]).ok());

    assert!(env.pq(&["-i", "editor"]).ok());
    let r = env.pq(&["-I", "editor"]);
    assert!(r.stdout.contains("Status         : I (installed explicitly)"), "{}", r.stdout);
    assert!(r.stdout.contains("Depends        : libedit\n"), "{}", r.stdout);
    assert!(r.stdout.contains("Files          : 2\n"), "{}", r.stdout);
    assert!(r.stdout.contains("Scripts        : -\n"), "{}", r.stdout);
    let r = env.pq(&["-I", "libedit"]);
    assert!(r.stdout.contains("Status         : A (installed as a dependency)"), "{}", r.stdout);
    assert!(r.stdout.contains("Required by    : editor\n"), "{}", r.stdout);

    let r = env.pq(&["--files", "editor"]);
    let root = env.root().display().to_string();
    assert_eq!(
        r.stdout,
        // usr/bin and usr/share did not exist: editor created them
        format!(
            "{root}/usr/bin\n{root}/usr/bin/editor\n{root}/usr/share\n{root}/usr/share/editor\n{root}/usr/share/editor/x\n"
        ),
        "{}",
        r.all()
    );

    // Owner of a file, given relative to the managed root
    let r = env.pq(&["-Q", "/usr/bin/editor"]);
    assert_eq!(r.stdout, format!("{root}/usr/bin/editor is owned by editor 2.0-1\n"), "{}", r.all());
    let r = env.pq(&["--query", "/usr/share/../bin/./editor"]);
    assert!(r.ok() && r.stdout.contains("owned by editor"), "{}", r.all());
    // Through a symlink of the managed system
    std::os::unix::fs::symlink("usr/lib", env.rootpath("lib")).unwrap();
    let r = env.pq(&["-Q", "/lib/libedit.so.1"]);
    assert!(r.stdout.contains("usr/lib/libedit.so.1 is owned by libedit 1.0-1"), "{}", r.all());
    // A directory may belong to several packages; a file of none fails
    let r = env.pq(&["-Q", "/usr/bin/other"]);
    assert!(!r.ok() && r.all().contains("no package owns /usr/bin/other"), "{}", r.all());
}

#[test]
fn scripts_skipped_without_shell_or_with_noscripts() {
    let env = Env::new();
    let failing = "exit 1\n";
    env.package(
        "base",
        "tool",
        "1.0",
        &[("usr/bin/tool", "tool"), (".PRE-INSTALL", failing), (".POST-REMOVE", failing)],
        "",
    );
    env.index(None);
    assert!(env.pq(&["-S", "--nosign"]).ok());

    // A system being bootstrapped has no shell yet: scripts are skipped
    let r = env.pq(&["-i", "tool"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("no /bin/sh in"), "{}", r.stdout);
    assert!(!env.rootpath(".PRE-INSTALL").exists(), "scripts are not extracted");
    assert!(!fs::read_to_string(env.rootpath("var/lib/pq/files/tool")).unwrap().contains("PRE-INSTALL"));
    // They are stored with the package
    assert!(env.rootpath("var/lib/pq/scripts/tool/pre-install").is_file());
    assert!(env.pq(&["-I", "tool"]).stdout.contains("Scripts        : pre-install post-remove\n"));

    let r = env.pq(&["-r", "tool", "--noscripts"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Skipping the post-remove script of tool (--noscripts)"), "{}", r.stdout);
    assert!(!env.rootpath("var/lib/pq/scripts/tool").exists());

    let r = env.pq(&["-l", "--noscripts"]);
    assert!(!r.ok() && r.all().contains("--noscripts only applies to"), "{}", r.all());
}

#[test]
fn scripts_run_in_the_managed_system() {
    let env = Env::new();
    if !env.shell_root() {
        return;
    }
    let scripts = |prefix: &str| {
        [
            (".PRE-INSTALL", logging(&format!("{prefix}pre-install"))),
            (".POST-INSTALL", logging(&format!("{prefix}post-install"))),
            (".PRE-REMOVE", logging(&format!("{prefix}pre-remove"))),
            (".POST-REMOVE", logging(&format!("{prefix}post-remove"))),
        ]
    };
    let v1 = scripts("");
    let mut files: Vec<(&str, &str)> = vec![("usr/bin/tool", "v1")];
    files.extend(v1.iter().map(|(e, c)| (*e, c.as_str())));
    env.package("base", "tool", "1.0", &files, "");
    env.index(None);
    assert!(env.pq(&["-S", "--nosign"]).ok());

    let r = env.pq(&["-i", "tool"]);
    assert!(r.ok(), "{}", r.all());
    assert_eq!(log(&env), "pre-install install 1.0-1 tool\npost-install install 1.0-1 tool\n");

    // An upgrade runs the scripts of the new version
    fs::remove_file(env.repo_dir().join("base/tool-1.0-1.tar.zst")).unwrap();
    let v2 = scripts("v2-");
    let mut files: Vec<(&str, &str)> = vec![("usr/bin/tool", "v2")];
    files.extend(v2.iter().map(|(e, c)| (*e, c.as_str())));
    env.package("base", "tool", "2.0", &files, "");
    env.index(None);
    assert!(env.pq(&["-S", "--nosign"]).ok());
    fs::remove_file(env.rootpath(LOG)).unwrap();
    let r = env.pq(&["-u", "tool"]);
    assert!(r.ok(), "{}", r.all());
    assert_eq!(log(&env), "v2-pre-install upgrade 2.0-1 1.0-1 tool\nv2-post-install upgrade 2.0-1 1.0-1 tool\n");

    // And a removal its stored remove scripts
    fs::remove_file(env.rootpath(LOG)).unwrap();
    let r = env.pq(&["-r", "tool"]);
    assert!(r.ok(), "{}", r.all());
    assert_eq!(log(&env), "v2-pre-remove remove 2.0-1 tool\nv2-post-remove remove 2.0-1 tool\n");
    assert!(!env.rootpath("var/lib/pq/scripts/tool").exists());

    // --noscripts
    fs::remove_file(env.rootpath(LOG)).unwrap();
    assert!(env.pq(&["-i", "tool", "--noscripts"]).ok());
    assert_eq!(log(&env), "");
}

#[test]
fn failing_scripts() {
    let env = Env::new();
    if !env.shell_root() {
        return;
    }
    env.package("base", "nopre", "1", &[("usr/bin/nopre", "x"), (".PRE-INSTALL", "echo refused; exit 3\n")], "");
    env.package("base", "nopost", "1", &[("usr/bin/nopost", "x"), (".POST-INSTALL", "exit 4\n")], "");
    env.package("base", "stuck", "1", &[("usr/bin/stuck", "x"), (".PRE-REMOVE", "exit 5\n")], "");
    env.index(None);
    assert!(env.pq(&["-S", "--nosign"]).ok());

    // A failing pre-install script cancels the installation
    let r = env.pq(&["-i", "nopre"]);
    assert!(!r.ok());
    assert!(r.stdout.contains("refused"), "{}", r.all());
    assert!(r.stderr.contains("installation of nopre cancelled"), "{}", r.all());
    assert!(r.stderr.contains("exit status 3"), "{}", r.all());
    assert!(!env.rootpath("usr/bin/nopre").exists());
    assert!(!env.rootpath("var/lib/pq/scripts/nopre").exists());
    assert_eq!(env.status("nopre"), "N");

    // A failing post-install script: the package stays, the command fails
    let r = env.pq(&["-i", "nopost"]);
    assert_eq!(r.status, 1, "{}", r.all());
    assert!(r.stderr.contains("the post-install script of nopost failed"), "{}", r.all());
    assert_eq!(env.status("nopost"), "I");

    // A failing pre-remove script cancels the removal
    assert!(env.pq(&["-i", "stuck"]).ok());
    let r = env.pq(&["-r", "stuck"]);
    assert!(!r.ok());
    assert!(r.stderr.contains("removal of stuck cancelled"), "{}", r.all());
    assert!(env.rootpath("usr/bin/stuck").exists());
    assert_eq!(env.status("stuck"), "I");
    assert!(env.pq(&["-r", "stuck", "--noscripts"]).ok());
    assert!(!env.rootpath("usr/bin/stuck").exists());
}

#[test]
fn hooks_run_once_per_command() {
    let env = Env::new();
    if !env.shell_root() {
        return;
    }
    fs::create_dir_all(env.rootpath("etc/pq/hooks")).unwrap();
    fs::write(
        env.rootpath("etc/pq/hooks/libs.hook"),
        format!("Description=Updating the library cache\nPath=usr/lib/*.so*\nExec=echo libs >> /{LOG}\n"),
    )
    .unwrap();
    fs::write(
        env.rootpath("etc/pq/hooks/gone.hook"),
        format!("Path=usr/share/doc/*\nOn=remove\nExec=echo gone >> /{LOG}\n"),
    )
    .unwrap();
    env.package("base", "liba", "1", &[("usr/lib/liba.so.1", "a"), ("usr/share/doc/liba/README", "a")], "");
    env.package("base", "libb", "1", &[("usr/lib/libb.so.1", "b")], "liba");
    env.package("base", "app", "1", &[("usr/bin/app", "app")], "");
    env.index(None);
    assert!(env.pq(&["-S", "--nosign"]).ok());

    // Two libraries installed by one command: the hook runs once
    let r = env.pq(&["-i", "libb"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Running hook: Updating the library cache"), "{}", r.stdout);
    assert_eq!(log(&env), "libs\n");

    // Nothing matching: no hook
    fs::remove_file(env.rootpath(LOG)).unwrap();
    assert!(env.pq(&["-i", "app"]).ok());
    assert_eq!(log(&env), "");

    // A removal triggers both hooks (sorted by file name)
    assert!(env.pq(&["-r", "libb"]).ok());
    assert!(env.pq(&["-r", "liba"]).ok());
    assert_eq!(log(&env), "libs\ngone\nlibs\n");

    // --noscripts also skips the hooks
    fs::remove_file(env.rootpath(LOG)).unwrap();
    assert!(env.pq(&["-i", "libb", "--noscripts"]).ok());
    assert_eq!(log(&env), "");
}

#[test]
fn example_hooks_are_valid() {
    use paquet::paquet::scripts::{Op, load_hooks};
    let hooks = load_hooks(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("hooks"));
    let names: Vec<&str> = hooks.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, ["desktop-database", "glib-schemas", "gtk-icon-cache", "ldconfig"]);
    let triggered = |path: &str| -> Vec<&str> {
        hooks
            .iter()
            .filter(|h| h.triggered_by(&[(path.as_bytes().to_vec(), Op::Install)]))
            .map(|h| h.name.as_str())
            .collect()
    };
    assert_eq!(triggered("/usr/lib/libz.so.1"), ["ldconfig"]);
    assert_eq!(triggered("/usr/share/icons/hicolor/48x48/apps/x.png"), ["gtk-icon-cache"]);
    assert_eq!(triggered("/usr/share/glib-2.0/schemas/org.x.gschema.xml"), ["glib-schemas"]);
    assert_eq!(triggered("/usr/share/applications/x.desktop"), ["desktop-database"]);
    assert!(triggered("/usr/bin/x").is_empty());
}
