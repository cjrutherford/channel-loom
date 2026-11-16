use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GoogleCredentials {
    ServiceAccount {
        client_email: String,
        private_key: String,
        #[serde(default)]
        token_uri: Option<String>,
    },
    OAuth {
        refresh_token: String,
        client_id: String,
        client_secret: String,
        #[serde(default)]
        token_uri: Option<String>,
    },
    AccessToken {
        access_token: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteEntry {
    pub id: String,
    pub name: String,
    pub mime_type: String,
    pub size: Option<i64>,
    pub modified_at: Option<i64>,
    pub md5: Option<String>,
    pub is_folder: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveEntry {
    pub id: String,
    pub name: String,
}

pub struct GoogleClient {
    agent: ureq::Agent,
    api_base: String,
    token_url: String,
}

impl GoogleClient {
    pub fn new() -> Result<Self> {
        let api_base = std::env::var("GOOGLE_DRIVE_API_BASE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "https://www.googleapis.com/drive/v3".into());
        let api_base = validate_https_endpoint(&api_base, "GOOGLE_DRIVE_API_BASE")?;
        let token_url = std::env::var("GOOGLE_OAUTH_TOKEN_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "https://oauth2.googleapis.com/token".into());
        let token_url = validate_https_endpoint(&token_url, "GOOGLE_OAUTH_TOKEN_URL")?;
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(60))
            .build();
        Ok(Self {
            agent,
            api_base,
            token_url,
        })
    }

    pub fn access_token(&self, credentials: &GoogleCredentials) -> Result<String> {
        match credentials {
            GoogleCredentials::AccessToken { access_token } => Ok(access_token.clone()),
            GoogleCredentials::ServiceAccount {
                client_email,
                private_key,
                token_uri,
            } => {
                let token_url = token_uri.clone().unwrap_or_else(|| self.token_url.clone());
                let assertion = service_account_assertion(client_email, private_key, &token_url)?;
                self.exchange(
                    &token_url,
                    &[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                        ("assertion", assertion.as_str()),
                    ],
                )
            }
            GoogleCredentials::OAuth {
                refresh_token,
                client_id,
                client_secret,
                token_uri,
            } => {
                let token_url = token_uri.clone().unwrap_or_else(|| self.token_url.clone());
                self.exchange(
                    &token_url,
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh_token.as_str()),
                        ("client_id", client_id.as_str()),
                        ("client_secret", client_secret.as_str()),
                    ],
                )
            }
        }
    }

    fn exchange(&self, token_url: &str, form: &[(&str, &str)]) -> Result<String> {
        let response = self
            .agent
            .post(token_url)
            .send_form(form)
            .map_err(|error| anyhow::anyhow!("Google token exchange failed: {error}"))?;
        let value: Value = response
            .into_json()
            .context("parse Google token response")?;
        value
            .get("access_token")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .context("Google token response did not include access_token")
    }

    pub fn list_folder(
        &self,
        token: &str,
        folder_id: &str,
        page_token: Option<&str>,
    ) -> Result<(Vec<RemoteEntry>, Option<String>)> {
        let mut url = Url::parse(&format!("{}/files", self.api_base))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair(
                "q",
                &format!("'{folder_id}' in parents and trashed = false"),
            );
            query.append_pair(
                "fields",
                "nextPageToken,files(id,name,mimeType,size,modifiedTime,md5Checksum)",
            );
            query.append_pair("pageSize", "1000");
            query.append_pair("supportsAllDrives", "true");
            query.append_pair("includeItemsFromAllDrives", "true");
            if let Some(page_token) = page_token {
                query.append_pair("pageToken", page_token);
            }
        }
        let value = self.get_json(token, url.as_str())?;
        let files = value
            .get("files")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let mut entries = Vec::new();
        for file in files {
            let Some(id) = file.get("id").and_then(|value| value.as_str()) else {
                continue;
            };
            let mime_type = file
                .get("mimeType")
                .and_then(|value| value.as_str())
                .unwrap_or("application/octet-stream")
                .to_string();
            entries.push(RemoteEntry {
                id: id.to_string(),
                name: file
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or(id)
                    .to_string(),
                is_folder: mime_type == "application/vnd.google-apps.folder",
                mime_type,
                size: file
                    .get("size")
                    .and_then(|value| value.as_str())
                    .and_then(|value| value.parse::<i64>().ok()),
                modified_at: file
                    .get("modifiedTime")
                    .and_then(|value| value.as_str())
                    .and_then(|value| {
                        OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                            .ok()
                    })
                    .map(|value| value.unix_timestamp() * 1000),
                md5: file
                    .get("md5Checksum")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            });
        }
        let next = value
            .get("nextPageToken")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        Ok((entries, next))
    }

    pub fn list_shared_drives(&self, token: &str) -> Result<Vec<DriveEntry>> {
        let mut url = Url::parse(&format!("{}/drives", self.api_base))?;
        url.query_pairs_mut()
            .append_pair("pageSize", "100")
            .append_pair("fields", "drives(id,name)");
        let value = self.get_json(token, url.as_str())?;
        Ok(value
            .get("drives")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|drive| {
                Some(DriveEntry {
                    id: drive.get("id")?.as_str()?.to_string(),
                    name: drive.get("name")?.as_str()?.to_string(),
                })
            })
            .collect())
    }

    pub fn download(
        &self,
        token: &str,
        remote_id: &str,
        range: Option<&str>,
    ) -> Result<ureq::Response> {
        let url = file_url(&self.api_base, remote_id)?;
        let mut request = self
            .agent
            .get(url.as_str())
            .set("Authorization", &format!("Bearer {token}"));
        if let Some(range) = range {
            request = request.set("Range", range);
        }
        request
            .call()
            .map_err(|error| anyhow::anyhow!("Google download failed: {error}"))
    }

    fn get_json(&self, token: &str, url: &str) -> Result<Value> {
        let response = self
            .agent
            .get(url)
            .set("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|error| anyhow::anyhow!("Google Drive request failed: {error}"))?;
        response.into_json().context("parse Google Drive response")
    }
}

fn file_url(api_base: &str, remote_id: &str) -> Result<Url> {
    let mut url = Url::parse(&format!("{}/files", api_base.trim_end_matches('/')))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Google API base cannot be a base URL"))?
        .pop_if_empty()
        .push(remote_id);
    url.query_pairs_mut()
        .append_pair("alt", "media")
        .append_pair("supportsAllDrives", "true");
    Ok(url)
}

/// Reject plaintext and obviously internal endpoints. Operator environment
/// values and service-account `token_uri` fields both pass through here so a
/// cloud source cannot turn the server into an SSRF relay.
pub(crate) fn validate_https_endpoint(value: &str, label: &str) -> Result<String> {
    let trimmed = value.trim();
    let parsed = Url::parse(trimmed).with_context(|| format!("{label} must be a URL"))?;
    if parsed.scheme() != "https" {
        bail!("{label} must use https");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("{label} must not include credentials");
    }
    let host = parsed
        .host_str()
        .with_context(|| format!("{label} must include a host"))?;
    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".internal")
        || host_lower == "metadata"
    {
        bail!("{label} must not target an internal host");
    }
    if let Ok(ip) = host.parse::<IpAddr>()
        && is_private_or_loopback(ip)
    {
        bail!("{label} must not target a private or loopback address");
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

fn is_private_or_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
                || (address.octets()[0] == 100 && (64..=127).contains(&address.octets()[1]))
        }
        IpAddr::V6(address) => {
            address.is_loopback()
                || address.is_unspecified()
                || address.is_unique_local()
                || address.is_unicast_link_local()
        }
    }
}

fn service_account_assertion(
    client_email: &str,
    private_key: &str,
    token_url: &str,
) -> Result<String> {
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iss": client_email,
        "scope": "https://www.googleapis.com/auth/drive.readonly",
        "aud": token_url,
        "iat": now,
        "exp": now + 3600,
    });
    let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
    let signing_input = format!("{header}.{claims}");
    let der = pem_to_der(private_key)?;
    let key = RsaKeyPair::from_pkcs8(&der)
        .map_err(|_| anyhow::anyhow!("invalid service account private key"))?;
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| anyhow::anyhow!("sign service account assertion"))?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    STANDARD
        .decode(body)
        .context("decode service account private key PEM")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_google_https_endpoints() {
        assert_eq!(
            validate_https_endpoint("https://www.googleapis.com/drive/v3/", "api").unwrap(),
            "https://www.googleapis.com/drive/v3"
        );
        assert!(validate_https_endpoint("https://oauth2.googleapis.com/token", "token").is_ok());
    }

    #[test]
    fn rejects_plaintext_and_internal_endpoints() {
        for value in [
            "http://www.googleapis.com/drive/v3",
            "https://localhost/token",
            "https://127.0.0.1/token",
            "https://10.0.0.5/token",
            "https://169.254.169.254/latest/meta-data",
            "https://metadata.google.internal/token",
            "https://user:pass@example.com/token",
        ] {
            assert!(
                validate_https_endpoint(value, "test").is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn encodes_remote_ids_as_one_path_segment() {
        let url = file_url("https://www.googleapis.com/drive/v3", "a/b?c#d").unwrap();
        assert!(url.path().ends_with("/files/a%2Fb%3Fc%23d"));
        assert_eq!(url.query(), Some("alt=media&supportsAllDrives=true"));
    }
}
