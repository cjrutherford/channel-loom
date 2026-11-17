use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use serde::Serialize;

use super::google::{DriveEntry, GoogleClient, RemoteEntry};

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCapabilities {
    pub range: bool,
    pub head: bool,
    pub shared_drives: bool,
    pub delta: bool,
    pub webhooks: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub capabilities: ProviderCapabilities,
}

#[derive(Debug, Clone)]
pub struct ProviderHead {
    pub status: u16,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub accept_ranges: bool,
}

pub trait CloudProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> ProviderCapabilities;
    fn validate_secret(&self, auth_type: &str, secret: &str) -> Result<()>;
    fn access_token(&self, auth_type: &str, secret: &str) -> Result<String>;
    fn list_folder(
        &self,
        token: &str,
        folder_id: &str,
        page: Option<&str>,
    ) -> Result<(Vec<RemoteEntry>, Option<String>)>;
    fn list_drives(&self, token: &str) -> Result<Vec<DriveEntry>>;
    fn download(&self, token: &str, remote_id: &str, range: Option<&str>)
    -> Result<ureq::Response>;
    fn head(&self, token: &str, remote_id: &str) -> Result<ProviderHead> {
        let _ = (token, remote_id);
        bail!("provider does not support HEAD")
    }
}

pub struct ProviderRegistry {
    providers: HashMap<&'static str, Arc<dyn CloudProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Result<Self> {
        let google = Arc::new(GoogleClient::new()?);
        let mut providers: HashMap<&'static str, Arc<dyn CloudProvider>> = HashMap::new();
        providers.insert(google.id(), google);
        Ok(Self { providers })
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn CloudProvider>> {
        self.providers.get(id).cloned()
    }

    pub fn require(&self, id: &str) -> Result<Arc<dyn CloudProvider>> {
        self.get(id)
            .ok_or_else(|| anyhow::anyhow!("unsupported cloud provider: {id}"))
    }

    pub fn list(&self) -> Vec<ProviderInfo> {
        let mut providers: Vec<ProviderInfo> = self
            .providers
            .values()
            .map(|provider| ProviderInfo {
                id: provider.id().to_string(),
                capabilities: provider.capabilities(),
            })
            .collect();
        providers.sort_by(|left, right| left.id.cmp(&right.id));
        providers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_google_capabilities() {
        let registry = ProviderRegistry::new().unwrap();
        let providers = registry.list();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "google");
        assert!(providers[0].capabilities.range);
        assert!(providers[0].capabilities.head);
        assert!(registry.require("google").is_ok());
        assert!(registry.require("missing").is_err());
    }
}
