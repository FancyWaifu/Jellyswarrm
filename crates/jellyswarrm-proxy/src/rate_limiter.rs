//! Rate limiting service for protecting authentication endpoints

use governor::{
    clock::DefaultClock,
    state::{InMemoryState, NotKeyed},
    Quota, RateLimiter,
};
use moka::future::Cache;
use std::{
    net::{IpAddr, SocketAddr},
    num::NonZeroU32,
    sync::Arc,
    time::Duration,
};
use tracing::warn;

/// Per-IP rate limiter for authentication endpoints
#[derive(Clone)]
pub struct AuthRateLimiter {
    limiters: Cache<IpAddr, Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>>,
    max_requests: NonZeroU32,
    window: Duration,
}

impl AuthRateLimiter {
    pub fn new(max_requests: u32, window_secs: u64) -> Self {
        const DEFAULT_MAX_REQUESTS: NonZeroU32 = match NonZeroU32::new(5) {
            Some(v) => v,
            None => unreachable!(),
        };
        let max_requests = NonZeroU32::new(max_requests).unwrap_or(DEFAULT_MAX_REQUESTS);

        Self {
            limiters: Cache::builder()
                .time_to_idle(Duration::from_secs(window_secs * 10))
                .max_capacity(10_000)
                .build(),
            max_requests,
            window: Duration::from_secs(window_secs),
        }
    }

    pub fn default_auth_limiter() -> Self {
        Self::new(5, 10)
    }

    pub async fn check(&self, ip: IpAddr) -> bool {
        let limiter = self.get_or_create_limiter(ip).await;
        match limiter.check() {
            Ok(_) => true,
            Err(_) => {
                warn!("Rate limit exceeded for IP: {}", ip);
                false
            }
        }
    }

    async fn get_or_create_limiter(
        &self,
        ip: IpAddr,
    ) -> Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>> {
        if let Some(limiter) = self.limiters.get(&ip).await {
            return limiter;
        }

        let quota = Quota::with_period(self.window)
            .expect("Rate limiter window duration must be non-zero")
            .allow_burst(self.max_requests);

        let limiter = Arc::new(RateLimiter::direct(quota));
        self.limiters.insert(ip, limiter.clone()).await;
        limiter
    }
}

/// The IP to rate-limit on. The TCP peer is authoritative. Forwarding headers
/// are only believed when the peer is a configured `trusted_proxies` entry, and
/// then only the *rightmost* hop — the one that proxy appended. Everything left
/// of it is client-controlled and trivially spoofed. With `trust_cf` the
/// Cloudflare-set `CF-Connecting-IP` wins (Cloudflare overwrites any client copy).
pub fn client_ip(
    peer: SocketAddr,
    headers: &axum::http::HeaderMap,
    trusted_proxies: &[IpAddr],
    trust_cf: bool,
) -> IpAddr {
    let peer_ip = peer.ip().to_canonical();
    if !trusted_proxies.contains(&peer_ip) {
        return peer_ip;
    }
    let header_ip = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|ip| ip.trim().parse::<IpAddr>().ok())
    };
    trust_cf
        .then(|| header_ip("cf-connecting-ip"))
        .flatten()
        .or_else(|| header_ip("x-forwarded-for"))
        .or_else(|| header_ip("x-real-ip"))
        .unwrap_or(peer_ip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn test_rate_limiter_allows_within_limit() {
        let limiter = AuthRateLimiter::new(3, 10);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

        assert!(limiter.check(ip).await);
        assert!(limiter.check(ip).await);
        assert!(limiter.check(ip).await);
    }

    #[tokio::test]
    async fn test_rate_limiter_blocks_over_limit() {
        let limiter = AuthRateLimiter::new(2, 10);
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));

        assert!(limiter.check(ip).await);
        assert!(limiter.check(ip).await);
        assert!(!limiter.check(ip).await);
    }

    #[tokio::test]
    async fn test_different_ips_have_separate_limits() {
        let limiter = AuthRateLimiter::new(1, 10);
        let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        assert!(limiter.check(ip1).await);
        assert!(limiter.check(ip2).await);
        assert!(!limiter.check(ip1).await);
        assert!(!limiter.check(ip2).await);
    }

    #[test]
    fn client_ip_ignores_spoofable_headers() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4, 9.9.9.9".parse().unwrap());
        let proxy: SocketAddr = "172.18.0.2:5000".parse().unwrap();
        // Untrusted peer (even a private one): headers are ignored entirely.
        assert_eq!(client_ip(proxy, &h, &[], false), proxy.ip());
        // Trusted proxy: only the rightmost (proxy-appended) hop counts.
        assert_eq!(
            client_ip(proxy, &h, &[proxy.ip()], false),
            "9.9.9.9".parse::<IpAddr>().unwrap()
        );
        // Trusted proxy, no headers: the peer itself.
        assert_eq!(
            client_ip(proxy, &Default::default(), &[proxy.ip()], false),
            proxy.ip()
        );
    }

    #[test]
    fn cf_connecting_ip_only_when_enabled_and_peer_trusted() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("cf-connecting-ip", "5.6.7.8".parse().unwrap());
        h.insert("x-forwarded-for", "9.9.9.9".parse().unwrap());
        let tunnel: SocketAddr = "172.18.0.9:4000".parse().unwrap();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert_eq!(client_ip(tunnel, &h, &[tunnel.ip()], true), ip("5.6.7.8"));
        // Off by default: falls back to X-Forwarded-For.
        assert_eq!(client_ip(tunnel, &h, &[tunnel.ip()], false), ip("9.9.9.9"));
        // Never from an untrusted peer, even when enabled.
        assert_eq!(client_ip(tunnel, &h, &[], true), tunnel.ip());
        // Enabled but header missing: X-Forwarded-For as before.
        h.remove("cf-connecting-ip");
        assert_eq!(client_ip(tunnel, &h, &[tunnel.ip()], true), ip("9.9.9.9"));
    }
}
