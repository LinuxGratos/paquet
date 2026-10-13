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

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
        ureq::Agent::config_builder()
            .tls_config(tls)
            .user_agent(concat!("paquet/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(30)))
            .build()
            .into()
    })
}

/// Opens a URL for reading.
pub fn open(url: &str) -> Result<Box<dyn Read>> {
    if let Some(path) = file_url_path(url) {
        let file = fs::File::open(&path).with_context(|| format!("unable to open {}", path.display()))?;
        return Ok(Box::new(file));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("unsupported URL {url} (expected http://, https:// or file://)");
    }
    let response = agent().get(url).call().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Box::new(response.into_body().into_reader()))
}

/// Downloads a URL into a file. The data first goes to `<dest>.part`, which
/// only replaces `dest` once complete.
pub fn download(url: &str, dest: &Path) -> Result<()> {
    let part = with_suffix(dest, ".part");
    let result = (|| -> Result<()> {
        let mut reader = open(url)?;
        let mut file = fs::File::create(&part).with_context(|| format!("unable to create {}", part.display()))?;
        copy_interruptible(&mut reader, &mut file)?;
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
    copy_interruptible(&mut reader, &mut data)?;
    Ok(data)
}

fn copy_interruptible(reader: &mut dyn Read, writer: &mut dyn Write) -> Result<u64> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0;
    loop {
        signals::check()?;
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        writer.write_all(&buf[..n])?;
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
