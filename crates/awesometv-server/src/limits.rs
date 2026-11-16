use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use axum::http::HeaderMap;

/// One minute of allowance. Fixed windows are deliberate: they are cheap, easy
/// to reason about, and only allow a short burst at a window boundary.
pub const RATE_WINDOW_MS: i64 = 60_000;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MediaClass {
    Api,
    Cast,
    Playlist,
    Segment,
    Stream,
}

impl MediaClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Cast => "cast",
            Self::Playlist => "playlist",
            Self::Segment => "segment",
            Self::Stream => "stream",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RateLimits {
    pub enabled: bool,
    pub api_per_min: u32,
    pub cast_per_min: u32,
    pub playlist_per_min: u32,
    pub segment_per_min: u32,
    pub stream_per_min: u32,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            enabled: true,
            api_per_min: 120,
            cast_per_min: 20,
            playlist_per_min: 120,
            segment_per_min: 1_800,
            stream_per_min: 12,
        }
    }
}

impl RateLimits {
    fn limit(&self, class: MediaClass) -> u32 {
        match class {
            MediaClass::Api => self.api_per_min,
            MediaClass::Cast => self.cast_per_min,
            MediaClass::Playlist => self.playlist_per_min,
            MediaClass::Segment => self.segment_per_min,
            MediaClass::Stream => self.stream_per_min,
        }
    }
}

/// Global and per-client caps for long-lived MPEG-TS sessions. A lease is held
/// until the ffmpeg child exits, so a client cannot open an unlimited number of
/// tuners or starve every other viewer.
pub struct StreamLimiter {
    global: Arc<Semaphore>,
    per_client: Mutex<HashMap<String, usize>>,
    max_per_client: usize,
}

pub struct StreamLease {
    limiter: Arc<StreamLimiter>,
    key: String,
    _permit: OwnedSemaphorePermit,
}

impl StreamLimiter {
    pub fn new(max_global: usize, max_per_client: usize) -> Self {
        Self {
            global: Arc::new(Semaphore::new(max_global.max(1))),
            per_client: Mutex::new(HashMap::new()),
            max_per_client,
        }
    }

    pub fn try_acquire(self: &Arc<Self>, key: &str) -> Option<StreamLease> {
        let permit = self.global.clone().try_acquire_owned().ok()?;
        let mut counts = self.per_client.lock().ok()?;
        let count = counts.get(key).copied().unwrap_or(0);
        if self.max_per_client > 0 && count >= self.max_per_client {
            return None;
        }
        counts.insert(key.to_string(), count + 1);
        Some(StreamLease {
            limiter: self.clone(),
            key: key.to_string(),
            _permit: permit,
        })
    }

    fn release(&self, key: &str) {
        let Ok(mut counts) = self.per_client.lock() else {
            return;
        };
        let Some(count) = counts.get_mut(key) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            counts.remove(key);
        }
    }
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        self.limiter.release(&self.key);
    }
}

#[derive(Default)]
struct Bucket {
    window_start_ms: i64,
    count: u32,
}

/// Per-client fixed-window counters for public media traffic. The map is
/// bounded and old windows are pruned so a large number of short-lived clients
/// cannot grow it without limit.
#[derive(Default)]
pub struct MediaRateLimiter {
    limits: RateLimits,
    buckets: HashMap<(String, MediaClass), Bucket>,
}

impl MediaRateLimiter {
    pub fn new(limits: RateLimits) -> Self {
        Self {
            limits,
            buckets: HashMap::new(),
        }
    }

    /// Returns the number of seconds to wait when the caller is over budget.
    /// A successful call consumes one token for this client and class.
    pub fn check(&mut self, key: &str, class: MediaClass, now_ms: i64) -> Option<u64> {
        if !self.limits.enabled {
            return None;
        }
        let limit = self.limits.limit(class);
        if limit == 0 {
            return None;
        }
        if self.buckets.len() > 8_192 {
            self.prune(now_ms);
        }
        let bucket = self.buckets.entry((key.to_string(), class)).or_default();
        if bucket.window_start_ms == 0
            || now_ms.saturating_sub(bucket.window_start_ms) >= RATE_WINDOW_MS
        {
            bucket.window_start_ms = now_ms;
            bucket.count = 0;
        }
        if bucket.count >= limit {
            let remaining = RATE_WINDOW_MS - now_ms.saturating_sub(bucket.window_start_ms);
            return Some((remaining.max(1_000) / 1_000) as u64);
        }
        bucket.count += 1;
        None
    }

    fn prune(&mut self, now_ms: i64) {
        self.buckets
            .retain(|_, bucket| now_ms.saturating_sub(bucket.window_start_ms) < RATE_WINDOW_MS);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.len()
    }
}

/// Prefer the header a reverse proxy sets itself. `X-Forwarded-For` is
/// append-only, so the last entry is the one added by the closest proxy; using
/// the first entry would let a client spoof its way around the limiter.
///
/// Forwarding headers are only trusted when the immediate peer is a loopback,
/// private, link-local, or Tailscale address. A direct public client cannot
/// choose its own bucket by sending `X-Real-IP`.
pub fn client_key(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let trust_forwarded = peer.is_some_and(|address| is_private_peer(address.ip()));
    if trust_forwarded {
        if let Some(ip) = header_ip(headers, "x-real-ip") {
            return ip.to_string();
        }
        if let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.rsplit(',').next())
            .and_then(parse_ip)
        {
            return ip.to_string();
        }
    }
    peer.map(|address| address.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn header_ip(headers: &HeaderMap, name: &'static str) -> Option<IpAddr> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_ip)
}

fn parse_ip(value: &str) -> Option<IpAddr> {
    value
        .trim()
        .trim_matches('"')
        .parse::<IpAddr>()
        .ok()
        .or_else(|| {
            value
                .trim()
                .parse::<SocketAddr>()
                .ok()
                .map(|address| address.ip())
        })
}

fn is_private_peer(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                // 100.64.0.0/10, the shared address space used by Tailscale.
                || (address.octets()[0] == 100 && (64..=127).contains(&address.octets()[1]))
        }
        IpAddr::V6(address) => {
            address.is_loopback() || address.is_unique_local() || address.is_unicast_link_local()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_static(value));
        }
        headers
    }

    fn peer(value: &str) -> SocketAddr {
        value.parse().unwrap()
    }

    #[test]
    fn blocks_after_the_limit_and_reports_retry_after() {
        let mut limiter = MediaRateLimiter::new(RateLimits {
            segment_per_min: 2,
            ..RateLimits::default()
        });
        let now = 1_000_000;
        assert_eq!(limiter.check("1.2.3.4", MediaClass::Segment, now), None);
        assert_eq!(limiter.check("1.2.3.4", MediaClass::Segment, now), None);
        assert!(limiter.check("1.2.3.4", MediaClass::Segment, now).is_some());
        assert_eq!(limiter.check("5.6.7.8", MediaClass::Segment, now), None);
    }

    #[test]
    fn classes_have_independent_budgets() {
        let mut limiter = MediaRateLimiter::new(RateLimits {
            playlist_per_min: 1,
            segment_per_min: 1,
            ..RateLimits::default()
        });
        let now = 1_000_000;
        assert_eq!(limiter.check("1.2.3.4", MediaClass::Playlist, now), None);
        assert_eq!(limiter.check("1.2.3.4", MediaClass::Segment, now), None);
        assert!(
            limiter
                .check("1.2.3.4", MediaClass::Playlist, now)
                .is_some()
        );
    }

    #[test]
    fn the_window_resets() {
        let mut limiter = MediaRateLimiter::new(RateLimits {
            api_per_min: 1,
            ..RateLimits::default()
        });
        let now = 1_000_000;
        assert_eq!(limiter.check("1.2.3.4", MediaClass::Api, now), None);
        assert!(limiter.check("1.2.3.4", MediaClass::Api, now).is_some());
        assert_eq!(
            limiter.check("1.2.3.4", MediaClass::Api, now + RATE_WINDOW_MS),
            None
        );
    }

    #[test]
    fn disabled_limits_allow_everything() {
        let mut limiter = MediaRateLimiter::new(RateLimits {
            enabled: false,
            segment_per_min: 1,
            ..RateLimits::default()
        });
        for _ in 0..10 {
            assert_eq!(limiter.check("1.2.3.4", MediaClass::Segment, 1_000), None);
        }
        assert_eq!(limiter.len(), 0);
    }

    #[test]
    fn trusted_proxy_headers_choose_the_client_bucket() {
        let headers = headers(&[("x-real-ip", "203.0.113.9")]);
        assert_eq!(
            client_key(&headers, Some(peer("127.0.0.1:1234"))),
            "203.0.113.9"
        );
    }

    #[test]
    fn untrusted_peers_cannot_spoof_forwarded_headers() {
        let headers = headers(&[
            ("x-real-ip", "203.0.113.9"),
            ("x-forwarded-for", "198.51.100.7, 203.0.113.9"),
        ]);
        assert_eq!(
            client_key(&headers, Some(peer("198.51.100.50:1234"))),
            "198.51.100.50"
        );
    }

    #[test]
    fn forwarded_for_uses_the_closest_proxy_entry() {
        let headers = headers(&[("x-forwarded-for", "198.51.100.7, 203.0.113.9")]);
        assert_eq!(
            client_key(&headers, Some(peer("10.0.0.2:1234"))),
            "203.0.113.9"
        );
    }

    #[test]
    fn stream_leases_enforce_global_and_client_caps() {
        let limiter = Arc::new(StreamLimiter::new(2, 1));
        let first = limiter.try_acquire("a").expect("first stream");
        assert!(limiter.try_acquire("a").is_none());
        let second = limiter.try_acquire("b").expect("second stream");
        assert!(limiter.try_acquire("c").is_none());
        drop(first);
        assert!(limiter.try_acquire("c").is_some());
        drop(second);
    }

    #[test]
    fn peer_address_is_the_fallback() {
        assert_eq!(
            client_key(&HeaderMap::new(), Some(peer("192.168.1.20:5678"))),
            "192.168.1.20"
        );
    }
}
