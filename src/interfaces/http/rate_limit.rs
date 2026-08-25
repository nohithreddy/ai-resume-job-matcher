use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::{AppState, errors::ApiError};

/// Key used when the server cannot determine the client address, for example
/// in tests that drive the router directly without connection information.
const FALLBACK_KEY: &str = "unknown";

/// Upper bound on tracked client keys; when exceeded, stale empty entries are
/// dropped so the map cannot grow without limit.
const MAX_TRACKED_KEYS: usize = 10_000;

/// In-memory sliding-window rate limiter keyed by client IP.
///
/// Each key holds the timestamps of accepted hits inside the configured
/// window. A hit is allowed while fewer than `max_requests` unexpired
/// timestamps exist, otherwise the caller receives the number of seconds
/// until the oldest hit leaves the window.
#[derive(Debug)]
pub struct AuthRateLimiter {
    max_requests: usize,
    window: Duration,
    hits: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl AuthRateLimiter {
    pub fn new(max_requests: u32, window_seconds: u64) -> Self {
        Self {
            max_requests: usize::try_from(max_requests.max(1)).unwrap_or(usize::MAX),
            window: Duration::from_secs(window_seconds.max(1)),
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Registers one hit for `key`. Returns `Ok(())` when the request is
    /// allowed or `Err(retry_after_seconds)` when the window is exhausted.
    pub fn check(&self, key: &str) -> Result<(), u64> {
        let mut hits = self.hits.lock().expect("auth rate limiter lock poisoned");
        let now = Instant::now();
        let entry = hits.entry(key.to_owned()).or_default();
        while entry
            .front()
            .is_some_and(|oldest| now.duration_since(*oldest) >= self.window)
        {
            entry.pop_front();
        }
        if entry.len() >= self.max_requests {
            let retry_after = entry.front().map_or(1, |oldest| {
                self.window
                    .checked_sub(now.duration_since(*oldest))
                    .map_or(1, |remaining| remaining.as_secs().max(1))
            });
            return Err(retry_after);
        }
        entry.push_back(now);
        if hits.len() > MAX_TRACKED_KEYS {
            hits.retain(|_, timestamps| !timestamps.is_empty());
        }
        Ok(())
    }
}

pub async fn auth_rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if let Err(retry_after_seconds) = state.rate_limiter.check(&client_ip(&request)) {
        metrics::counter!("auth_rate_limited_total").increment(1);
        return ApiError::too_many_requests(retry_after_seconds).into_response();
    }
    next.run(request).await
}

fn client_ip(request: &Request) -> String {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or_else(
            || FALLBACK_KEY.to_owned(),
            |connect_info| connect_info.0.ip().to_string(),
        )
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::AuthRateLimiter;

    const FALLBACK_TEST_KEY: &str = "unknown";

    #[test]
    fn allows_up_to_the_limit_then_reports_retry_after() {
        let limiter = AuthRateLimiter::new(2, 60);
        assert!(limiter.check("203.0.113.10").is_ok());
        assert!(limiter.check("203.0.113.10").is_ok());
        let retry_after = limiter
            .check("203.0.113.10")
            .expect_err("the third hit must be limited");
        assert!(
            (1..=60).contains(&retry_after),
            "retry-after should be seconds within the window"
        );
    }

    #[test]
    fn limits_are_tracked_per_client_key() {
        let limiter = AuthRateLimiter::new(1, 60);
        assert!(limiter.check("203.0.113.10").is_ok());
        assert!(limiter.check("203.0.113.10").is_err());
        assert!(limiter.check("203.0.113.11").is_ok());
        assert!(limiter.check(FALLBACK_TEST_KEY).is_ok());
    }

    #[test]
    fn expired_hits_leave_the_window() {
        let limiter = AuthRateLimiter::new(1, 60);
        assert!(limiter.check("203.0.113.10").is_ok());
        assert!(limiter.check("203.0.113.10").is_err());
        {
            let mut hits = limiter.hits.lock().expect("lock should not be poisoned");
            for timestamps in hits.values_mut() {
                *timestamps.front_mut().expect("a hit is recorded") =
                    Instant::now() - Duration::from_secs(61);
            }
        }
        assert!(limiter.check("203.0.113.10").is_ok());
    }
}
