//! End-to-end tests of paquet and genpqrepo, on a `file://` mirror and a
//! `--chroot` root in a temporary directory.

mod common;

use std::fs;

use common::Env;

/// A signed mirror with an editor depending on a library.
fn signed_mirror() -> (Env, std::path::PathBuf) {
    let env = Env::new();
    env.package("base", "libedit", "1.0", &[("usr/lib/libedit.so.1", "lib v1"), ("etc/libedit.conf", "conf v1\n")], "");
    env.package("base", "editor", "2.0", &[("usr/bin/editor", "editor v2")], "libedit>=1.0");
    let key = env.genkey();
    env.index(Some(&key));
    (env, key)
}

#[test]
fn sync_checks_the_signature() {
    let env = Env::new();
    env.package("base", "foo", "1.0", &[("usr/bin/foo", "foo")], "");

    // Unsigned index: refused unless --nosign
    let r = env.index(None);
    assert!(r.stderr.contains("not signed"));
    let r = env.pq(&["--sync"]);
    assert!(!r.ok());
    assert!(r.all().contains("no signature found"), "{}", r.all());
    let r = env.pq(&["--sync", "--nosign"]);
    assert!(r.ok(), "{}", r.all());
    assert_eq!(env.status("foo"), "N");

    // Signed index, but no trusted key yet
    let key = env.path("other.key");
    let r = env.cmd("genpqrepo", &["--genkey", env.path("other").to_str().unwrap()], None);
    assert!(r.ok());
    env.index(Some(&key));
    let r = env.pq(&["-S"]);
    assert!(!r.ok());
    assert!(r.all().contains("no trusted public key"), "{}", r.all());

    // Trusted key: accepted
    fs::copy(env.path("other.pub"), env.path("keys/other.pub")).unwrap();
    let r = env.pq(&["-S"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Signature verified with other.pub"));

    // Tampered index: rejected, the local index is unchanged
    let index = env.repo_dir().join("PQREPO");
    let original = fs::read_to_string(&index).unwrap();
    fs::write(&index, original.replace("The foo package", "The evil package")).unwrap();
    let r = env.pq(&["-S"]);
    assert!(!r.ok());
    assert!(r.all().contains("invalid signature"), "{}", r.all());
    assert_eq!(fs::read_to_string(env.rootpath("var/lib/pq/PQREPO")).unwrap(), original);

    // --nosign is only for --sync
    let r = env.pq(&["-l", "--nosign"]);
    assert!(!r.ok());
}

#[test]
fn install_upgrade_remove() {
    let (env, key) = signed_mirror();
    assert!(env.pq(&["-S"]).ok());
    assert_eq!(env.status("editor"), "N");

    // Install with its dependency
    let r = env.pq(&["-i", "editor"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Installing editor and its dependencies: libedit editor"), "{}", r.stdout);
    assert!(r.stdout.contains("version constraint 'libedit>=1.0' of editor is not enforced yet"));
    assert_eq!(fs::read_to_string(env.rootpath("usr/bin/editor")).unwrap(), "editor v2");
    assert_eq!(env.status("editor"), "I");
    assert_eq!(env.status("libedit"), "A");
    let list = fs::read_to_string(env.rootpath("var/lib/pq/files/libedit")).unwrap();
    assert!(list.starts_with("# root: /\n"));
    assert!(list.contains("/usr/lib/libedit.so.1\n"));
    assert!(list.contains("/usr/lib\n"));
    assert!(!list.contains(".META"));
    assert!(!env.rootpath(".META").exists());
    assert!(!env.rootpath("var/lib/pq/transaction/editor").exists());

    // Already installed; an explicit install of a dependency only marks it
    assert!(!env.pq(&["-i", "editor"]).ok());
    let r = env.pq(&["-i", "libedit"]);
    assert!(r.ok() && r.stdout.contains("libedit is now marked as explicitly installed"), "{}", r.all());
    assert!(env.pq(&["--mark-auto", "libedit"]).ok());
    assert_eq!(env.status("libedit"), "A");

    // The library cannot go while the editor needs it
    let r = env.pq(&["-r", "libedit"]);
    assert!(!r.ok());
    assert!(r.all().contains("required by the following installed packages:\neditor"), "{}", r.all());

    // A new version of the library: it drops a file, and the local change
    // to its configuration is kept
    fs::write(env.rootpath("etc/libedit.conf"), "my settings\n").unwrap();
    fs::remove_file(env.repo_dir().join("base/libedit-1.0-1.tar.zst")).unwrap();
    env.package("base", "libedit", "1.1", &[("usr/lib/libedit.so.2", "lib v2"), ("etc/libedit.conf", "conf v2\n")], "");
    env.index(Some(&key));
    assert!(env.pq(&["-S"]).ok());
    assert_eq!(env.status("libedit"), "AU");
    assert_eq!(env.status("editor"), "I");

    let r = env.pq_env(&["-u"], Some("\n"), &[]);
    assert!(r.ok(), "{}", r.all());
    assert!(
        r.stdout.contains("Packages available for upgrade (in upgrade order):\n  libedit  1.0-1 -> 1.1-1\n"),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("Keeping existing"), "{}", r.stdout);
    assert_eq!(env.status("libedit"), "A");
    assert!(!env.rootpath("usr/lib/libedit.so.1").exists());
    assert_eq!(fs::read_to_string(env.rootpath("usr/lib/libedit.so.2")).unwrap(), "lib v2");
    assert_eq!(fs::read_to_string(env.rootpath("etc/libedit.conf")).unwrap(), "my settings\n");
    assert_eq!(fs::read_to_string(env.rootpath("etc/libedit.conf.pqnew")).unwrap(), "conf v2\n");
    assert!(env.pq_env(&["-u"], None, &[]).stdout.contains("No packages need upgrading."));

    // Removing the editor leaves the library orphaned
    let r = env.pq(&["-r", "editor"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("1 orphan package(s) left: libedit."), "{}", r.stdout);
    assert!(!env.rootpath("usr/bin/editor").exists());
    assert_eq!(env.status("editor"), "N");
    assert_eq!(env.pq(&["-o"]).stdout, "libedit\n");

    // Autoremove, cancelled then confirmed; protected files stay
    let r = env.pq_env(&["autoremove"], Some("n\n"), &[]);
    assert!(r.stdout.contains("Autoremove cancelled."));
    let r = env.pq_env(&["autoremove"], Some("y\n"), &[]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Keeping protected file:"));
    assert!(!env.rootpath("usr/lib/libedit.so.2").exists());
    assert!(!env.rootpath("usr/lib").exists(), "empty directories go");
    assert!(env.rootpath("etc/libedit.conf").exists());
    assert_eq!(env.status("libedit"), "N");
    assert!(env.pq(&["-o"]).stdout.contains("No orphan packages."));
}

#[test]
fn conflicts_are_refused() {
    let (env, key) = signed_mirror();
    env.package("extra", "clash", "1.0", &[("usr/bin/editor", "other editor")], "");
    env.index(Some(&key));
    assert!(env.pq(&["-S"]).ok());
    assert!(env.pq(&["-i", "editor"]).ok());
    let r = env.pq(&["-i", "clash"]);
    assert!(!r.ok());
    assert!(r.all().contains("the following files already exist"), "{}", r.all());
    assert_eq!(fs::read_to_string(env.rootpath("usr/bin/editor")).unwrap(), "editor v2");
    assert_eq!(env.status("clash"), "N");
    assert!(!env.rootpath("var/lib/pq/files/clash").exists());

    // Unknown package and unsafe names
    let r = env.pq(&["-i", "nothere"]);
    assert!(r.all().contains("package nothere not found in the repository."), "{}", r.all());
    let r = env.pq(&["-i", "../x"]);
    assert!(r.all().contains("invalid package name"), "{}", r.all());
}

#[test]
fn failed_operations_are_rolled_back() {
    let (env, key) = signed_mirror();
    assert!(env.pq(&["-S"]).ok());
    assert!(env.pq(&["-i", "libedit"]).ok());
    let state_before = fs::read_to_string(env.rootpath("var/lib/pq/PQREPOSTATE")).unwrap();

    // A failure after the extraction: nothing is left
    let r = env.pq_env(&["-i", "editor"], None, &[("PQ_TEST_CRASH", "fail:after-extract")]);
    assert!(!r.ok());
    assert!(r.stdout.contains("Rolling back the operation on editor"), "{}", r.all());
    assert!(!env.rootpath("usr/bin/editor").exists());
    assert!(!env.rootpath("var/lib/pq/files/editor").exists());
    assert!(!env.rootpath("var/lib/pq/transaction/editor").exists());
    assert_eq!(fs::read_to_string(env.rootpath("var/lib/pq/PQREPOSTATE")).unwrap(), state_before);

    // Ctrl-C: rolled back, exit status 130
    let r = env.pq_env(&["-i", "editor"], None, &[("PQ_TEST_CRASH", "int:after-extract")]);
    assert_eq!(r.status, 130, "{}", r.all());
    assert!(!env.rootpath("usr/bin/editor").exists());
    assert!(!env.rootpath("var/lib/pq/transaction/editor").exists());

    // A crash: the journal stays, and the next command rolls it back
    let r = env.pq_env(&["-i", "editor"], None, &[("PQ_TEST_CRASH", "kill:after-extract")]);
    assert_eq!(r.status, -1);
    assert!(env.rootpath("usr/bin/editor").exists());
    assert!(env.rootpath("var/lib/pq/transaction/editor").exists());
    let r = env.pq(&["-l"]);
    assert!(r.stderr.contains("an interrupted operation is pending"), "{}", r.all());
    let r = env.pq(&["--recover"]);
    assert!(r.ok() && r.stdout.contains("editor restored to its previous state."), "{}", r.all());
    assert!(!env.rootpath("usr/bin/editor").exists());
    assert_eq!(fs::read_to_string(env.rootpath("var/lib/pq/PQREPOSTATE")).unwrap(), state_before);
    assert!(env.pq(&["--recover"]).stdout.contains("Nothing to recover."));

    // A failed upgrade restores the previous version, files and state
    fs::remove_file(env.repo_dir().join("base/libedit-1.0-1.tar.zst")).unwrap();
    env.package("base", "libedit", "1.1", &[("usr/lib/libedit.so.2", "lib v2"), ("etc/libedit.conf", "conf v2\n")], "");
    env.index(Some(&key));
    assert!(env.pq(&["-S"]).ok());
    let state_before = fs::read_to_string(env.rootpath("var/lib/pq/PQREPOSTATE")).unwrap();
    let list_before = fs::read_to_string(env.rootpath("var/lib/pq/files/libedit")).unwrap();
    let r = env.pq_env(&["-u", "libedit"], None, &[("PQ_TEST_CRASH", "int:after-stale")]);
    assert_eq!(r.status, 130, "{}", r.all());
    assert_eq!(fs::read_to_string(env.rootpath("usr/lib/libedit.so.1")).unwrap(), "lib v1");
    assert!(!env.rootpath("usr/lib/libedit.so.2").exists());
    assert_eq!(fs::read_to_string(env.rootpath("etc/libedit.conf")).unwrap(), "conf v1\n");
    assert!(!env.rootpath("etc/libedit.conf.pqnew").exists());
    assert_eq!(fs::read_to_string(env.rootpath("var/lib/pq/PQREPOSTATE")).unwrap(), state_before);
    assert_eq!(fs::read_to_string(env.rootpath("var/lib/pq/files/libedit")).unwrap(), list_before);

    // And the upgrade then works
    let r = env.pq(&["-u", "libedit"]);
    assert!(r.ok(), "{}", r.all());
    assert!(env.rootpath("usr/lib/libedit.so.2").exists());
}

#[test]
fn checksum_mismatch_is_refused() {
    let (env, _key) = signed_mirror();
    assert!(env.pq(&["-S"]).ok());
    // The archive changes after the index was signed
    env.package("base", "editor", "2.0", &[("usr/bin/editor", "trojan")], "");
    let r = env.pq(&["-i", "editor"]);
    assert!(!r.ok());
    assert!(r.all().contains("checksum mismatch for editor-2.0-1.tar.zst"), "{}", r.all());
    assert!(!env.rootpath("usr/bin/editor").exists());
}

#[test]
fn list_search_and_cache() {
    let (env, _key) = signed_mirror();
    let r = env.pq(&["-l"]);
    assert!(!r.ok() && r.all().contains("--sync' first"), "{}", r.all());
    assert!(env.pq(&["-S"]).ok());
    let r = env.cmd_env("pq", &["-s", "EDIT", "--chroot", env.root().to_str().unwrap()], None, &[]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Name: editor\nVersion: 2.0-1\nDescription: The editor package\nFolder: base\n"));
    assert!(r.stdout.contains("Depends: libedit>=1.0\nStatus: N\n"));
    let r = env.pq(&["-l"]);
    assert!(r.stdout.contains("Name: libedit, Version: 1.0-1, Description: The libedit package, Status: N"));

    assert!(env.pq(&["-i", "editor"]).ok());
    let cache = env.rootpath("var/lib/pq/cache");
    fs::copy(cache.join("editor-2.0-1.tar.zst"), cache.join("editor-1.9-1.tar.zst")).unwrap();
    let r = env.pq(&["--cache-info"]);
    assert!(r.stdout.contains("Total: 3 archive(s)"), "{}", r.stdout);
    assert!(r.stdout.contains("Old versions: 1 archive(s)"), "{}", r.stdout);
    let r = env.pq(&["--purge-old-cache"]);
    assert!(r.stdout.contains("Removing editor-1.9-1.tar.zst"), "{}", r.stdout);
    assert!(cache.join("editor-2.0-1.tar.zst").exists());
    let r = env.pq_env(&["-C"], Some("y\n"), &[]);
    assert!(r.stdout.contains("Cache cleared successfully!"), "{}", r.stdout);
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);

    let r = env.pq(&["--bogus"]);
    assert_eq!(r.status, 1);
    assert!(r.stdout.contains("Usage:"));
}

#[test]
fn genpqrepo_index() {
    let env = Env::new();
    env.package("base", "foo", "1.9", &[("usr/bin/foo", "old")], "");
    env.package("base", "foo", "1.10", &[("usr/bin/foo", "new")], "a, b");
    env.package("extra", "bar", "1", &[("usr/bin/bar", "bar")], "");
    fs::write(env.repo_dir().join("extra/badname.tar.zst"), "x").unwrap();
    let r = env.index(None);
    assert!(r.stderr.contains("foo 1.9-1 ignored, foo 1.10-1 is newer"), "{}", r.stderr);
    assert!(r.stderr.contains("skipping"), "{}", r.stderr);
    let index = fs::read_to_string(env.repo_dir().join("PQREPO")).unwrap();
    let lines: Vec<&str> = index.lines().collect();
    assert_eq!(lines.len(), 2, "{index}");
    let foo: Vec<&str> = lines[0].split(';').collect();
    assert_eq!(&foo[2..], ["base", "foo", "1.10", "1", "The foo package", "", "base", "", "a b"]);
    assert_eq!(foo[0].len(), 64);
    assert!(lines[1].contains(";extra;bar;1;1;"));

    // Refreshing one folder keeps the others
    fs::remove_file(env.repo_dir().join("extra/bar-1-1.tar.zst")).unwrap();
    env.package("extra", "baz", "2", &[("usr/bin/baz", "baz")], "");
    let r = env.cmd("genpqrepo", &[env.repo_dir().to_str().unwrap(), "extra"], None);
    assert!(r.ok(), "{}", r.all());
    let index = fs::read_to_string(env.repo_dir().join("PQREPO")).unwrap();
    assert!(index.contains(";base;foo;1.10;"));
    assert!(index.contains(";extra;baz;2;"));
    assert!(!index.contains(";bar;"));

    let r = env.cmd("genpqrepo", &[], None);
    assert_eq!(r.status, 1);
    assert!(r.stdout.contains("Usage:"));
}

#[test]
fn parallel_downloads_over_http() {
    let (env, key) = signed_mirror();
    for n in 1..=4 {
        env.package("base", &format!("lib{n}"), "1.0", &[(&format!("usr/lib/lib{n}.so"), "library")], "");
    }
    env.package("base", "app", "1.0", &[("usr/bin/app", "app")], "lib1 lib2 lib3 lib4");
    env.index(Some(&key));
    let (url, served) = common::serve(env.path("mirror"));
    fs::write(env.path("pq.conf"), format!("MIRROR={url}\nBRANCH=stable\nPARALLEL_DOWNLOADS=3\n")).unwrap();
    let no_proxy = [("NO_PROXY", "127.0.0.1,localhost"), ("no_proxy", "127.0.0.1,localhost")];
    assert!(env.pq_env(&["-S"], None, &no_proxy).ok());

    let r = env.pq_env(&["-i", "app"], None, &no_proxy);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Downloading 5 packages ("), "{}", r.stdout);
    for n in 1..=4 {
        assert!(r.stdout.contains(&format!("\n  lib{n} 1.0-1  ")), "{}", r.stdout);
    }
    assert!(r.stdout.contains(" downloaded in "), "{}", r.stdout);
    // Everything is downloaded before anything is installed
    let downloaded = r.stdout.find(" downloaded in ").unwrap();
    assert!(r.stdout.find("Installing package").unwrap() > downloaded, "{}", r.stdout);
    assert_eq!(env.status("app"), "I");
    let max = served.max_active.load(std::sync::atomic::Ordering::SeqCst);
    assert!((2..=3).contains(&max), "{max} simultaneous downloads");
    let requests = served.requests.lock().unwrap().clone();
    assert_eq!(requests.iter().filter(|r| r.starts_with("HEAD ")).count(), 5, "{requests:?}");

    // An upgrade shows the download size before asking
    env.package("base", "lib1", "1.1", &[("usr/lib/lib1.so", "library v2")], "");
    env.package("base", "lib2", "1.1", &[("usr/lib/lib2.so", "library v2")], "");
    env.index(Some(&key));
    assert!(env.pq_env(&["-S"], None, &no_proxy).ok());
    let size: u64 = ["lib1-1.1-1", "lib2-1.1-1"]
        .iter()
        .map(|a| fs::metadata(env.repo_dir().join(format!("base/{a}.tar.zst"))).unwrap().len())
        .sum();
    let r = env.pq_env(&["-u"], Some("n\n"), &no_proxy);
    assert!(r.stdout.contains("  lib1  1.0-1 -> 1.1-1\n  lib2  1.0-1 -> 1.1-1\n"), "{}", r.stdout);
    let expected = format!("Download size: {}\n", paquet::paquet::download::size_label(size));
    assert!(r.stdout.contains(&expected), "{}", r.stdout);
    assert!(r.stdout.contains("Upgrade cancelled."), "{}", r.stdout);
    assert!(!r.stdout.contains("Downloading"), "nothing downloaded: {}", r.stdout);

    // A missing archive stops the upgrade before anything changes; the
    // other archive stays in the cache
    fs::remove_file(env.repo_dir().join("base/lib2-1.1-1.tar.zst")).unwrap();
    let r = env.pq_env(&["-u", "-y"], None, &no_proxy);
    assert!(!r.ok(), "{}", r.all());
    assert!(r.stdout.contains("+ 1 of unknown size"), "{}", r.stdout);
    assert!(r.stdout.contains("  lib2 1.1-1      failed"), "{}", r.stdout);
    assert!(r.stderr.contains("404"), "{}", r.stderr);
    assert!(r.stderr.contains("1 download of 2 failed; nothing was changed."), "{}", r.stderr);
    assert_eq!(fs::read_to_string(env.rootpath("usr/lib/lib1.so")).unwrap(), "library");
    assert!(env.rootpath("var/lib/pq/cache/lib1-1.1-1.tar.zst").is_file());

    // Once the mirror is fixed, only the missing archive is downloaded
    env.package("base", "lib2", "1.1", &[("usr/lib/lib2.so", "library v2")], "");
    env.index(Some(&key));
    assert!(env.pq_env(&["-S"], None, &no_proxy).ok());
    let r = env.pq_env(&["-u", "--yes"], None, &no_proxy);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("(1 archive already in the cache)"), "{}", r.stdout);
    assert!(r.stdout.contains("Downloading 1 package ("), "{}", r.stdout);
    assert_eq!(fs::read_to_string(env.rootpath("usr/lib/lib2.so")).unwrap(), "library v2");

    let r = env.pq_env(&["-l", "-y"], None, &no_proxy);
    assert!(!r.ok() && r.stderr.contains("-y only applies to"), "{}", r.all());
}

#[test]
fn configured_groups_come_with_their_packages() {
    let env = Env::new();
    env.package("base", "lib", "1.0", &[("usr/lib/lib.so", "lib")], "");
    env.package("base", "lib.fr", "1.0", &[("usr/share/locale/fr/lib.mo", "fr")], "lib");
    env.package("base", "app", "1.0", &[("usr/bin/app", "app")], "lib");
    env.package("base", "app.fr", "1.0", &[("usr/share/locale/fr/app.mo", "fr")], "app");
    env.package("base", "app.de", "1.0", &[("usr/share/locale/de/app.mo", "de")], "app");
    env.package("base", "app.man", "1.0", &[("usr/share/man/man1/app.1", "man")], "app");
    env.index(None);
    let conf = |extra: &str| {
        let mirror = env.path("mirror").display().to_string();
        fs::write(env.path("pq.conf"), format!("MIRROR=file://{mirror}\nBRANCH=stable\n{extra}")).unwrap();
    };
    assert!(env.pq(&["-S", "--nosign"]).ok());

    // Without GROUP, English only: no subpackage
    let r = env.pq(&["-i", "app"]);
    assert!(r.ok(), "{}", r.all());
    assert_eq!((env.status("app"), env.status("lib"), env.status("app.fr")), ("I".into(), "A".into(), "N".into()));

    // GROUP lines: -u adds what installed packages lack
    conf("GROUP=\"fr\"\nGROUP=\"man\"\n");
    let r = env.pq(&["-u", "-y"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Packages to add for GROUP fr man: app.fr app.man lib.fr\n"), "{}", r.stdout);
    for p in ["app.fr", "app.man", "lib.fr"] {
        assert_eq!(env.status(p), "A", "{p}");
    }
    assert_eq!(env.status("app.de"), "N");
    assert!(env.rootpath("usr/share/locale/fr/app.mo").is_file());
    assert_eq!(env.pq(&["-o"]).stdout, "No orphan packages.\n");

    // They go with their package; lib.fr stays while lib does
    let r = env.pq(&["-r", "app"]);
    assert!(r.ok(), "{}", r.all());
    for p in ["app", "app.fr", "app.man"] {
        assert_eq!(env.status(p), "N", "{p}");
    }
    assert!(r.stdout.contains("2 orphan package(s) left: lib lib.fr."), "{}", r.stdout);
    let r = env.pq(&["autoremove", "-y"]);
    assert!(r.ok(), "{}", r.all());
    assert_eq!((env.status("lib"), env.status("lib.fr")), ("N".into(), "N".into()));

    // -i brings them at once, the dependencies' too
    let r = env.pq(&["-i", "app"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Installing app and its dependencies: lib app lib.fr app.fr app.man\n"), "{}", r.stdout);

    // A group no longer configured leaves orphans
    conf("GROUP=fr\nGROUP=Français\n");
    let r = env.pq(&["-o"]);
    assert_eq!(r.stdout, "app.man\n", "{}", r.all());
    assert!(r.stderr.contains("GROUP: 'Français' is neither a language"), "{}", r.stderr);
}
