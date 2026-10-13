//! Downloads of package archives: several at a time, each checked against
//! its checksum, with a progress line on a terminal.
//!
//! ```text
//! Downloading 3 packages (1.4 MiB):
//!   ✓ libedit 1.1-1      12.0 KiB
//!   ✓ zlib 1.3.1-1      142.0 KiB
//!   ━━━━━━━━━━━━━━━━━━━━━━━━━━━━──────────────   64%   0.9 MiB / 1.4 MiB   2.3 MiB/s
//! ```
//!
//! Without a terminal, only the lines of the finished downloads are written.

use std::fs;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use crate::fetch;
use crate::util::{sha256_file, with_suffix};

/// Number of simultaneous downloads, unless `PARALLEL_DOWNLOADS` says
/// otherwise.
pub const DEFAULT_JOBS: usize = 4;
pub const MAX_JOBS: usize = 16;

/// An archive to download.
pub struct Item {
    /// `<name> <version>-<release>`, for the messages.
    pub label: String,
    pub url: String,
    /// Path in the cache, written once the checksum is right.
    pub dest: PathBuf,
    pub checksum: String,
    /// Size announced by the mirror, if known.
    pub size: Option<u64>,
}

/// Applies `f` to every item, with up to `jobs` threads. The results are
/// in the order of the items.
pub fn parallel_map<T: Sync, R: Send>(items: &[T], jobs: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    thread::scope(|s| {
        for _ in 0..jobs.clamp(1, items.len().max(1)) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(i) else { break };
                    let result = f(item);
                    *results[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                }
            });
        }
    });
    results.into_iter().filter_map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner())).collect()
}

/// Size for humans: `512 B`, `12.0 KiB`, `1.4 MiB`...
pub fn size_label(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    for unit in ["KiB", "MiB", "GiB"] {
        if value < 1024.0 || unit == "GiB" {
            return format!("{value:.1} {unit}");
        }
        value /= 1024.0;
    }
    unreachable!()
}

/// Total of the known sizes, and how many sizes are unknown.
pub fn total_size(items: &[Item]) -> (u64, usize) {
    (items.iter().filter_map(|i| i.size).sum(), items.iter().filter(|i| i.size.is_none()).count())
}

/// `1.4 MiB`, or `1.4 MiB + 2 of unknown size`.
pub fn describe_total(known: u64, unknown: usize) -> String {
    match unknown {
        0 => size_label(known),
        n => format!("{} + {n} of unknown size", size_label(known)),
    }
}

pub fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") }
}

/// How the output looks: a progress line and symbols on a terminal only.
struct Display {
    tty: bool,
    unicode: bool,
    color: bool,
    columns: usize,
}

impl Display {
    fn detect() -> Display {
        let tty = std::io::stdout().is_terminal();
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let unicode = locale.contains("utf-8") || locale.contains("utf8");
        let color = tty && std::env::var_os("NO_COLOR").is_none() && std::env::var("TERM").as_deref() != Ok("dumb");
        Display { tty, unicode, color, columns: terminal_columns().unwrap_or(80) }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
    }

    /// Mark in front of a finished download, on a terminal.
    fn mark(&self, ok: bool) -> String {
        match (self.tty && self.unicode, ok) {
            (false, _) => String::new(),
            (true, true) => self.paint("32", "✓") + " ",
            (true, false) => self.paint("31", "✗") + " ",
        }
    }

    /// The progress line: a bar and a percentage when the total is known.
    fn progress(&self, received: u64, total: Option<u64>, elapsed: Duration) -> String {
        let secs = elapsed.as_secs_f64();
        let speed =
            if secs >= 0.5 { format!("{}/s", size_label((received as f64 / secs) as u64)) } else { String::new() };
        let Some(total) = total.filter(|t| *t > 0) else {
            return format!("  {}   {speed}", size_label(received));
        };
        let ratio = (received as f64 / total as f64).min(1.0);
        let total = size_label(total);
        let received = size_label(received);
        let text = format!("{:>4.0}%   {received:>w$} / {total}   {speed}", ratio * 100.0, w = total.len());
        let width = self.columns.saturating_sub(text.chars().count() + 6).min(40);
        if width < 10 {
            return format!("  {text}");
        }
        let filled = (ratio * width as f64).round() as usize;
        let (on, off) = if self.unicode { ("━", "─") } else { ("#", "-") };
        let bar = on.repeat(filled) + &self.paint("2", &off.repeat(width - filled));
        format!("  {bar} {text}")
    }

    fn draw(&self, line: &str) {
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "\r\x1b[K{line}");
        let _ = out.flush();
    }

    fn clear(&self) {
        if self.tty {
            self.draw("");
        }
    }
}

/// Width of the terminal on stdout.
fn terminal_columns() -> Option<usize> {
    // SAFETY: TIOCGWINSZ fills a winsize structure
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_col > 0).then_some(size.ws_col as usize)
}

/// Shared counters of a download session.
struct Counters {
    received: AtomicU64,
    /// Total of the known sizes, completed as transfers announce theirs.
    known: AtomicU64,
    unknown: AtomicUsize,
}

/// Downloads an archive to `<dest>.new`, checks it, then moves it to
/// `dest`. A failed transfer is tried a second time. Returns its size.
fn fetch_one(item: &Item, counters: &Counters) -> Result<u64> {
    let new = with_suffix(&item.dest, ".new");
    let mut last = None;
    for _ in 0..2 {
        let mut got = 0;
        let announce = |size: Option<u64>| {
            if let (None, Some(size)) = (item.size, size) {
                counters.known.fetch_add(size, Ordering::SeqCst);
                counters.unknown.fetch_sub(1, Ordering::SeqCst);
            }
        };
        let result = fetch::download_with(&item.url, &new, announce, |n| {
            got += n;
            counters.received.fetch_add(n, Ordering::SeqCst);
            Ok(())
        });
        if let Err(err) = result {
            counters.received.fetch_sub(got, Ordering::SeqCst);
            last = Some(err);
            continue;
        }
        let actual = sha256_file(&new)?;
        if actual != item.checksum {
            let _ = fs::remove_file(&new);
            counters.received.fetch_sub(got, Ordering::SeqCst);
            let file = item.dest.file_name().unwrap_or_default().to_string_lossy();
            bail!("checksum mismatch for {file}\n  expected: {}\n  actual:   {actual}", item.checksum);
        }
        fs::rename(&new, &item.dest)?;
        return Ok(got);
    }
    Err(last.unwrap_or_else(|| anyhow!("download failed")))
}

/// Downloads archives into the cache, `jobs` at a time. Every download is
/// attempted; the function fails if any of them did.
pub fn download_all(items: &[Item], jobs: usize) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let display = Display::detect();
    let (known, unknown) = total_size(items);
    println!("Downloading {} ({}):", plural(items.len(), "package"), describe_total(known, unknown));
    let width = items.iter().map(|i| i.label.chars().count()).max().unwrap_or(0).min(40);
    let counters =
        Counters { received: AtomicU64::new(0), known: AtomicU64::new(known), unknown: AtomicUsize::new(unknown) };
    let next = AtomicUsize::new(0);
    let start = Instant::now();
    let mut failures: Vec<anyhow::Error> = Vec::new();

    thread::scope(|s| {
        let (tx, rx) = mpsc::channel::<(usize, Result<u64>)>();
        for _ in 0..jobs.clamp(1, items.len()) {
            let tx = tx.clone();
            let (next, counters) = (&next, &counters);
            s.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(i) else { break };
                    if tx.send((i, fetch_one(item, counters))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok((i, result)) => {
                    display.clear();
                    let label = &items[i].label;
                    match result {
                        Ok(size) => {
                            println!("  {}{label:<width$}  {:>10}", display.mark(true), size_label(size));
                        }
                        Err(err) => {
                            println!("  {}{label:<width$}  {:>10}", display.mark(false), "failed");
                            failures.push(err);
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if display.tty {
                let total =
                    (counters.unknown.load(Ordering::SeqCst) == 0).then(|| counters.known.load(Ordering::SeqCst));
                display.draw(&display.progress(counters.received.load(Ordering::SeqCst), total, start.elapsed()));
            }
        }
        display.clear();
    });

    if failures.is_empty() {
        let received = counters.received.load(Ordering::SeqCst);
        let secs = start.elapsed().as_secs_f64();
        let speed =
            if secs >= 0.5 { format!(", {}/s", size_label((received as f64 / secs) as u64)) } else { String::new() };
        println!("  {} downloaded in {secs:.1} s{speed}", size_label(received));
        return Ok(());
    }
    for err in &failures {
        eprintln!("Error: {err:#}");
    }
    bail!("{} of {} failed; nothing was changed.", plural(failures.len(), "download"), items.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(1023), "1023 B");
        assert_eq!(size_label(12 * 1024), "12.0 KiB");
        assert_eq!(size_label(1468006), "1.4 MiB");
        assert_eq!(size_label(5 << 30), "5.0 GiB");
        assert_eq!(describe_total(2048, 0), "2.0 KiB");
        assert_eq!(describe_total(2048, 2), "2.0 KiB + 2 of unknown size");
    }

    #[test]
    fn progress_line() {
        let display = Display { tty: true, unicode: false, color: false, columns: 80 };
        let line = display.progress(512 * 1024, Some(1024 * 1024), Duration::from_secs(1));
        // The bar takes what the terminal width leaves, at most 40 columns
        let expected = format!("  {}{}   50%   512.0 KiB / 1.0 MiB   512.0 KiB/s", "#".repeat(17), "-".repeat(16));
        assert_eq!(line, expected);
        let wide = Display { columns: 200, ..display };
        assert!(wide.progress(0, Some(10), Duration::ZERO).starts_with(&format!("  {} ", "-".repeat(40))));
        let narrow = Display { columns: 30, ..wide };
        assert_eq!(narrow.progress(0, Some(10), Duration::ZERO), "     0%    0 B / 10 B   ");
        assert_eq!(narrow.progress(2048, None, Duration::ZERO), "  2.0 KiB   ");
    }

    #[test]
    fn parallel_results_keep_their_order() {
        let items: Vec<u64> = (0..50).collect();
        assert_eq!(parallel_map(&items, 8, |n| n * 2), items.iter().map(|n| n * 2).collect::<Vec<_>>());
        assert!(parallel_map(&Vec::<u64>::new(), 4, |n| *n).is_empty());
    }
}
