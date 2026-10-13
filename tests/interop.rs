//! Compatibility with the files of the shell tools: Ed25519 keys and
//! signatures made by OpenSSL, archives made by GNU tar. Skipped when
//! openssl or tar is missing.

mod common;

use std::fs;
use std::process::Command;

use common::Env;

fn have(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok_and(|o| o.status.success())
        || Command::new(program).arg("version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn openssl_keys_and_signatures() {
    if !have("openssl") {
        eprintln!("openssl not found, skipped");
        return;
    }
    let env = Env::new();
    env.package("base", "foo", "1.0", &[("usr/bin/foo", "foo")], "");

    // A key made by openssl signs the index; openssl verifies it
    let key = env.path("ossl.key");
    let public = env.path("ossl.pub");
    assert!(
        Command::new("openssl")
            .args(["genpkey", "-algorithm", "ed25519", "-out"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("openssl")
            .args(["pkey", "-pubout", "-in"])
            .arg(&key)
            .arg("-out")
            .arg(&public)
            .status()
            .unwrap()
            .success()
    );
    env.index(Some(&key));
    let index = env.repo_dir().join("PQREPO");
    let verified = Command::new("openssl")
        .args(["pkeyutl", "-verify", "-pubin", "-rawin", "-inkey"])
        .arg(&public)
        .arg("-in")
        .arg(&index)
        .arg("-sigfile")
        .arg(env.repo_dir().join("PQREPO.sig"))
        .output()
        .unwrap();
    assert!(verified.status.success(), "{}", String::from_utf8_lossy(&verified.stdout));

    // A key made by genpqrepo signs with openssl; paquet verifies it
    let our_key = env.genkey();
    assert!(
        Command::new("openssl")
            .args(["pkeyutl", "-sign", "-rawin", "-inkey"])
            .arg(&our_key)
            .arg("-in")
            .arg(&index)
            .arg("-out")
            .arg(env.repo_dir().join("PQREPO.sig"))
            .status()
            .unwrap()
            .success()
    );
    let r = env.pq(&["-S"]);
    assert!(r.ok(), "{}", r.all());
    assert!(r.stdout.contains("Signature verified with repo.pub"));
}

#[test]
fn gnu_tar_archives() {
    if !have("tar") {
        eprintln!("tar not found, skipped");
        return;
    }
    let env = Env::new();
    let stage = env.path("stage/gnu");
    fs::create_dir_all(stage.join("usr/share/gnu")).unwrap();
    fs::write(stage.join("usr/share/gnu/data"), "gnu data").unwrap();
    std::os::unix::fs::symlink("data", stage.join("usr/share/gnu/link")).unwrap();
    fs::write(stage.join(".META"), "Description: Made by GNU tar\nGroups: \nSet: base\nDepends: dep1,dep2\n").unwrap();

    // As pqmake.sh did: tar --zstd --sort=name --owner=0 --group=0 --numeric-owner
    let tar = Command::new("tar")
        .args(["--sort=name", "--owner=0", "--group=0", "--numeric-owner", "-cf", "-", "-C"])
        .arg(&stage)
        .arg(".")
        .output()
        .unwrap();
    assert!(tar.status.success());
    let dir = env.repo_dir().join("base");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("gnu-1.0-1.tar.zst"), zstd::encode_all(tar.stdout.as_slice(), 3).unwrap()).unwrap();
    fs::create_dir_all(env.repo_dir().join("base")).unwrap();
    env.package("base", "dep1", "1", &[("usr/bin/dep1", "1")], "");
    env.package("base", "dep2", "1", &[("usr/bin/dep2", "2")], "");

    env.index(None);
    let index = fs::read_to_string(env.repo_dir().join("PQREPO")).unwrap();
    assert!(index.contains(";base;gnu;1.0;1;Made by GNU tar;;base;;dep1 dep2\n"), "{index}");

    let r = env.pq(&["-S", "--nosign"]);
    assert!(r.ok(), "{}", r.all());
    let r = env.pq(&["-i", "gnu"]);
    assert!(r.ok(), "{}", r.all());
    assert_eq!(fs::read_to_string(env.rootpath("usr/share/gnu/link")).unwrap(), "gnu data");
    let list = fs::read_to_string(env.rootpath("var/lib/pq/files/gnu")).unwrap();
    // /usr already exists: the dependencies, installed first, created it
    assert_eq!(list, "# root: /\n/usr/share\n/usr/share/gnu\n/usr/share/gnu/data\n/usr/share/gnu/link\n");
    assert_eq!(env.status("dep1"), "A");
}
