use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

struct KeyMaterial {
    key: [u8; 32],
    hmac: hmac::Key,
}

impl KeyMaterial {
    fn new(key: [u8; 32]) -> Self {
        let hmac = hmac::Key::new(hmac::HMAC_SHA256, &key);
        Self { key, hmac }
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<String> {
        let mut nonce = [0u8; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("generate nonce"))?;
        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &self.key)
                .map_err(|_| anyhow::anyhow!("build AES key"))?,
        );
        let mut data = plaintext.to_vec();
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut data)
            .map_err(|_| anyhow::anyhow!("encrypt secret"))?;
        let mut encoded = nonce.to_vec();
        encoded.extend_from_slice(&data);
        Ok(STANDARD.encode(encoded))
    }

    fn decrypt(&self, encoded: &str) -> Result<Vec<u8>> {
        let bytes = STANDARD
            .decode(encoded)
            .context("decode encrypted secret")?;
        if bytes.len() < 12 {
            bail!("encrypted secret is too short");
        }
        let (nonce, ciphertext) = bytes.split_at(12);
        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &self.key)
                .map_err(|_| anyhow::anyhow!("build AES key"))?,
        );
        let mut data = ciphertext.to_vec();
        let plaintext = key
            .open_in_place(
                Nonce::try_assume_unique_for_key(nonce)
                    .map_err(|_| anyhow::anyhow!("invalid nonce"))?,
                Aad::empty(),
                &mut data,
            )
            .map_err(|_| anyhow::anyhow!("decrypt secret"))?;
        Ok(plaintext.to_vec())
    }
}

pub struct SecretKey {
    primary: KeyMaterial,
    previous: Vec<KeyMaterial>,
}

impl SecretKey {
    pub fn load(data_dir: &Path) -> Result<Self> {
        if let Ok(encoded) = std::env::var("AWESOMETV_SECRET_KEY")
            && !encoded.trim().is_empty()
        {
            let key = decode_key(&encoded, "AWESOMETV_SECRET_KEY")?;
            let previous = match std::env::var("AWESOMETV_SECRET_KEY_PREVIOUS") {
                Ok(value) => value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| decode_key(value, "AWESOMETV_SECRET_KEY_PREVIOUS"))
                    .collect::<Result<Vec<_>>>()?,
                Err(_) => Vec::new(),
            };
            return Ok(Self::with_previous(key, previous));
        }
        let dir = data_dir.join("secrets");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("cloud.key");
        let key = if path.is_file() {
            let bytes = std::fs::read(&path)?;
            if bytes.len() != 32 {
                bail!("cloud.key must contain 32 bytes");
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            key
        } else {
            let mut key = [0u8; 32];
            SystemRandom::new()
                .fill(&mut key)
                .map_err(|_| anyhow::anyhow!("generate secret key"))?;
            std::fs::write(&path, key)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            key
        };
        Ok(Self::from_bytes(key))
    }

    pub fn from_bytes(key: [u8; 32]) -> Self {
        Self::with_previous(key, Vec::new())
    }

    pub fn with_previous(primary: [u8; 32], previous: Vec<[u8; 32]>) -> Self {
        Self {
            primary: KeyMaterial::new(primary),
            previous: previous.into_iter().map(KeyMaterial::new).collect(),
        }
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<String> {
        self.primary.encrypt(plaintext)
    }

    pub fn decrypt(&self, encoded: &str) -> Result<Vec<u8>> {
        match self.primary.decrypt(encoded) {
            Ok(plaintext) => Ok(plaintext),
            Err(primary_error) => {
                for key in &self.previous {
                    if let Ok(plaintext) = key.decrypt(encoded) {
                        return Ok(plaintext);
                    }
                }
                Err(primary_error)
            }
        }
    }

    /// Re-encrypt a value that was written with a previous key. Returns `None`
    /// when the value already uses the primary key.
    pub fn rotate(&self, encoded: &str) -> Result<Option<String>> {
        if self.primary.decrypt(encoded).is_ok() {
            return Ok(None);
        }
        for key in &self.previous {
            if let Ok(plaintext) = key.decrypt(encoded) {
                return Ok(Some(self.primary.encrypt(&plaintext)?));
            }
        }
        bail!("encrypted secret does not match the primary or previous key")
    }

    pub fn sign(&self, data: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(hmac::sign(&self.primary.hmac, data).as_ref())
    }

    pub fn verify(&self, data: &[u8], signature: &str) -> bool {
        let Ok(decoded) = URL_SAFE_NO_PAD.decode(signature) else {
            return false;
        };
        hmac::verify(&self.primary.hmac, data, &decoded).is_ok()
    }
}

fn decode_key(encoded: &str, label: &str) -> Result<[u8; 32]> {
    let decoded = STANDARD
        .decode(encoded.trim())
        .or_else(|_| URL_SAFE_NO_PAD.decode(encoded.trim()))
        .with_context(|| format!("{label} must be base64"))?;
    if decoded.len() != 32 {
        bail!("{label} must decode to 32 bytes");
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&decoded);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypts_and_decrypts() {
        let key = SecretKey::from_bytes([7u8; 32]);
        let encrypted = key.encrypt(b"hello cloud").unwrap();
        assert_ne!(encrypted, "hello cloud");
        assert_eq!(key.decrypt(&encrypted).unwrap(), b"hello cloud");
    }

    #[test]
    fn detects_tampering() {
        let key = SecretKey::from_bytes([9u8; 32]);
        let encrypted = key.encrypt(b"secret").unwrap();
        let mut bytes = STANDARD.decode(&encrypted).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(key.decrypt(&STANDARD.encode(bytes)).is_err());
    }

    #[test]
    fn signs_and_verifies() {
        let key = SecretKey::from_bytes([3u8; 32]);
        let signature = key.sign(b"source/remote");
        assert!(key.verify(b"source/remote", &signature));
        assert!(!key.verify(b"source/other", &signature));
    }

    #[test]
    fn previous_key_allows_rotation() {
        let old = SecretKey::from_bytes([1u8; 32]);
        let encrypted = old.encrypt(b"cloud secret").unwrap();
        let rotating = SecretKey::with_previous([2u8; 32], vec![[1u8; 32]]);
        assert_eq!(rotating.decrypt(&encrypted).unwrap(), b"cloud secret");
        let rotated = rotating
            .rotate(&encrypted)
            .unwrap()
            .expect("needs rotation");
        assert!(rotating.rotate(&rotated).unwrap().is_none());
        let new_only = SecretKey::from_bytes([2u8; 32]);
        assert_eq!(new_only.decrypt(&rotated).unwrap(), b"cloud secret");
        assert!(new_only.decrypt(&encrypted).is_err());
    }
}
