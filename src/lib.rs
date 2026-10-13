//! Great-OS package tools.
//!
//! - [`paquet`]: installs, upgrades and removes the packages of a system
//!   (the `paquet` and `pq` commands).
//! - [`genpqrepo`]: generates the `PQREPO` index of a mirror.
//! - [`pqmake`]: builds packages from their recipes (`Pkgfile`),
//!   dependencies first.
//!
//! The three tools share the formats of the original shell scripts, so a
//! system, a mirror or a recipe tree can move from one to the other.

pub mod archive;
pub mod config;
pub mod fetch;
pub mod genpqrepo;
pub mod paquet;
pub mod pqmake;
pub mod record;
pub mod sign;
pub mod signals;
pub mod util;
pub mod version;

pub use util::{Interrupted, Reported};

/// Runs a command's main function and turns its result into an exit code.
/// Errors are printed as "Error: <message>", followed by their causes.
pub fn run_main(main: impl FnOnce() -> anyhow::Result<()>) -> i32 {
    match main() {
        Ok(()) => 0,
        Err(err) => exit_code(&err),
    }
}

/// Prints an error (unless it was already reported) and returns the exit
/// code it stands for.
pub fn exit_code(err: &anyhow::Error) -> i32 {
    if let Some(Interrupted(signal)) = err.downcast_ref::<Interrupted>() {
        return 128 + signal;
    }
    if err.downcast_ref::<Reported>().is_none() {
        util::report(err);
    }
    1
}
