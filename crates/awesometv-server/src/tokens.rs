use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};

const TOKEN_PREFIX: &str = "cl_";

pub struct GeneratedToken {
    pub id: String,
    pub token: String,
    pub hash: String,
}

/// Tokens are `cl_<id>_<secret>`. The id is hex so it cannot contain the
/// separator; the secret is base64url and may contain `_` or `-`.
pub fn generate() -> Result<GeneratedToken> {
    let mut id_bytes = [0u8; 8];
    let mut secret_bytes = [0u8; 32];
    let random = SystemRandom::new();
    random
        .fill(&mut id_bytes)
        .map_err(|_| anyhow::anyhow!("generate token id"))?;
    random
        .fill(&mut secret_bytes)
        .map_err(|_| anyhow::anyhow!("generate token secret"))?;
    let id = id_bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let secret = URL_SAFE_NO_PAD.encode(secret_bytes);
    Ok(GeneratedToken {
        token: format!("{TOKEN_PREFIX}{id}_{secret}"),
        hash: hash_secret(&secret),
        id,
    })
}

pub fn parse(token: &str) -> Option<(&str, &str)> {
    let rest = token.trim().strip_prefix(TOKEN_PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    if id.len() != 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) || secret.is_empty() {
        return None;
    }
    Some((id, secret))
}

pub fn hash_secret(secret: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest(&SHA256, secret.as_bytes()).as_ref())
}

pub fn verify_secret(secret: &str, stored_hash: &str) -> bool {
    let Ok(stored) = URL_SAFE_NO_PAD.decode(stored_hash) else {
        return false;
    };
    let candidate = digest(&SHA256, secret.as_bytes());
    constant_time_eq(candidate.as_ref(), &stored)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_parseable_tokens() {
        let generated = generate().unwrap();
        let (id, secret) = parse(&generated.token).unwrap();
        assert_eq!(id, generated.id);
        assert!(verify_secret(secret, &generated.hash));
    }

    #[test]
    fn rejects_wrong_and_malformed_tokens() {
        let generated = generate().unwrap();
        let (_, secret) = parse(&generated.token).unwrap();
        assert!(!verify_secret(secret, &hash_secret("different")));
        assert!(parse("not-a-token").is_none());
        assert!(parse("cl_short_secret").is_none());
        assert!(parse("cl_0000000000000000_").is_none());
    }
}
