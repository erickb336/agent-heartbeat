//! Sends one JSON body with HTTPS and classifies the answer. The tracker sends, never listens.
//!
//! - The macOS TLS stack (native-tls) checks the certificate.
//! - Redirects are not followed, so the bearer secret cannot reach another host.
//! - Only the HTTP status and Retry-After are read. The body is drained (for connection reuse)
//!   up to 4 KB and dropped.

use std::time::Duration;

use ureq::tls::{TlsConfig, TlsProvider};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    /// 2xx.
    Accepted,
    /// 409: the receiver already has a newer observation. Delivered; nothing to retry.
    Stale,
    /// 401 or 403: the secret is wrong. Retry slowly.
    Unauthorized,
    /// 429, 5xx, a timeout or a network error. Retry with backoff; Retry-After in seconds if given.
    RetryLater(Option<u64>),
    /// Any other status (400, 413, 415, 422, 3xx). Retrying the same snapshot will not help.
    Rejected(u16),
}

impl Outcome {
    pub fn describe(self) -> String {
        match self {
            Outcome::Accepted => "accepted".into(),
            Outcome::Stale => "stale (409)".into(),
            Outcome::Unauthorized => "unauthorized (401/403)".into(),
            Outcome::RetryLater(_) => "temporary failure".into(),
            Outcome::Rejected(code) => format!("rejected ({code})"),
        }
    }
}

pub struct Sender {
    endpoint: String,
    authorization: String,
    agent: ureq::Agent,
}

impl Sender {
    pub fn new(endpoint: String, secret: &str) -> Sender {
        let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .timeout_global(Some(Duration::from_secs(5)))
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .user_agent(concat!("agent-heartbeat/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Sender { endpoint, authorization: format!("Bearer {secret}"), agent }
    }

    pub fn send(&self, body: &[u8]) -> Outcome {
        let response = self
            .agent
            .post(&self.endpoint)
            .header("Authorization", &self.authorization)
            .header("Content-Type", "application/json")
            .send(body);
        let mut response = match response {
            Ok(r) => r,
            Err(_) => return Outcome::RetryLater(None),
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let _ = response.body_mut().with_config().limit(4096).read_to_vec();
        classify(status, retry_after)
    }
}

fn classify(status: u16, retry_after: Option<u64>) -> Outcome {
    match status {
        200..=299 => Outcome::Accepted,
        409 => Outcome::Stale,
        401 | 403 => Outcome::Unauthorized,
        429 | 500..=599 => Outcome::RetryLater(retry_after),
        other => Outcome::Rejected(other),
    }
}

/// Exponential backoff with full jitter: a random delay between 1 s and min(60, 2 × 2^(n-1)) s.
pub fn backoff(attempt: u32, retry_after: Option<u64>, random: u64) -> Duration {
    if let Some(s) = retry_after {
        return Duration::from_secs(s.clamp(1, 60));
    }
    let cap = 2u64.saturating_mul(1 << attempt.saturating_sub(1).min(5)).min(60);
    Duration::from_secs(1 + random % cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_actions() {
        assert_eq!(classify(204, None), Outcome::Accepted);
        assert_eq!(classify(409, None), Outcome::Stale);
        assert_eq!(classify(401, None), Outcome::Unauthorized);
        assert_eq!(classify(429, Some(3)), Outcome::RetryLater(Some(3)));
        assert_eq!(classify(503, None), Outcome::RetryLater(None));
        assert_eq!(classify(400, None), Outcome::Rejected(400));
        assert_eq!(classify(302, None), Outcome::Rejected(302));
    }

    #[test]
    fn backoff_grows_and_stays_within_the_cap() {
        for attempt in 1..20 {
            for random in [0, 7, 999_999] {
                let d = backoff(attempt, None, random).as_secs();
                assert!((1..=60).contains(&d), "attempt {attempt}: {d}");
            }
        }
        assert!(backoff(1, None, 999).as_secs() <= 2);
        assert_eq!(backoff(3, Some(500), 0).as_secs(), 60);
    }

    #[test]
    fn an_unreachable_endpoint_is_a_temporary_failure_not_a_crash() {
        // Port 9 on the loopback interface: nothing listens, so the connection is refused at once.
        let s = Sender::new("http://127.0.0.1:9/heartbeat".into(), "test-secret");
        assert_eq!(s.send(b"{}"), Outcome::RetryLater(None));
    }
}
