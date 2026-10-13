//! End-to-end tests of pqmake (host builds), then of the built repository
//! with paquet.

mod common;

use std::fs;
use std::path::Path;

use common::{Env, Run};

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
        "base/libfake.devel-1.0_rc1-2.tar.zst",
        "extra/hello-2.1-1.tar.zst",
        "extra/hello.man-2.1-1.tar.zst",
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
    let devel = meta("base/libfake.devel-1.0_rc1-2.tar.zst");
    assert!(
        devel.contains("Description: Fake C library (devel)\nGroups: devel\nSet: base\nDepends: libfake\n"),
        "{devel}"
    );
    let names: Vec<String> = paquet::archive::list(&repo.join("extra/hello.man-2.1-1.tar.zst"))
        .unwrap()
        .iter()
        .map(|e| String::from_utf8_lossy(&e.name).into_owned())
        .collect();
    assert!(names.contains(&"usr/share/man/man1/hello.1".to_string()), "{names:?}");
    let names: Vec<String> = paquet::archive::list(&repo.join("extra/hello-2.1-1.tar.zst"))
        .unwrap()
        .iter()
        .map(|e| String::from_utf8_lossy(&e.name).into_owned())
        .collect();
    assert!(!names.iter().any(|n| n.contains("/man/")), "{names:?}");
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
