use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

pub struct SecretKey {
    key: [u8; 32],
    hmac: hmac::Key,
}

impl SecretKey {
    pub fn load(data_dir: &Path) -> Result<Self> {
        if let Ok(encoded) = std::env::var("AWESOMETV_SECRET_KEY")
            && !encoded.trim().is_empty()
        {
            let decoded = STANDARD
                .decode(encoded.trim())
                .or_else(|_| URL_SAFE_NO_PAD.decode(encoded.trim()))
                .context("AWESOMETV_SECRET_KEY must be base64")?;
            if decoded.len() != 32 {
                bail!("AWESOMETV_SECRET_KEY must decode to 32 bytes");
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&decoded);
            return Ok(Self::from_bytes(key));
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
        let hmac = hmac::Key::new(hmac::HMAC_SHA256, &key);
        Self { key, hmac }
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<String> {
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

    pub fn decrypt(&self, encoded: &str) -> Result<Vec<u8>> {
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

    pub fn sign(&self, data: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(hmac::sign(&self.hmac, data).as_ref())
    }

    pub fn verify(&self, data: &[u8], signature: &str) -> bool {
        let Ok(decoded) = URL_SAFE_NO_PAD.decode(signature) else {
            return false;
        };
        hmac::verify(&self.hmac, data, &decoded).is_ok()
    }
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
}
