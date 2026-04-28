//! Rate limiting service for protecting authentication endpoints

use governor::{
    clock::DefaultClock,
    state::{InMemoryState, NotKeyed},
    Quota, RateLimiter,
};
use moka::future::Cache;
use std::{net::IpAddr, num::NonZeroU32, sync::Arc, time::Duration};
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

/// Extract client IP from headers, falling back to UNSPECIFIED for direct
/// connections so they share a global limiter bucket.
pub fn extract_client_ip(headers: &axum::http::HeaderMap) -> IpAddr {
    if let Some(xff) = headers.get("x-forwarded-for") {
        if let Ok(xff_str) = xff.to_str() {
            if let Some(first_ip) = xff_str.split(',').next() {
                if let Ok(ip) = first_ip.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }

    if let Some(real_ip) = headers.get("x-real-ip") {
        if let Ok(ip_str) = real_ip.to_str() {
            if let Ok(ip) = ip_str.trim().parse::<IpAddr>() {
                return ip;
            }
        }
    }

    IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
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
}
