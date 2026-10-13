//! Command line of `paquet` (and of its alias `pq`).

use std::path::PathBuf;

use anyhow::{Result, bail};

use super::{Paquet, Paths};
use crate::{Reported, config, signals, util};

pub fn display_help(prog: &str, keys_dir: &std::path::Path) {
    println!("Usage: {prog} [OPTION] [--chroot <path>]");
    println!("Package Manager for Great-OS");
    println!();
    println!("Options:");
    println!("  -S, --sync [--nosign]        Fetch the repository file, check its signature and");
    println!("                               refresh package status (--nosign skips the check)");
    println!("  -i, --install <package_name> Install a package and its missing dependencies");
    println!("  -l, --list                   List all packages");
    println!("  -s, --search <search_term>   Search for a package by name, description, folder, or set");
    println!("  -r, --remove <package_name>  Remove a package no installed package depends on");
    println!("  -u, --upgrade [package_name] Upgrade a package, or all packages with status 'U'");
    println!("                               (dependencies first)");
    println!("  -o, --orphans                List automatically installed packages no longer needed");
    println!("  -Q, --query <file>           Show the package owning a file");
    println!("  -I, --info <package_name>    Show what is known about a package");
    println!("  -F, --files <package_name>   List the files of an installed package");
    println!("  autoremove, --autoremove     Remove all orphan packages");
    println!("  --mark-auto <package_name>   Mark an installed package as installed automatically");
    println!("  --mark-explicit <package_name> Mark an installed package as installed explicitly");
    println!("  --cache-info                 Show the cached archives and their size");
    println!("  --purge-old-cache            Remove cached archives superseded by a newer version");
    println!("  -C, --cache                  Show the cache size and offer to clear it");
    println!("  --recover                    Roll back an install or upgrade interrupted by a crash");
    println!("                               (done automatically by the next command that changes the system)");
    println!("  -h, --help                   Display this help message");
    println!();
    println!("  --chroot <path>              Manage the system installed under <path>");
    println!("                               (its database lives in <path>/var/lib/pq)");
    println!("  --noscripts                  Run neither the package scripts nor the hooks");
    println!("                               (with -i, -r, -u, autoremove)");
    println!();
    println!("Trusted signing keys: {}/*.pub", keys_dir.display());
    println!();
    println!("Status: N not installed, I installed explicitly, A installed automatically;");
    println!("        U update available, X no longer in the repository.");
}

/// Entry point of the `paquet` and `pq` commands.
pub fn main() -> i32 {
    signals::install();
    let prog = util::program_name("paquet");
    let args: Vec<String> = std::env::args().skip(1).collect();
    crate::run_main(|| run(&prog, args))
}

fn run(prog: &str, raw_args: Vec<String>) -> Result<()> {
    // --chroot, --nosign and --noscripts may appear anywhere
    let mut chroot: Option<PathBuf> = None;
    let mut nosign = false;
    let mut noscripts = false;
    let mut args = Vec::new();
    let mut iter = raw_args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--nosign" => nosign = true,
            "--noscripts" => noscripts = true,
            "--chroot" => match iter.next() {
                Some(dir) if !dir.is_empty() && !dir.starts_with('-') => chroot = Some(dir.into()),
                _ => bail!("--chroot requires a path argument."),
            },
            _ => args.push(arg),
        }
    }
    let command = args.first().map(String::as_str).unwrap_or("");
    let operand = args.get(1).map(String::as_str).filter(|s| !s.is_empty());

    let mut paths = Paths::system();
    if matches!(command, "-h" | "--help") {
        display_help(prog, &paths.keys_dir);
        return Ok(());
    }
    if let Some(dir) = &chroot {
        paths = paths.with_root(dir)?;
    }
    if nosign && !matches!(command, "-S" | "--sync") {
        bail!("--nosign only applies to -S, --sync.");
    }
    if noscripts
        && !matches!(
            command,
            "-i" | "--install" | "-r" | "--remove" | "-u" | "--upgrade" | "autoremove" | "--autoremove"
        )
    {
        bail!("--noscripts only applies to -i, -r, -u and autoremove.");
    }

    if !paths.config_file.is_file() {
        bail!("Configuration file {} not found!", paths.config_file.display());
    }
    let (mut mirror, mut branch) = (String::new(), String::new());
    for (key, value) in config::read(&paths.config_file)? {
        match key.as_str() {
            "MIRROR" => mirror = value,
            "BRANCH" => branch = value,
            _ => eprintln!("Warning: ignoring unknown setting {key} in {}", paths.config_file.display()),
        }
    }
    if branch.is_empty() {
        branch = "stable".into();
    }
    let mut pq = Paquet::new(paths, mirror, branch, util::machine_arch(), prog.to_string());
    pq.nosign = nosign;
    pq.noscripts = noscripts;

    // Package names end up in paths: reject anything unsafe early
    if matches!(
        command,
        "-i" | "--install"
            | "-r"
            | "--remove"
            | "-u"
            | "--upgrade"
            | "--mark-auto"
            | "--mark-explicit"
            | "-I"
            | "--info"
            | "-F"
            | "--files"
    ) {
        if let Some(name) = operand {
            if !util::is_valid_name(name) {
                bail!("invalid package name '{name}'.");
            }
        }
    }

    // Commands that modify the system need root and exclusive access, and
    // first roll back any operation a crash left unfinished
    let modifies = matches!(
        command,
        "-S" | "--sync"
            | "-i"
            | "--install"
            | "-r"
            | "--remove"
            | "-u"
            | "--upgrade"
            | "autoremove"
            | "--autoremove"
            | "--mark-auto"
            | "--mark-explicit"
            | "--purge-old-cache"
            | "-C"
            | "--cache"
            | "--recover"
    );
    if modifies {
        // PQ_TEST_NONROOT is a test hook, like PQ_TEST_CRASH
        if util::euid() != 0 && std::env::var_os("PQ_TEST_NONROOT").is_none() {
            bail!("this command must be run as root.");
        }
        pq.acquire_lock()?;
        if command != "--recover" {
            pq.recover(false);
        }
    } else if pq.has_pending_transaction() {
        eprintln!("Warning: an interrupted operation is pending; run '{prog} --recover' as root.");
    }

    let need_operand = |what: &str| -> Result<&str> {
        match operand {
            Some(op) => Ok(op),
            None => {
                eprintln!("Error: No {what} provided.");
                display_help(prog, &pq.paths.keys_dir);
                Err(Reported.into())
            }
        }
    };

    let result = match command {
        "-S" | "--sync" => pq.sync(),
        "-i" | "--install" => {
            let name = need_operand("package name")?.to_string();
            pq.install(&name)
        }
        "-r" | "--remove" => {
            let name = need_operand("package name")?.to_string();
            pq.remove(&name, false)
        }
        "--mark-auto" | "--mark-explicit" => {
            let name = need_operand("package name")?.to_string();
            pq.mark(&name, if command == "--mark-auto" { 'A' } else { 'I' })
        }
        "-l" | "--list" => pq.list(),
        "-s" | "--search" => {
            let term = need_operand("search term")?.to_string();
            pq.search(&term)
        }
        "-u" | "--upgrade" => match operand {
            Some(name) => {
                let name = name.to_string();
                pq.upgrade(&name)
            }
            None => pq.upgrade_all(),
        },
        "-Q" | "--query" => {
            let file = need_operand("file")?.to_string();
            pq.query(&file)
        }
        "-I" | "--info" => {
            let name = need_operand("package name")?.to_string();
            pq.info(&name)
        }
        "-F" | "--files" => {
            let name = need_operand("package name")?.to_string();
            pq.files(&name)
        }
        "-o" | "--orphans" => {
            pq.list_orphans();
            Ok(())
        }
        "autoremove" | "--autoremove" => pq.autoremove(),
        "--cache-info" => {
            pq.cache_info();
            Ok(())
        }
        "--purge-old-cache" => {
            pq.purge_old_cache();
            Ok(())
        }
        "--recover" => {
            pq.recover(true);
            Ok(())
        }
        "-C" | "--cache" => {
            pq.manage_cache();
            Ok(())
        }
        _ => {
            eprintln!("Invalid option. Use -h or --help for usage information.");
            display_help(prog, &pq.paths.keys_dir);
            Err(Reported.into())
        }
    };

    // Hooks run once, for everything the command installed or removed
    if !matches!(&result, Err(err) if err.downcast_ref::<crate::Interrupted>().is_some()) {
        pq.run_hooks();
    }
    if result.is_ok() && pq.scripts_failed() {
        return Err(Reported.into());
    }
    result
}
