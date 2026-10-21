use super::Credentials;
use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};
use zeroize::Zeroizing;

#[derive(Clone, Serialize, Deserialize)]
pub struct SealedCredentials {
    pub nonce: String,
    pub ciphertext: String,
}

pub struct Vault {
    key: Zeroizing<Vec<u8>>,
}
impl Vault {
    pub fn open(dir: &Path, has_credentials: bool) -> Result<Self> {
        let path = dir.join("credentials.key");
        let key = match fs::read(&path) {
            Ok(bytes) => {
                ensure!(
                    fs::symlink_metadata(&path)?.file_type().is_file(),
                    "credentials.key must be a regular file"
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    ensure!(
                        fs::metadata(&path)?.permissions().mode() & 0o077 == 0,
                        "credentials.key must have mode 0600"
                    );
                }
                bytes
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    !has_credentials,
                    "credentials.key is missing; restore the original key before starting farmd"
                );
                let mut bytes = Zeroizing::new(vec![0u8; 32]);
                getrandom::getrandom(&mut bytes)
                    .map_err(|_| anyhow!("secure random source unavailable"))?;
                let mut options = OpenOptions::new();
                options.create_new(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(&path)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                fs::File::open(dir)?.sync_all()?;
                bytes.to_vec()
            }
            Err(e) => return Err(e).context("read credential encryption key"),
        };
        let key = Zeroizing::new(key);
        ensure!(
            key.len() == 32,
            "credentials.key must contain exactly 32 raw bytes"
        );
        Ok(Self { key })
    }
    pub fn seal(&self, credentials: &Credentials, aad: &str) -> Result<SealedCredentials> {
        let cipher = XChaCha20Poly1305::new_from_slice(&self.key).unwrap();
        let plain = Zeroizing::new(serde_json::to_vec(credentials)?);
        let mut nonce = [0u8; 24];
        getrandom::getrandom(&mut nonce)
            .map_err(|_| anyhow!("secure random source unavailable"))?;
        let ciphertext = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plain,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| anyhow!("credential encryption failed"))?;
        Ok(SealedCredentials {
            nonce: STANDARD.encode(nonce),
            ciphertext: STANDARD.encode(ciphertext),
        })
    }
    pub fn unseal(&self, sealed: &SealedCredentials, aad: &str) -> Result<Credentials> {
        let nonce = STANDARD
            .decode(&sealed.nonce)
            .context("invalid credential nonce")?;
        ensure!(nonce.len() == 24, "invalid credential nonce");
        let cipher = XChaCha20Poly1305::new_from_slice(&self.key).unwrap();
        let ciphertext = STANDARD
            .decode(&sealed.ciphertext)
            .context("invalid encrypted credential")?;
        let nonce: [u8; 24] = nonce
            .try_into()
            .map_err(|_| anyhow!("invalid credential nonce"))?;
        let plain = Zeroizing::new(
            cipher
                .decrypt(
                    &XNonce::from(nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: aad.as_bytes(),
                    },
                )
                .map_err(|_| {
                    anyhow!("credential decryption failed; check credentials.key and state backup")
                })?,
        );
        serde_json::from_slice(&plain).context("invalid encrypted credential contents")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secrets_survive_restart_but_reject_tampering_wrong_identity_and_wrong_key() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path(), false).unwrap();
        let credential = Credentials::Password {
            password: "unique-test-password".into(),
        };
        let mut encrypted = vault.seal(&credential, "host-a-identity").unwrap();
        assert!(
            !serde_json::to_string(&encrypted)
                .unwrap()
                .contains("unique-test-password")
        );
        assert_ne!(
            encrypted.nonce,
            vault.seal(&credential, "host-a-identity").unwrap().nonce
        );
        drop(vault);
        let vault = Vault::open(dir.path(), true).unwrap();
        assert!(matches!(
            vault.unseal(&encrypted, "host-a-identity").unwrap(),
            Credentials::Password { .. }
        ));
        assert!(vault.unseal(&encrypted, "host-b-identity").is_err());
        let other = tempfile::tempdir().unwrap();
        assert!(
            Vault::open(other.path(), false)
                .unwrap()
                .unseal(&encrypted, "host-a-identity")
                .is_err()
        );
        encrypted.ciphertext.replace_range(0..1, "!");
        assert!(vault.unseal(&encrypted, "host-a-identity").is_err());
        std::fs::remove_file(dir.path().join("credentials.key")).unwrap();
        assert!(Vault::open(dir.path(), true).is_err());
        assert!(!dir.path().join("credentials.key").exists());
    }
}
