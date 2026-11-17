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
        Ok(Self::from_providers(vec![google]))
    }

    pub fn from_providers(providers: Vec<Arc<dyn CloudProvider>>) -> Self {
        let providers = providers
            .into_iter()
            .map(|provider| (provider.id(), provider))
            .collect();
        Self { providers }
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

#[cfg(test)]
mod conformance {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;

    use anyhow::{Context, Result, bail};

    use super::*;

    #[derive(Clone)]
    struct MockEntry {
        id: String,
        name: String,
        parent: String,
        is_folder: bool,
        data: Vec<u8>,
    }

    struct MockProvider {
        base: String,
        entries: Vec<MockEntry>,
        page_size: usize,
    }

    impl MockProvider {
        fn new(entries: Vec<MockEntry>) -> Self {
            let served = Arc::new(entries.clone());
            let served_for_thread = Arc::clone(&served);
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            thread::spawn(move || {
                for stream in listener.incoming().take(64) {
                    let Ok(mut stream) = stream else {
                        continue;
                    };
                    let mut buffer = [0u8; 8192];
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let mut lines = request.lines();
                    let mut first = lines.next().unwrap_or("").split_whitespace();
                    let method = first.next().unwrap_or("");
                    let path = first.next().unwrap_or("");
                    let id = path.trim_start_matches("/file/");
                    let Some(entry) = served_for_thread.iter().find(|entry| entry.id == id) else {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        continue;
                    };
                    let range = lines.find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("range: bytes=")
                            .map(str::to_string)
                    });
                    let total = entry.data.len();
                    let (status, start, end) = match range.and_then(|range| {
                        let (start, end) = range.split_once('-')?;
                        let start = start.parse::<usize>().ok()?;
                        let end = if end.is_empty() {
                            total.saturating_sub(1)
                        } else {
                            end.parse::<usize>().ok()?
                        };
                        Some((
                            start.min(total.saturating_sub(1)),
                            end.min(total.saturating_sub(1)),
                        ))
                    }) {
                        Some((start, end)) if start <= end => ("206 Partial Content", start, end),
                        _ => ("200 OK", 0, total.saturating_sub(1)),
                    };
                    let length = if total == 0 { 0 } else { end - start + 1 };
                    let mut headers = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nAccept-Ranges: bytes\r\nContent-Length: {length}\r\nConnection: close\r\n"
                    );
                    if status.starts_with("206") {
                        headers
                            .push_str(&format!("Content-Range: bytes {start}-{end}/{total}\r\n"));
                    }
                    headers.push_str("\r\n");
                    let _ = stream.write_all(headers.as_bytes());
                    if method != "HEAD" && total > 0 {
                        let _ = stream.write_all(&entry.data[start..=end]);
                    }
                }
            });
            Self {
                base: format!("http://{address}"),
                entries: served.as_ref().clone(),
                page_size: 3,
            }
        }

        fn tree(files: usize) -> Self {
            let mut entries = vec![
                MockEntry {
                    id: "folder".into(),
                    name: "Folder".into(),
                    parent: "root".into(),
                    is_folder: true,
                    data: Vec::new(),
                },
                MockEntry {
                    id: "sample".into(),
                    name: "sample.bin".into(),
                    parent: "root".into(),
                    is_folder: false,
                    data: b"abcdefghij".to_vec(),
                },
            ];
            for index in 0..files {
                entries.push(MockEntry {
                    id: format!("file-{index}"),
                    name: format!("file-{index}.bin"),
                    parent: "folder".into(),
                    is_folder: false,
                    data: vec![index as u8; 8],
                });
            }
            Self::new(entries)
        }
    }

    impl CloudProvider for MockProvider {
        fn id(&self) -> &'static str {
            "mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                range: true,
                head: true,
                shared_drives: false,
                delta: false,
                webhooks: false,
            }
        }

        fn validate_secret(&self, _auth_type: &str, secret: &str) -> Result<()> {
            if secret == "mock" {
                Ok(())
            } else {
                bail!("mock secret must be 'mock'")
            }
        }

        fn access_token(&self, _auth_type: &str, secret: &str) -> Result<String> {
            self.validate_secret("", secret)?;
            Ok("mock-token".into())
        }

        fn list_folder(
            &self,
            _token: &str,
            folder_id: &str,
            page: Option<&str>,
        ) -> Result<(Vec<RemoteEntry>, Option<String>)> {
            let mut children: Vec<&MockEntry> = self
                .entries
                .iter()
                .filter(|entry| entry.parent == folder_id)
                .collect();
            children.sort_by(|left, right| left.name.cmp(&right.name));
            let offset = page
                .and_then(|page| page.parse::<usize>().ok())
                .unwrap_or(0);
            let end = (offset + self.page_size).min(children.len());
            let entries = children[offset..end]
                .iter()
                .map(|entry| RemoteEntry {
                    id: entry.id.clone(),
                    name: entry.name.clone(),
                    mime_type: if entry.is_folder {
                        "application/vnd.mock.folder".into()
                    } else {
                        "application/octet-stream".into()
                    },
                    size: Some(entry.data.len() as i64),
                    modified_at: None,
                    md5: None,
                    is_folder: entry.is_folder,
                })
                .collect();
            let next = (end < children.len()).then(|| end.to_string());
            Ok((entries, next))
        }

        fn list_drives(&self, _token: &str) -> Result<Vec<DriveEntry>> {
            Ok(Vec::new())
        }

        fn download(
            &self,
            _token: &str,
            remote_id: &str,
            range: Option<&str>,
        ) -> Result<ureq::Response> {
            let url = format!("{}/file/{remote_id}", self.base);
            let mut request = ureq::get(&url);
            if let Some(range) = range {
                request = request.set("Range", range);
            }
            request
                .call()
                .map_err(|error| anyhow::anyhow!("mock download: {error}"))
        }

        fn head(&self, _token: &str, remote_id: &str) -> Result<ProviderHead> {
            let url = format!("{}/file/{remote_id}", self.base);
            let response = ureq::head(&url)
                .call()
                .map_err(|error| anyhow::anyhow!("mock HEAD: {error}"))?;
            Ok(ProviderHead {
                status: response.status(),
                content_type: response.header("Content-Type").map(str::to_string),
                content_length: response
                    .header("Content-Length")
                    .and_then(|value| value.parse().ok()),
                accept_ranges: response
                    .header("Accept-Ranges")
                    .is_some_and(|value| value.eq_ignore_ascii_case("bytes")),
            })
        }
    }

    fn run_conformance(provider: &dyn CloudProvider) -> Result<()> {
        let token = provider.access_token("access_token", "mock")?;
        let (root, _) = provider.list_folder(&token, "root", None)?;
        assert!(
            root.iter()
                .any(|entry| entry.id == "folder" && entry.is_folder)
        );
        assert!(
            root.iter()
                .any(|entry| entry.id == "sample" && !entry.is_folder)
        );

        let range = provider.download(&token, "sample", Some("bytes=0-3"))?;
        assert_eq!(range.status(), 206);
        let mut bytes = Vec::new();
        range.into_reader().read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"abcd");

        let head = provider.head(&token, "sample")?;
        assert_eq!(head.status, 200);
        assert_eq!(head.content_length, Some(10));
        assert!(head.accept_ranges);
        Ok(())
    }

    #[test]
    fn mock_provider_passes_conformance() {
        let provider = MockProvider::tree(6);
        run_conformance(&provider).unwrap();
        let registry = ProviderRegistry::from_providers(vec![Arc::new(provider)]);
        assert_eq!(registry.list()[0].id, "mock");
    }

    #[test]
    fn pagination_handles_ten_thousand_entries() {
        let provider = MockProvider::tree(10_000);
        let token = "mock-token";
        let mut page = None;
        let mut seen = 0usize;
        loop {
            let (entries, next) = provider
                .list_folder(token, "folder", page.as_deref())
                .context("page")
                .unwrap();
            seen += entries.len();
            match next {
                Some(next) => page = Some(next),
                None => break,
            }
        }
        assert_eq!(seen, 10_000);
    }
}
