use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use crate::cloud::google::validate_https_endpoint;

const DISCOVERY_TTL: Duration = Duration::from_secs(3_600);

#[derive(Clone, Debug)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
    pub scopes: String,
    pub role_claim: String,
    pub admin_role: String,
    pub operator_role: String,
    pub viewer_role: String,
    pub default_role: String,
}

impl OidcConfig {
    pub fn from_env() -> Result<Option<Self>> {
        let issuer = match std::env::var("AWESOMETV_OIDC_ISSUER") {
            Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => return Ok(None),
        };
        let client_id = std::env::var("AWESOMETV_OIDC_CLIENT_ID").unwrap_or_default();
        let redirect_url = std::env::var("AWESOMETV_OIDC_REDIRECT_URL").unwrap_or_default();
        if client_id.trim().is_empty() || redirect_url.trim().is_empty() {
            bail!(
                "AWESOMETV_OIDC_CLIENT_ID and AWESOMETV_OIDC_REDIRECT_URL are required when OIDC is enabled"
            );
        }
        let issuer = validate_oidc_endpoint(&issuer, "AWESOMETV_OIDC_ISSUER")?;
        let redirect_url = validate_redirect_url(&redirect_url, "AWESOMETV_OIDC_REDIRECT_URL")?;
        Ok(Some(Self {
            issuer,
            client_id: client_id.trim().to_string(),
            client_secret: std::env::var("AWESOMETV_OIDC_CLIENT_SECRET").unwrap_or_default(),
            redirect_url,
            scopes: std::env::var("AWESOMETV_OIDC_SCOPES")
                .unwrap_or_else(|_| "openid profile email".into()),
            role_claim: std::env::var("AWESOMETV_OIDC_ROLE_CLAIM")
                .unwrap_or_else(|_| "roles".into()),
            admin_role: std::env::var("AWESOMETV_OIDC_ADMIN_ROLE")
                .unwrap_or_else(|_| "admin".into()),
            operator_role: std::env::var("AWESOMETV_OIDC_OPERATOR_ROLE")
                .unwrap_or_else(|_| "operator".into()),
            viewer_role: std::env::var("AWESOMETV_OIDC_VIEWER_ROLE")
                .unwrap_or_else(|_| "viewer".into()),
            default_role: std::env::var("AWESOMETV_OIDC_DEFAULT_ROLE")
                .unwrap_or_else(|_| "viewer".into()),
        }))
    }
}

fn validate_oidc_endpoint(value: &str, label: &str) -> Result<String> {
    let parsed = Url::parse(value.trim()).with_context(|| format!("{label} must be a URL"))?;
    let local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if parsed.scheme() == "http" && local {
        return Ok(value.trim().trim_end_matches('/').to_string());
    }
    validate_https_endpoint(value, label)
}

fn validate_redirect_url(value: &str, label: &str) -> Result<String> {
    let parsed = Url::parse(value.trim()).with_context(|| format!("{label} must be a URL"))?;
    let local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && local) {
        bail!("{label} must use https (http is allowed only for localhost)");
    }
    Ok(value.trim().to_string())
}

#[derive(Clone, Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    userinfo_endpoint: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PendingLogin {
    pub verifier: String,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub username: String,
    pub role: String,
}

pub struct OidcClient {
    config: OidcConfig,
    agent: ureq::Agent,
    discovery: Mutex<Option<(Discovery, Instant)>>,
}

impl OidcClient {
    pub fn from_env() -> Result<Option<Arc<Self>>> {
        let Some(config) = OidcConfig::from_env()? else {
            return Ok(None);
        };
        Ok(Some(Arc::new(Self::new(config))))
    }

    pub fn new(config: OidcConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(20))
            .redirects(0)
            .build();
        Self {
            config,
            agent,
            discovery: Mutex::new(None),
        }
    }

    pub async fn start(&self) -> Result<(String, String, PendingLogin)> {
        let discovery = self.discovery().await?;
        let state = random_value(32)?;
        let verifier = random_value(32)?;
        let nonce = random_value(24)?;
        let challenge = URL_SAFE_NO_PAD.encode(digest(&SHA256, verifier.as_bytes()).as_ref());
        let mut url = Url::parse(&discovery.authorization_endpoint)
            .context("parse authorization endpoint")?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", &self.config.redirect_url)
            .append_pair("scope", &self.config.scopes)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok((
            url.to_string(),
            state,
            PendingLogin {
                verifier,
                created_at: crate::db::now_ms(),
            },
        ))
    }

    pub async fn finish(&self, code: &str, pending: &PendingLogin) -> Result<Profile> {
        let discovery = self.discovery().await?;
        let token_url = validate_oidc_endpoint(&discovery.token_endpoint, "OIDC token_endpoint")?;
        let userinfo_url = discovery
            .userinfo_endpoint
            .as_deref()
            .context("OIDC provider has no userinfo_endpoint")?;
        let userinfo_url = validate_oidc_endpoint(userinfo_url, "OIDC userinfo_endpoint")?;
        let client_id = self.config.client_id.clone();
        let client_secret = self.config.client_secret.clone();
        let redirect_url = self.config.redirect_url.clone();
        let verifier = pending.verifier.clone();
        let code = code.to_string();
        let agent = self.agent.clone();
        let token = tokio::task::spawn_blocking(move || {
            let mut form: Vec<(&str, &str)> = vec![
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", redirect_url.as_str()),
                ("client_id", client_id.as_str()),
                ("code_verifier", verifier.as_str()),
            ];
            if !client_secret.is_empty() {
                form.push(("client_secret", client_secret.as_str()));
            }
            let response = agent
                .post(&token_url)
                .send_form(&form)
                .map_err(|error| anyhow::anyhow!("OIDC token exchange failed: {error}"))?;
            let value: Value = response.into_json().context("parse OIDC token response")?;
            value
                .get("access_token")
                .and_then(Value::as_str)
                .map(str::to_string)
                .context("OIDC token response has no access_token")
        })
        .await
        .map_err(|error| anyhow::anyhow!("OIDC token task failed: {error}"))??;

        let agent = self.agent.clone();
        let profile = tokio::task::spawn_blocking(move || {
            let response = agent
                .get(&userinfo_url)
                .set("Authorization", &format!("Bearer {token}"))
                .call()
                .map_err(|error| anyhow::anyhow!("OIDC userinfo failed: {error}"))?;
            let value: Value = response.into_json().context("parse OIDC userinfo")?;
            let username = value
                .get("preferred_username")
                .or_else(|| value.get("email"))
                .or_else(|| value.get("name"))
                .or_else(|| value.get("sub"))
                .and_then(Value::as_str)
                .context("OIDC userinfo has no username")?
                .to_string();
            Ok::<Value, anyhow::Error>(value).map(|value| (value, username))
        })
        .await
        .map_err(|error| anyhow::anyhow!("OIDC userinfo task failed: {error}"))??;
        let role = role_from_claims(&profile.0, &self.config);
        Ok(Profile {
            username: profile.1,
            role,
        })
    }

    async fn discovery(&self) -> Result<Discovery> {
        if let Some((discovery, at)) = self.discovery.lock().await.as_ref()
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(discovery.clone());
        }
        let issuer = self.config.issuer.clone();
        let agent = self.agent.clone();
        let discovery = tokio::task::spawn_blocking(move || {
            let url = format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            );
            let response = agent
                .get(&url)
                .call()
                .map_err(|error| anyhow::anyhow!("OIDC discovery failed: {error}"))?;
            let discovery: Discovery = response
                .into_json()
                .context("parse OIDC discovery document")?;
            Ok::<Discovery, anyhow::Error>(discovery)
        })
        .await
        .map_err(|error| anyhow::anyhow!("OIDC discovery task failed: {error}"))??;
        validate_oidc_endpoint(&discovery.issuer, "OIDC issuer")?;
        validate_oidc_endpoint(
            &discovery.authorization_endpoint,
            "OIDC authorization_endpoint",
        )?;
        *self.discovery.lock().await = Some((discovery.clone(), Instant::now()));
        Ok(discovery)
    }
}

fn role_from_claims(value: &Value, config: &OidcConfig) -> String {
    let mut roles: Vec<&str> = Vec::new();
    if let Some(claim) = value.get(&config.role_claim) {
        match claim {
            Value::String(role) => roles.push(role),
            Value::Array(values) => {
                roles.extend(values.iter().filter_map(Value::as_str));
            }
            _ => {}
        }
    }
    if roles
        .iter()
        .any(|role| role.eq_ignore_ascii_case(&config.admin_role))
    {
        return "admin".into();
    }
    if roles
        .iter()
        .any(|role| role.eq_ignore_ascii_case(&config.operator_role))
    {
        return "operator".into();
    }
    if roles
        .iter()
        .any(|role| role.eq_ignore_ascii_case(&config.viewer_role))
    {
        return "viewer".into();
    }
    config.default_role.clone()
}

fn random_value(bytes: usize) -> Result<String> {
    let mut data = vec![0u8; bytes];
    SystemRandom::new()
        .fill(&mut data)
        .map_err(|_| anyhow::anyhow!("generate OIDC random value"))?;
    Ok(URL_SAFE_NO_PAD.encode(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OidcConfig {
        OidcConfig {
            issuer: "https://id.example.com".into(),
            client_id: "client".into(),
            client_secret: "secret".into(),
            redirect_url: "https://tv.example.com/auth/oidc/callback".into(),
            scopes: "openid profile email".into(),
            role_claim: "roles".into(),
            admin_role: "admin".into(),
            operator_role: "operator".into(),
            viewer_role: "viewer".into(),
            default_role: "viewer".into(),
        }
    }

    #[test]
    fn maps_role_claims_by_priority() {
        let value = serde_json::json!({"roles": ["viewer", "operator"]});
        assert_eq!(role_from_claims(&value, &config()), "operator");
        let value = serde_json::json!({"roles": ["admin", "operator"]});
        assert_eq!(role_from_claims(&value, &config()), "admin");
        let value = serde_json::json!({"roles": "viewer"});
        assert_eq!(role_from_claims(&value, &config()), "viewer");
        let value = serde_json::json!({});
        assert_eq!(role_from_claims(&value, &config()), "viewer");
    }

    #[test]
    fn rejects_insecure_redirects_except_localhost() {
        assert!(validate_redirect_url("http://tv.example.com/cb", "redirect").is_err());
        assert!(validate_redirect_url("http://localhost:8409/cb", "redirect").is_ok());
        assert!(validate_redirect_url("https://tv.example.com/cb", "redirect").is_ok());
        assert!(validate_oidc_endpoint("http://127.0.0.1:8417", "issuer").is_ok());
        assert!(validate_oidc_endpoint("http://id.example.com", "issuer").is_err());
    }
}
