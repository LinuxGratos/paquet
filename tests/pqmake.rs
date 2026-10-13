//! End-to-end tests of pqmake (host builds), then of the built repository
//! with paquet.

mod common;

use std::fs;
use std::path::Path;

use common::{Env, Run};
use paquet::pqmake;

fn recipe(env: &Env, set: &str, name: &str, body: &str) {
    let dir = env.path(&format!("recipes/{set}/{name}"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("Pkgfile"), body).unwrap();
}

fn pqmake(env: &Env, args: &[&str]) -> Run {
    let config = env.path("pqmake.conf");
    if !config.exists() {
        fs::write(
            &config,
            format!(
                "RECIPES_DIR={}\nREPO_DIR={}\nWORK_DIR={}\nSOURCES_DIR={}\nLOG_DIR={}\nMAKEFLAGS=-j1\n",
                env.path("recipes").display(),
                env.repo_dir().display(),
                env.path("work").display(),
                env.path("sources").display(),
                env.path("logs").display()
            ),
        )
        .unwrap();
    }
    let config = config.display().to_string();
    let home = env.tmp.path().display().to_string();
    env.cmd_env("pqmake", args, None, &[("PQMAKE_CONFIG", &config), ("PQMAKE_ALLOW_ROOT", "1"), ("HOME", &home)])
}

/// A program linked with the C library, to detect runtime dependencies.
fn some_elf_program() -> Option<&'static str> {
    ["/bin/true", "/usr/bin/true"].into_iter().find(|p| Path::new(p).is_file())
}

fn standard_recipes(env: &Env) {
    // Provides the C library the programs need, plus a header (devel group)
    recipe(
        env,
        "base",
        "libfake",
        r#"description="Fake C library"
name=libfake
version=1.0-rc1
release=2
build() {
    mkdir -p "$PKG/usr/lib" "$PKG/usr/include"
    echo lib > "$PKG/usr/lib/libc.so.6"
    echo '#define FAKE 1' > "$PKG/usr/include/fake.h"
}
"#,
    );
    // Built after libfake; its program needs libc.so.6
    let elf = some_elf_program().unwrap_or("/bin/sh");
    recipe(
        env,
        "extra",
        "hello",
        &format!(
            r#"description="Says hello"
name=hello
version=2.1
makedepends=(libfake)
run=(extradep)
source=(hello.txt data.tar.gz)
build() {{
    test -f hello.txt
    test -f data/inside.txt
    test "$PKGMK_ARCH" = "$(uname -m)"
    mkdir -p "$PKG/usr/bin" "$PKG/usr/share/man/man1" "$PKG/usr/share/hello"
    cp {elf} "$PKG/usr/bin/hello"
    cp hello.txt data/inside.txt "$PKG/usr/share/hello/"
    echo man > "$PKG/usr/share/man/man1/hello.1"
}}
"#
        ),
    );
    let dir = env.path("recipes/extra/hello");
    fs::write(dir.join("hello.txt"), "hello source\n").unwrap();
    fs::create_dir_all(env.path("data")).unwrap();
    fs::write(env.path("data/inside.txt"), "inside\n").unwrap();
    let ok = std::process::Command::new("tar")
        .arg("-czf")
        .arg(dir.join("data.tar.gz"))
        .arg("-C")
        .arg(env.tmp.path())
        .arg("data")
        .status()
        .unwrap()
        .success();
    assert!(ok);
    recipe(
        env,
        "extra",
        "extradep",
        r#"name=extradep
version=1
description="Runtime dependency"
PKGMK_IGNORE_RUNTIMEDEPS=yes
build() { mkdir -p "$PKG/usr/share/extradep"; echo x > "$PKG/usr/share/extradep/x"; }
"#,
    );
}

#[test]
fn builds_in_dependency_order() {
    let env = Env::new();
    standard_recipes(&env);

    let r = pqmake(&env, &["-n", "hello"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Build plan: 3 recipe(s), 3 to build"), "{}", r.stdout);
    let plan: Vec<&str> = r.stdout.lines().skip(1).collect();
    assert_eq!(plan[0], "  build  libfake 1.0_rc1-2");
    assert!(plan[1..3].contains(&"  build  hello 2.1-1"), "{plan:?}");

    let r = pqmake(&env, &["-q", "hello"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("==> 3 recipe(s) built"), "{}", r.stdout);
    let repo = env.repo_dir();
    for archive in [
        "base/libfake-1.0_rc1-2.tar.zst",
        "base/libfake-1.0_rc1-2-devel.tar.zst",
        "extra/hello-2.1-1.tar.zst",
        "extra/extradep-1-1.tar.zst",
    ] {
        assert!(repo.join(archive).is_file(), "{archive} missing:\n{}", r.all());
    }
    assert!(!env.path("work/hello").exists(), "work directories are removed");
    assert!(env.path("logs/hello-2.1-1.log").is_file());

    // Metadata: groups, descriptions and dependencies
    let meta = |a: &str| paquet::archive::read_meta(&repo.join(a)).unwrap().unwrap();
    let hello = meta("extra/hello-2.1-1.tar.zst");
    assert!(hello.contains("Description: Says hello\nGroups: \nSet: extra\n"), "{hello}");
    if some_elf_program().is_some() {
        assert!(hello.contains("Depends: extradep libfake\n"), "{hello}");
    }
    let devel = meta("base/libfake-1.0_rc1-2-devel.tar.zst");
    assert!(
        devel.contains("Description: Fake C library (devel)\nGroups: devel\nSet: base\nDepends: libfake\n"),
        "{devel}"
    );
    // English manual pages stay in the main package
    assert!(!repo.join("extra/hello-2.1-1-man.tar.zst").exists());
    let names: Vec<String> = paquet::archive::list(&repo.join("extra/hello-2.1-1.tar.zst"))
        .unwrap()
        .iter()
        .map(|e| String::from_utf8_lossy(&e.name).into_owned())
        .collect();
    assert!(names.contains(&"usr/share/man/man1/hello.1".to_string()), "{names:?}");
    assert!(names.contains(&"usr/share/hello/inside.txt".to_string()), "{names:?}");

    // Up to date, unless forced
    let r = pqmake(&env, &["-n", "hello"]);
    assert!(r.stdout.contains("3 recipe(s), 0 to build"), "{}", r.stdout);
    let r = pqmake(&env, &["-n", "-f", "hello"]);
    assert!(r.stdout.contains("  build  hello 2.1-1 (forced)"), "{}", r.stdout);

    // The repository works with paquet
    let r = pqmake(&env, &["--index", "hello"]);
    assert!(r.ok(), "{}", r.all());
    assert!(env.repo_dir().join("PQREPO").is_file());
    assert!(env.pq(&["-S", "--nosign"]).ok());
    let r = env.pq(&["-i", "hello"]);
    assert!(r.ok(), "{}", r.all());
    assert_eq!(env.status("hello"), "I");
    assert_eq!(env.status("extradep"), "A");
    assert_eq!(fs::read_to_string(env.rootpath("usr/share/hello/hello.txt")).unwrap(), "hello source\n");
}

#[test]
fn reports_cycles_and_failures() {
    let env = Env::new();
    recipe(&env, "base", "a", "name=a\nversion=1\nmakedepends=(b)\nbuild() { :; }\n");
    recipe(&env, "base", "b", "name=b\nversion=1\nmakedepends=(c.devel)\nbuild() { :; }\n");
    recipe(&env, "base", "c", "name=c\nversion=1\nmakedepends=(a)\nbuild() { :; }\n");
    let r = pqmake(&env, &["-n", "a"]);
    assert!(!r.ok());
    assert!(r.stderr.contains("dependency cycle: a -> b -> c -> a"), "{}", r.stderr);

    let env = Env::new();
    recipe(&env, "base", "broken", "name=broken\nversion=1\nbuild() { echo failing step; false; }\n");
    recipe(&env, "base", "needsbroken", "name=needsbroken\nversion=1\nmakedepends=(broken)\nbuild() { :; }\n");
    recipe(&env, "base", "empty", "name=empty\nversion=1\nbuild() { :; }\n");
    recipe(&env, "base", "fine", "name=fine\nversion=1\nrun=(gcc)\nbuild() { mkdir -p $PKG/x; touch $PKG/x/y; }\n");
    recipe(&env, "base", "misnamed", "name=other\nversion=1\n");
    let r = pqmake(&env, &["-k", "needsbroken", "empty", "fine"]);
    assert!(!r.ok());
    let all = r.all();
    assert!(all.contains("failing step"), "{all}");
    assert!(all.contains("Error: build of broken failed"), "{all}");
    assert!(all.contains("==> Skipping needsbroken: broken was not built"), "{all}");
    assert!(all.contains("installed nothing in $PKG"), "{all}");
    assert!(all.contains("Without recipe, assumed provided by the build system: gcc"), "{all}");
    assert!(all.contains("failed: broken empty; not built: broken empty needsbroken"), "{all}");
    assert!(env.repo_dir().join("base/fine-1-1.tar.zst").is_file());
    assert!(env.path("work/broken").is_dir(), "failed work directories are kept");

    let r = pqmake(&env, &["misnamed"]);
    assert!(r.all().contains("name=other does not match its directory (misnamed)"), "{}", r.all());
    let r = pqmake(&env, &["nothere"]);
    assert!(r.all().contains("no recipe named nothere"), "{}", r.all());
}

#[test]
fn scripts_and_build_styles() {
    let env = Env::new();
    // A shared style, versioned with the recipes
    fs::create_dir_all(env.path("recipes/styles")).unwrap();
    fs::write(
        env.path("recipes/styles/copy"),
        "mkdir -p \"$PKG/usr/share/$name\"\necho \"built by the copy style\" > \"$PKG/usr/share/$name/how\"\n",
    )
    .unwrap();
    // A default style replaces the built-in autotools build
    fs::write(
        env.path("recipes/styles/default"),
        "mkdir -p \"$PKG/usr/share/$name\"\ntouch \"$PKG/usr/share/$name/default\"\n",
    )
    .unwrap();
    // Another one in STYLES_DIR
    fs::create_dir_all(env.path("shared-styles")).unwrap();
    fs::write(env.path("shared-styles/shared"), "mkdir -p \"$PKG/shared\"\ntouch \"$PKG/shared/ok\"\n").unwrap();
    fs::write(
        env.path("pqmake.conf"),
        format!(
            "RECIPES_DIR={}\nREPO_DIR={}\nWORK_DIR={}\nSOURCES_DIR={}\nLOG_DIR={}\nSTYLES_DIR={}\n",
            env.path("recipes").display(),
            env.repo_dir().display(),
            env.path("work").display(),
            env.path("sources").display(),
            env.path("logs").display(),
            env.path("shared-styles").display()
        ),
    )
    .unwrap();
    recipe(&env, "base", "sharedstyle", "name=sharedstyle\nversion=1\nbuild=shared\n");
    recipe(&env, "base", "styled", "name=styled\nversion=1\nbuild=copy\n");
    recipe(&env, "base", "plain", "name=plain\nversion=1\n");
    recipe(&env, "base", "own", "name=own\nversion=1\nbuild=copy\nbuild() { mkdir -p $PKG/own; touch $PKG/own/f; }\n");
    for (script, content) in [("post-install", "echo installed\n"), ("pre-remove", "echo removing\n")] {
        fs::write(env.path(&format!("recipes/base/styled/{script}")), content).unwrap();
    }

    let r = pqmake(&env, &["-q", "styled", "plain", "own", "sharedstyle"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("with its post-install script"), "{}", r.stdout);
    let repo = env.repo_dir();
    let names = |archive: &str| -> Vec<String> {
        paquet::archive::list(&repo.join(archive))
            .unwrap()
            .iter()
            .map(|e| String::from_utf8_lossy(&e.name).into_owned())
            .collect()
    };
    let styled = names("base/styled-1-1.tar.zst");
    assert!(styled.contains(&"usr/share/styled/how".to_string()), "{styled:?}");
    assert!(styled.contains(&".POST-INSTALL".to_string()) && styled.contains(&".PRE-REMOVE".to_string()));
    assert!(!styled.contains(&".PRE-INSTALL".to_string()));
    let scripts = paquet::archive::read_metadata(&repo.join("base/styled-1-1.tar.zst"), &[".POST-INSTALL"]).unwrap();
    assert_eq!(scripts[".POST-INSTALL"], b"echo installed\n");
    assert!(names("base/plain-1-1.tar.zst").contains(&"usr/share/plain/default".to_string()));
    assert!(names("base/sharedstyle-1-1.tar.zst").contains(&"shared/ok".to_string()));
    // A build() function wins over build=
    assert!(names("base/own-1-1.tar.zst").contains(&"own/f".to_string()));

    // An unknown style is reported when the plan is made
    recipe(&env, "base", "lost", "name=lost\nversion=1\nbuild=nosuchstyle\n");
    let r = pqmake(&env, &["-n", "lost"]);
    assert!(!r.ok());
    assert!(r.all().contains("build style 'nosuchstyle' not found"), "{}", r.all());
}

#[test]
fn footprints_show_what_changed() {
    let env = Env::new();
    let tool = |release: &str, body: &str| {
        recipe(&env, "base", "tool", &format!("name=tool\nversion=1\nrelease={release}\nbuild() {{\n{body}\n}}\n"));
    };
    tool(
        "1",
        r#"mkdir -p "$PKG/usr/bin" "$PKG/usr/share/tool" "$PKG/usr/share/locale/fr"
    printf '#!/bin/sh\n' > "$PKG/usr/bin/tool"; chmod 755 "$PKG/usr/bin/tool"
    echo a > "$PKG/usr/share/tool/a"
    ln -s a "$PKG/usr/share/tool/link"
    echo fr > "$PKG/usr/share/locale/fr/tool.mo""#,
    );
    let r = pqmake(&env, &["-q", "tool"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("  new footprint: base/tool/.footprint\n"), "{}", r.stdout);
    assert!(r.stdout.contains("  new footprint: base/tool/.footprint.fr\n"), "{}", r.stdout);
    let footprint = fs::read_to_string(env.path("recipes/base/tool/.footprint")).unwrap();
    assert_eq!(
        footprint,
        "drwxr-xr-x\troot/root\tusr/\n\
         drwxr-xr-x\troot/root\tusr/bin/\n\
         -rwxr-xr-x\troot/root\tusr/bin/tool\n\
         drwxr-xr-x\troot/root\tusr/share/\n\
         drwxr-xr-x\troot/root\tusr/share/tool/\n\
         -rw-r--r--\troot/root\tusr/share/tool/a\n\
         lrwxrwxrwx\troot/root\tusr/share/tool/link -> a\n"
    );

    // Same result: nothing to report
    let r = pqmake(&env, &["-q", "-f", "tool"]);
    assert!(r.ok() && !r.stdout.contains("ootprint"), "{}", r.all());

    // The program becomes setuid, a file is renamed, a link changes and the
    // man page is gone
    tool(
        "2",
        r#"mkdir -p "$PKG/usr/bin" "$PKG/usr/share/tool"
    printf '#!/bin/sh\n' > "$PKG/usr/bin/tool"; chmod 4755 "$PKG/usr/bin/tool"
    echo b > "$PKG/usr/share/tool/b"
    ln -s b "$PKG/usr/share/tool/link""#,
    );
    let r = pqmake(&env, &["-q", "tool"]);
    assert!(r.ok(), "the build still succeeds: {}", r.all());
    let expected = "==> Footprint of tool changed (base/tool/.footprint):\n\
                    \x20   CHANGED  -rwsr-xr-x root/root usr/bin/tool  (was -rwxr-xr-x: setuid added)\n\
                    \x20   MISSING  -rw-r--r-- root/root usr/share/tool/a\n\
                    \x20   NEW      -rw-r--r-- root/root usr/share/tool/b\n\
                    \x20   CHANGED  lrwxrwxrwx root/root usr/share/tool/link -> b  (was -> a)\n\
                    ==> tool.fr is no longer produced (base/tool/.footprint.fr)\n\
                    \x20   If these changes are expected: pqmake --update-footprint tool\n";
    assert!(r.stdout.contains(expected), "{}", r.stdout);
    assert!(r.stdout.contains("==> Footprint changed: tool (see above)"), "{}", r.stdout);
    assert_eq!(fs::read_to_string(env.path("recipes/base/tool/.footprint")).unwrap(), footprint, "kept as is");

    // The packager accepts the changes
    let r = pqmake(&env, &["--update-footprint", "tool"]);
    assert!(r.ok(), "{}", r.all());
    assert_eq!(r.stdout, "  base/tool/.footprint updated\n  base/tool/.footprint.fr removed\n");
    assert!(
        fs::read_to_string(env.path("recipes/base/tool/.footprint"))
            .unwrap()
            .contains("-rwsr-xr-x\troot/root\tusr/bin/tool\n")
    );
    assert!(!env.path("recipes/base/tool/.footprint.fr").exists());
    assert_eq!(pqmake(&env, &["-u", "tool"]).stdout, "  footprints of tool are up to date\n");
    let r = pqmake(&env, &["-q", "-f", "tool"]);
    assert!(r.ok() && !r.stdout.contains("ootprint"), "{}", r.all());

    // Only a built recipe has archives to take the footprint from
    tool("3", ":");
    let r = pqmake(&env, &["-u", "tool"]);
    assert!(!r.ok() && r.stderr.contains("tool 1-3 is not built"), "{}", r.all());
}

#[test]
fn provided_build_styles() {
    let styles = Path::new(env!("CARGO_MANIFEST_DIR")).join("styles");
    let mut names: Vec<String> =
        fs::read_dir(&styles).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(
        names,
        ["autogen", "autoreconf", "autotools", "cmake", "kde5", "kde6", "meson", "pip3", "plasma", "python3", "xorg"]
    );
    for name in &names {
        let ok = std::process::Command::new("bash").arg("-n").arg(styles.join(name)).status().unwrap().success();
        assert!(ok, "{name} is not valid bash");
    }

    // Every style is found in STYLES_DIR
    let env = Env::new();
    fs::write(
        env.path("pqmake.conf"),
        format!(
            "RECIPES_DIR={}\nREPO_DIR={}\nWORK_DIR={}\nSOURCES_DIR={}\nLOG_DIR={}\nSTYLES_DIR={}\nMAKEFLAGS=-j1\n",
            env.path("recipes").display(),
            env.repo_dir().display(),
            env.path("work").display(),
            env.path("sources").display(),
            env.path("logs").display(),
            styles.display()
        ),
    )
    .unwrap();
    for name in &names {
        let recipe_name = format!("with{name}");
        recipe(&env, "base", &recipe_name, &format!("name={recipe_name}\nversion=1\nbuild={name}\n"));
    }
    let all: Vec<String> = names.iter().map(|n| format!("with{n}")).collect();
    let args: Vec<&str> = std::iter::once("-n").chain(all.iter().map(String::as_str)).collect();
    let r = pqmake(&env, &args);
    assert!(r.ok(), "{}", r.all());

    // A real build with the autotools style
    if !pqmake::command_exists("make") {
        return;
    }
    let src = env.path("autosrc/fake-2.0");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("configure"),
        "#!/bin/sh\necho \"configured with $*\" > args\n\
         printf 'all:\\n\\ttrue\\ninstall:\\n\\tmkdir -p $(DESTDIR)/usr/share/fake\\n\\tcp args $(DESTDIR)/usr/share/fake/\\n' > Makefile\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(src.join("configure"), fs::Permissions::from_mode(0o755)).unwrap();
    recipe(&env, "base", "fake", "name=fake\nversion=2.0\nsource=(fake-2.0.tar.gz)\nbuild=autotools\n");
    let ok = std::process::Command::new("tar")
        .arg("-czf")
        .arg(env.path("recipes/base/fake/fake-2.0.tar.gz"))
        .arg("-C")
        .arg(env.path("autosrc"))
        .arg("fake-2.0")
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let r = pqmake(&env, &["-q", "fake"]);
    assert!(r.ok(), "{}", r.all());
    let archive = env.repo_dir().join("base/fake-2.0-1.tar.zst");
    let root = env.path("extracted");
    fs::create_dir_all(&root).unwrap();
    paquet::archive::extract(&archive, &root).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("usr/share/fake/args")).unwrap(),
        "configured with --prefix=/usr --sysconfdir=/etc --localstatedir=/var --disable-static\n"
    );
}

#[test]
fn sources_packages() {
    let env = Env::new();
    // Upstream: an HTTP server holding the source archive
    fs::create_dir_all(env.path("upstream/srcdemo-1.0")).unwrap();
    fs::write(env.path("upstream/srcdemo-1.0/hello.txt"), "from upstream\n").unwrap();
    let ok = std::process::Command::new("tar")
        .arg("-czf")
        .arg(env.path("upstream/srcdemo-1.0.tar.gz"))
        .arg("-C")
        .arg(env.path("upstream"))
        .arg("srcdemo-1.0")
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let (url, served) = common::serve(env.path("upstream"));
    let gets = || served.requests.lock().unwrap().iter().filter(|r| r.starts_with("GET ")).count();
    let build = |release: &str, extra: &str| -> Run {
        recipe(
            &env,
            "base",
            "srcdemo",
            &format!(
                "name=srcdemo\nversion=1.0\nrelease={release}\ndescription=\"Demo\"\n{extra}\n\
                 source=({url}/srcdemo-1.0.tar.gz local.patch)\n\
                 build() {{\n    test -f local.patch\n    mkdir -p \"$PKG/usr/share/srcdemo\"\n    \
                 cp srcdemo-1.0/hello.txt \"$PKG/usr/share/srcdemo/\"\n}}\n"
            ),
        );
        let config = env.path("pqmake.conf").display().to_string();
        let home = env.tmp.path().display().to_string();
        env.cmd_env(
            "pqmake",
            &["srcdemo"],
            None,
            &[
                ("PQMAKE_CONFIG", &config),
                ("PQMAKE_ALLOW_ROOT", "1"),
                ("HOME", &home),
                ("NO_PROXY", "127.0.0.1"),
                ("no_proxy", "127.0.0.1"),
            ],
        )
    };
    pqmake(&env, &["-n", "nothing"]); // writes the configuration
    fs::create_dir_all(env.path("recipes/base/srcdemo")).unwrap();
    fs::write(env.path("recipes/base/srcdemo/local.patch"), "patch\n").unwrap();
    fs::write(env.path("recipes/base/srcdemo/post-install"), "echo installed\n").unwrap();

    // First build: downloaded, then kept in the sources package with the
    // recipe
    let r = build("1", "");
    assert!(r.ok(), "{}", r.all());
    assert_eq!(gets(), 1);
    let repo = env.repo_dir();
    let sources = repo.join("base/srcdemo-1.0-1-sources.tar.zst");
    assert!(repo.join("base/srcdemo-1.0-1.tar.zst").is_file());
    let mut names: Vec<String> = paquet::archive::list(&sources)
        .unwrap()
        .iter()
        .map(|e| String::from_utf8_lossy(&e.name).into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ".META",
            "usr",
            "usr/src",
            "usr/src/srcdemo-1.0",
            "usr/src/srcdemo-1.0/.footprint",
            "usr/src/srcdemo-1.0/Pkgfile",
            "usr/src/srcdemo-1.0/local.patch",
            "usr/src/srcdemo-1.0/post-install",
            "usr/src/srcdemo-1.0/srcdemo-1.0.tar.gz",
        ]
    );
    let meta = paquet::archive::read_meta(&sources).unwrap().unwrap();
    assert!(meta.starts_with("Description: Demo (sources)\nGroups: sources\n"), "{meta}");
    assert!(!env.path("recipes/base/srcdemo/.footprint.sources").exists(), "no footprint for the sources");

    // Not in the cache any more: taken from the sources package in the
    // repository, not downloaded
    fs::remove_dir_all(env.path("sources/srcdemo")).unwrap();
    let r = build("2", "");
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains(&format!("Using srcdemo-1.0.tar.gz from {}", sources.display())), "{}", r.stdout);
    assert_eq!(gets(), 1);
    assert!(env.path("sources/srcdemo/srcdemo-1.0.tar.gz").is_file());

    // Installed sources (USR_SRC_DIR) come before the repository
    let usr_src = env.path("usrsrc/srcdemo-1.0");
    fs::create_dir_all(&usr_src).unwrap();
    fs::copy(env.path("upstream/srcdemo-1.0.tar.gz"), usr_src.join("srcdemo-1.0.tar.gz")).unwrap();
    let mut config = fs::read_to_string(env.path("pqmake.conf")).unwrap();
    config.push_str(&format!("USR_SRC_DIR={}\n", env.path("usrsrc").display()));
    fs::write(env.path("pqmake.conf"), config).unwrap();
    fs::remove_dir_all(env.path("sources/srcdemo")).unwrap();
    let r = build("3", "");
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains(&format!("Using srcdemo-1.0.tar.gz from {}", usr_src.display())), "{}", r.stdout);
    assert_eq!(gets(), 1);

    // The sources package installs in /usr/src/<name>-<version>/
    paquet::genpqrepo::generate(&repo, &repo.join("PQREPO"), None, None).unwrap();
    assert!(env.pq(&["-S", "--nosign"]).ok());
    let r = env.pq(&["-i", "srcdemo.sources"]);
    assert!(r.ok(), "{}", r.all());
    assert_eq!(fs::read_to_string(env.rootpath("usr/src/srcdemo-1.0/local.patch")).unwrap(), "patch\n");

    // PKGMK_IGNORE_GROUPS=(sources): no sources package
    let r = build("4", "PKGMK_IGNORE_GROUPS=(sources nope)");
    assert!(r.ok(), "{}", r.all());
    assert!(r.stderr.contains("PKGMK_IGNORE_GROUPS: unknown group 'nope'"), "{}", r.stderr);
    assert!(repo.join("base/srcdemo-1.0-4.tar.zst").is_file());
    assert!(!repo.join("base/srcdemo-1.0-4-sources.tar.zst").exists());
}

#[test]
fn translations_are_split_by_language() {
    let env = Env::new();
    let polyglot = |release: &str, extra: &str| {
        recipe(
            &env,
            "base",
            "polyglot",
            &format!(
                r#"name=polyglot
version=1
release={release}
description="Speaks"
{extra}
build() {{
    mkdir -p "$PKG/usr/bin" "$PKG/usr/share/man/man1" "$PKG/usr/share/man/fr/man1"
    echo program > "$PKG/usr/bin/polyglot"
    for d in fr fr_CA de en_GB en@quot pt_BR; do
        mkdir -p "$PKG/usr/share/locale/$d/LC_MESSAGES"
        echo $d > "$PKG/usr/share/locale/$d/LC_MESSAGES/polyglot.mo"
    done
    echo alias > "$PKG/usr/share/locale/locale.alias"
    echo man > "$PKG/usr/share/man/man1/polyglot.1"
    echo page > "$PKG/usr/share/man/fr/man1/polyglot.1"
    mkdir -p "$PKG/usr/share/help/C" "$PKG/usr/share/help/de"
    echo help > "$PKG/usr/share/help/C/index.page"
    echo hilfe > "$PKG/usr/share/help/de/index.page"
}}
"#
            ),
        );
    };
    polyglot("1", "");
    let r = pqmake(&env, &["-q", "polyglot"]);
    assert!(r.ok(), "{}", r.all());
    let repo = env.repo_dir().join("base");
    let files = |archive: &str| -> Vec<String> {
        paquet::archive::list(&repo.join(archive))
            .unwrap()
            .iter()
            .filter(|e| !e.is_dir && !e.name.starts_with(b"."))
            .map(|e| String::from_utf8_lossy(&e.name).into_owned())
            .collect()
    };
    // English stays in the main package
    assert_eq!(
        files("polyglot-1-1.tar.zst"),
        [
            "usr/bin/polyglot",
            "usr/share/help/C/index.page",
            "usr/share/locale/en@quot/LC_MESSAGES/polyglot.mo",
            "usr/share/locale/en_GB/LC_MESSAGES/polyglot.mo",
            "usr/share/locale/locale.alias",
            "usr/share/man/man1/polyglot.1",
        ]
    );
    // One package per base language, translated manual pages included
    assert_eq!(
        files("polyglot-1-1-fr.tar.zst"),
        [
            "usr/share/locale/fr/LC_MESSAGES/polyglot.mo",
            "usr/share/locale/fr_CA/LC_MESSAGES/polyglot.mo",
            "usr/share/man/fr/man1/polyglot.1",
        ]
    );
    assert_eq!(
        files("polyglot-1-1-de.tar.zst"),
        ["usr/share/help/de/index.page", "usr/share/locale/de/LC_MESSAGES/polyglot.mo"]
    );
    assert_eq!(files("polyglot-1-1-pt.tar.zst"), ["usr/share/locale/pt_BR/LC_MESSAGES/polyglot.mo"]);
    assert!(!repo.join("polyglot-1-1-man.tar.zst").exists());
    let meta = paquet::archive::read_meta(&repo.join("polyglot-1-1-fr.tar.zst")).unwrap().unwrap();
    assert_eq!(meta, "Description: Speaks (fr)\nGroups: fr\nSet: base\nDepends: polyglot\n");
    assert!(env.path("recipes/base/polyglot/.footprint.fr").is_file());

    // A language kept in the main package
    polyglot("2", "PKGMK_IGNORE_GROUPS=(de)");
    let r = pqmake(&env, &["-q", "polyglot"]);
    assert!(r.ok(), "{}", r.all());
    assert!(!repo.join("polyglot-1-2-de.tar.zst").exists());
    assert!(files("polyglot-1-2.tar.zst").contains(&"usr/share/locale/de/LC_MESSAGES/polyglot.mo".to_string()));
    assert!(r.stdout.contains("==> polyglot.de is no longer produced (base/polyglot/.footprint.de)"), "{}", r.stdout);

    // The repository index knows the language packages
    env.index(None);
    let index = fs::read_to_string(env.repo_dir().join("PQREPO")).unwrap();
    assert!(index.contains(";polyglot.fr;1;2;"), "{index}");
}
