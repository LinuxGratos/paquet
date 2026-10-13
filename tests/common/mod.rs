#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A mirror, a managed root and the configuration tying them, in a
/// temporary directory.
pub struct Env {
    pub tmp: tempfile::TempDir,
    pub arch: String,
}

pub struct Run {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    fn from(out: Output) -> Run {
        Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    pub fn ok(&self) -> bool {
        self.status == 0
    }

    pub fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

impl Env {
    pub fn new() -> Env {
        let env = Env { tmp: tempfile::tempdir().unwrap(), arch: paquet::util::machine_arch() };
        fs::create_dir_all(env.repo_dir()).unwrap();
        fs::create_dir_all(env.root()).unwrap();
        fs::create_dir_all(env.path("keys")).unwrap();
        fs::write(env.path("pq.conf"), format!("MIRROR=\"file://{}\"\nBRANCH=stable\n", env.path("mirror").display()))
            .unwrap();
        env
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    pub fn repo_dir(&self) -> PathBuf {
        self.path(&format!("mirror/stable/{}", self.arch))
    }

    pub fn root(&self) -> PathBuf {
        self.path("root")
    }

    pub fn rootpath(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    /// Writes an archive `<folder>/<name>-<version>-<release>.tar.zst` in the
    /// mirror. `files` are (path, content) pairs; `depends` goes to .META.
    pub fn package(&self, folder: &str, name: &str, version: &str, files: &[(&str, &str)], depends: &str) -> PathBuf {
        let stage = self.path(&format!("stage/{name}-{version}"));
        let _ = fs::remove_dir_all(&stage);
        fs::create_dir_all(&stage).unwrap();
        for (path, content) in files {
            let full = stage.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, content).unwrap();
        }
        let mut meta = format!("Description: The {name} package\nGroups: \nSet: {folder}\n");
        if !depends.is_empty() {
            meta.push_str(&format!("Depends: {depends}\n"));
        }
        fs::write(stage.join(".META"), meta).unwrap();
        let dir = self.repo_dir().join(folder);
        fs::create_dir_all(&dir).unwrap();
        let archive = dir.join(paquet::util::archive_file_name(name, version, "1"));
        paquet::archive::create(&stage, &archive, false).unwrap();
        archive
    }

    pub fn genkey(&self) -> PathBuf {
        let r = self.cmd("genpqrepo", &["--genkey", self.path("repo").to_str().unwrap()], None);
        assert!(r.ok(), "{}", r.all());
        fs::copy(self.path("repo.pub"), self.path("keys/repo.pub")).unwrap();
        self.path("repo.key")
    }

    pub fn index(&self, key: Option<&Path>) -> Run {
        let repo = self.repo_dir();
        let mut args = Vec::new();
        if let Some(k) = key {
            args.push("-k".to_string());
            args.push(k.display().to_string());
        }
        args.push(repo.display().to_string());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let r = self.cmd("genpqrepo", &args, None);
        assert!(r.ok(), "{}", r.all());
        r
    }

    pub fn cmd_env(&self, bin: &str, args: &[&str], input: Option<&str>, extra: &[(&str, &str)]) -> Run {
        let exe = match bin {
            "paquet" => env!("CARGO_BIN_EXE_paquet"),
            "pq" => env!("CARGO_BIN_EXE_pq"),
            "genpqrepo" => env!("CARGO_BIN_EXE_genpqrepo"),
            "pqmake" => env!("CARGO_BIN_EXE_pqmake"),
            _ => panic!("unknown binary {bin}"),
        };
        let mut cmd = Command::new(exe);
        cmd.args(args)
            .env("PQ_CONFIG", self.path("pq.conf"))
            .env("PQ_KEYS_DIR", self.path("keys"))
            .env("PQ_TEST_NONROOT", "1")
            .env_remove("PQ_SIGN_KEY")
            .env_remove("PQ_TEST_CRASH")
            .current_dir(self.tmp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            if let Some(input) = input {
                stdin.write_all(input.as_bytes()).unwrap();
            }
        }
        Run::from(child.wait_with_output().unwrap())
    }

    pub fn cmd(&self, bin: &str, args: &[&str], input: Option<&str>) -> Run {
        self.cmd_env(bin, args, input, &[])
    }

    /// Runs paquet on the managed root.
    pub fn pq(&self, args: &[&str]) -> Run {
        self.pq_env(args, None, &[])
    }

    pub fn pq_env(&self, args: &[&str], input: Option<&str>, extra: &[(&str, &str)]) -> Run {
        let root = self.root().display().to_string();
        let mut all: Vec<&str> = args.to_vec();
        all.push("--chroot");
        all.push(&root);
        self.cmd_env("paquet", &all, input, extra)
    }

    /// Gives the managed root a working /bin/sh (the host's, with its
    /// libraries), so that package scripts and hooks can run in it. Needs
    /// root (for chroot): returns false, and the test should stop, otherwise.
    pub fn shell_root(&self) -> bool {
        if paquet::util::euid() != 0 {
            eprintln!("not running as root: scripts cannot run in a chroot, test skipped");
            return false;
        }
        let Ok(sh) = fs::canonicalize("/bin/sh") else { return false };
        let Ok(ldd) = Command::new("ldd").arg(&sh).output() else { return false };
        let mut files = vec![(sh, PathBuf::from("bin/sh"))];
        for word in String::from_utf8_lossy(&ldd.stdout).split_whitespace() {
            if let Some(rel) = word.strip_prefix('/') {
                files.push((PathBuf::from(word), PathBuf::from(rel)));
            }
        }
        for (src, rel) in files {
            let dest = self.root().join(rel);
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            fs::copy(&src, &dest).unwrap();
        }
        true
    }

    /// Status of a package in the state of the managed root ("" if absent).
    pub fn status(&self, name: &str) -> String {
        let state = fs::read_to_string(self.rootpath("var/lib/pq/PQREPOSTATE")).unwrap_or_default();
        for line in state.lines() {
            let f: Vec<&str> = line.splitn(11, ';').collect();
            if f.get(3) == Some(&name) {
                return f.get(9).unwrap_or(&"").to_string();
            }
        }
        String::new()
    }
}

/// What a test HTTP server saw.
#[derive(Default)]
pub struct Served {
    /// Requests, as "<method> <path>".
    pub requests: std::sync::Mutex<Vec<String>>,
    active: std::sync::atomic::AtomicUsize,
    /// Most transfers in progress at the same time.
    pub max_active: std::sync::atomic::AtomicUsize,
}

/// Serves the files of a directory over HTTP on 127.0.0.1, in the
/// background, answering HEAD and GET (404 for missing files). Bodies are
/// sent slowly, so that simultaneous downloads overlap. Returns the base URL.
pub fn serve(dir: PathBuf) -> (String, std::sync::Arc<Served>) {
    use std::io::{BufRead, BufReader};
    use std::sync::atomic::Ordering;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let served = std::sync::Arc::new(Served::default());
    let stats = served.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (dir, stats) = (dir.clone(), stats.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    return;
                }
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let mut words = request.split_whitespace();
                let (method, path) = (words.next().unwrap_or("").to_string(), words.next().unwrap_or("").to_string());
                stats.requests.lock().unwrap().push(format!("{method} {path}"));
                let Ok(body) = fs::read(dir.join(path.trim_start_matches('/'))) else {
                    let _ =
                        stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    return;
                };
                let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(header.as_bytes());
                if method == "GET" {
                    let now = stats.active.fetch_add(1, Ordering::SeqCst) + 1;
                    stats.max_active.fetch_max(now, Ordering::SeqCst);
                    for chunk in body.chunks(body.len().div_ceil(4).max(1)) {
                        std::thread::sleep(std::time::Duration::from_millis(60));
                        if stream.write_all(chunk).is_err() {
                            break;
                        }
                    }
                    stats.active.fetch_sub(1, Ordering::SeqCst);
                }
            });
        }
    });
    (url, served)
}
