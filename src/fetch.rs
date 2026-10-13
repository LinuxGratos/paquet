//! Downloads over HTTP(S), or copies from `file://` URLs.
//!
//! HTTP errors fail the download (like `curl -f`), redirects are followed,
//! certificates are checked against the system's store and the usual proxy
//! variables (`https_proxy`, `ALL_PROXY`...) are honored.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::signals;
use crate::util::with_suffix;

fn new_agent(keep_alive: bool) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    let mut config = ureq::Agent::config_builder()
        .tls_config(tls)
        .user_agent(concat!("paquet/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)));
    if !keep_alive {
        config = config.max_idle_connections(0).max_idle_connections_per_host(0);
    }
    config.build().into()
}

/// Sends a GET or HEAD request. Connections are kept for the next requests;
/// but an HTTP/1.0 server (`python3 -m http.server`...) may close one
/// without saying so: a request failing on the network is sent once more,
/// on a new connection.
fn call(url: &str, head: bool) -> Result<ureq::http::Response<ureq::Body>> {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    static SINGLE_USE: OnceLock<ureq::Agent> = OnceLock::new();
    let send = |agent: &ureq::Agent| if head { agent.head(url).call() } else { agent.get(url).call() };
    let result = match send(AGENT.get_or_init(|| new_agent(true))) {
        Err(ureq::Error::Io(_)) => send(SINGLE_USE.get_or_init(|| new_agent(false))),
        other => other,
    };
    result.map_err(|e| anyhow::anyhow!("{e}"))
}

fn check_scheme(url: &str) -> Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("unsupported URL {url} (expected http://, https:// or file://)");
    }
    Ok(())
}

/// Opens a URL for reading. Also returns its size, when known.
pub fn open_sized(url: &str) -> Result<(Box<dyn Read>, Option<u64>)> {
    if let Some(path) = file_url_path(url) {
        let file = fs::File::open(&path).with_context(|| format!("unable to open {}", path.display()))?;
        let size = file.metadata().ok().map(|m| m.len());
        return Ok((Box::new(file), size));
    }
    check_scheme(url)?;
    let response = call(url, false)?;
    let size = content_length(response.headers());
    Ok((Box::new(response.into_body().into_reader()), size))
}

/// Opens a URL for reading.
pub fn open(url: &str) -> Result<Box<dyn Read>> {
    Ok(open_sized(url)?.0)
}

fn content_length(headers: &ureq::http::HeaderMap) -> Option<u64> {
    headers.get("content-length")?.to_str().ok()?.trim().parse().ok()
}

/// Size of the file behind a URL, without downloading it (an HTTP `HEAD`
/// request). None when the server does not tell.
pub fn size(url: &str) -> Result<Option<u64>> {
    if let Some(path) = file_url_path(url) {
        let meta = fs::metadata(&path).with_context(|| format!("unable to open {}", path.display()))?;
        return Ok(Some(meta.len()));
    }
    check_scheme(url)?;
    let response = call(url, true)?;
    Ok(content_length(response.headers()))
}

/// Downloads a URL into a file. The data first goes to `<dest>.part`, which
/// only replaces `dest` once complete.
pub fn download(url: &str, dest: &Path) -> Result<()> {
    download_with(url, dest, |_| {}, |_| Ok(signals::check()?))
}

/// Downloads a URL into a file, like [`download`], reporting its size (when
/// known) to `on_size` once the transfer starts, then the size of each
/// block received to `on_data` (0 before the first one), which can stop the
/// transfer by failing.
pub fn download_with(
    url: &str,
    dest: &Path,
    on_size: impl FnOnce(Option<u64>),
    mut on_data: impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    let part = with_suffix(dest, ".part");
    let result = (|| -> Result<()> {
        let (mut reader, size) = open_sized(url)?;
        on_size(size);
        let mut file = fs::File::create(&part).with_context(|| format!("unable to create {}", part.display()))?;
        copy_with(&mut reader, &mut file, &mut on_data)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&part);
        return Err(err).with_context(|| format!("failed to download {url}"));
    }
    fs::rename(&part, dest).with_context(|| format!("unable to write {}", dest.display()))
}

/// Downloads a URL into memory. Errors do not repeat the URL.
pub fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut reader = open(url)?;
    copy_with(&mut reader, &mut data, &mut |_| Ok(signals::check()?))?;
    Ok(data)
}

fn copy_with(reader: &mut dyn Read, writer: &mut dyn Write, on_data: &mut dyn FnMut(u64) -> Result<()>) -> Result<u64> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0;
    on_data(0)?;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        writer.write_all(&buf[..n])?;
        on_data(n as u64)?;
        total += n as u64;
    }
}

/// Local path of a `file://` URL (`file:///srv/repo` or `file://localhost/srv`).
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    Some(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(s: &str) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    std::ffi::OsString::from_vec(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls() {
        assert_eq!(file_url_path("file:///srv/pq"), Some(PathBuf::from("/srv/pq")));
        assert_eq!(file_url_path("file://localhost/srv/a%20b"), Some(PathBuf::from("/srv/a b")));
        assert_eq!(file_url_path("https://x"), None);
    }

    #[test]
    fn downloads_files() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::write(&src, b"hello").unwrap();
        let dest = dir.path().join("dest");
        download(&format!("file://{}", src.display()), &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello");
        assert!(download(&format!("file://{}/missing", dir.path().display()), &dest).is_err());
        assert!(!with_suffix(&dest, ".part").exists());
    }
}
