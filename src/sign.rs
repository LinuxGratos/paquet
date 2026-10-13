//! Ed25519 signatures of the repository index.
//!
//! Keys are PEM files in the format of OpenSSL (`openssl genpkey -algorithm
//! ed25519`): PKCS#8 for the private key, SubjectPublicKeyInfo for the public
//! one. Signatures are the raw 64 bytes made by `openssl pkeyutl -sign
//! -rawin`, so keys and signatures can be exchanged with the shell tools.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use ed25519_dalek::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// Creates a key pair: `<name>.key` (private, mode 600) and `<name>.pub`.
pub fn generate_key(name: &str) -> Result<(PathBuf, PathBuf)> {
    let key_path = PathBuf::from(format!("{name}.key"));
    let pub_path = PathBuf::from(format!("{name}.pub"));
    if key_path.exists() || pub_path.exists() {
        bail!("{} or {} already exists.", key_path.display(), pub_path.display());
    }
    let key = SigningKey::generate(&mut rand_core::OsRng);
    // PKCS#8 v1 without the public key, as written by openssl genpkey
    let private_pem = ed25519_dalek::pkcs8::KeypairBytes { secret_key: key.to_bytes(), public_key: None }
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| anyhow!("unable to encode the private key: {e}"))?;
    let public_pem = key
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| anyhow!("unable to encode the public key: {e}"))?;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&key_path)
        .with_context(|| format!("unable to create {}", key_path.display()))?;
    file.write_all(private_pem.as_bytes())?;
    fs::write(&pub_path, public_pem.as_bytes()).with_context(|| format!("unable to create {}", pub_path.display()))?;
    fs::set_permissions(&pub_path, fs::Permissions::from_mode(0o644))?;
    Ok((key_path, pub_path))
}

pub fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let pem = fs::read_to_string(path)
        .with_context(|| format!("signing key {} not found or not readable.", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem).map_err(|e| anyhow!("{} is not an Ed25519 private key: {e}", path.display()))
}

pub fn load_verifying_key(path: &Path) -> Result<VerifyingKey> {
    let pem = fs::read_to_string(path)?;
    VerifyingKey::from_public_key_pem(&pem).map_err(|e| anyhow!("{} is not an Ed25519 public key: {e}", path.display()))
}

/// Detached signature of some data (64 bytes).
pub fn sign(key: &SigningKey, data: &[u8]) -> Vec<u8> {
    key.sign(data).to_bytes().to_vec()
}

pub fn verify(key: &VerifyingKey, data: &[u8], signature: &[u8]) -> bool {
    match Signature::from_slice(signature) {
        Ok(sig) => key.verify(data, &sig).is_ok(),
        Err(_) => false,
    }
}

/// Checks a signature against the trusted public keys (`<dir>/*.pub`): it is
/// accepted as soon as one of them verifies it. Returns the matching key.
pub fn verify_with_keys(keys_dir: &Path, data: &[u8], signature: &[u8]) -> Result<PathBuf> {
    let keys = crate::util::files_with_suffix(keys_dir, ".pub");
    if keys.is_empty() {
        bail!("no trusted public key found in {}.", keys_dir.display());
    }
    for path in keys {
        match load_verifying_key(&path) {
            Ok(key) if verify(&key, data, signature) => return Ok(path),
            Ok(_) => {}
            Err(err) => eprintln!("Warning: {err:#}"),
        }
    }
    bail!("invalid signature (no key in {} matches).", keys_dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let dir = tempfile::tempdir().unwrap();
        let name = dir.path().join("repo");
        let (key_path, pub_path) = generate_key(name.to_str().unwrap()).unwrap();
        assert!(generate_key(name.to_str().unwrap()).is_err());
        let key = load_signing_key(&key_path).unwrap();
        let sig = sign(&key, b"index");
        assert_eq!(sig.len(), 64);
        let public = load_verifying_key(&pub_path).unwrap();
        assert!(verify(&public, b"index", &sig));
        assert!(!verify(&public, b"index2", &sig));
        let keys = dir.path().join("keys");
        fs::create_dir(&keys).unwrap();
        assert!(verify_with_keys(&keys, b"index", &sig).is_err());
        fs::copy(&pub_path, keys.join("repo.pub")).unwrap();
        assert!(verify_with_keys(&keys, b"index", &sig).is_ok());
        assert!(verify_with_keys(&keys, b"other", &sig).is_err());
    }
}
