use anyhow::{bail, Result};
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{Response, StatusCode};
use std::time::Duration;

pub const CHROME_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Header set that mirrors what a real Chrome tab sends for a top-level
/// navigation. Retailers behind bot mitigation reject requests that carry only
/// a User-Agent, so the client hints and fetch metadata matter.
pub fn browser_headers(accept_language: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        ),
    );
    headers.insert(
        reqwest::header::ACCEPT_LANGUAGE,
        HeaderValue::from_str(accept_language).unwrap_or(HeaderValue::from_static("fi-FI,fi;q=0.9")),
    );
    headers.insert("sec-ch-ua", HeaderValue::from_static(
        "\"Chromium\";v=\"131\", \"Not_A Brand\";v=\"24\", \"Google Chrome\";v=\"131\"",
    ));
    headers.insert("sec-ch-ua-mobile", HeaderValue::from_static("?0"));
    headers.insert("sec-ch-ua-platform", HeaderValue::from_static("\"Linux\""));
    headers.insert("sec-fetch-dest", HeaderValue::from_static("document"));
    headers.insert("sec-fetch-mode", HeaderValue::from_static("navigate"));
    headers.insert("sec-fetch-site", HeaderValue::from_static("none"));
    headers.insert("sec-fetch-user", HeaderValue::from_static("?1"));
    headers.insert("upgrade-insecure-requests", HeaderValue::from_static("1"));
    headers
}

/// Builds a client that keeps cookies across requests, which is required by the
/// retailers that hand out a session cookie on the first page view.
pub fn browser_client(accept_language: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(CHROME_UA)
        .default_headers(browser_headers(accept_language))
        .cookie_store(true)
        .timeout(Duration::from_secs(30))
        .build()
        .expect("static client configuration is valid")
}

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(800),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    pub fn new(max_attempts: u32, base_delay: Duration, max_delay: Duration) -> Self {
        Self {
            max_attempts,
            base_delay,
            max_delay,
        }
    }

    /// Exponential backoff with a caller-supplied jitter fraction.
    ///
    /// `jitter` is taken as a parameter rather than sampled internally so the
    /// schedule can be asserted in tests; `backoff` is the randomized wrapper.
    /// A server-sent `Retry-After` always wins over the computed delay, still
    /// clamped to `max_delay` so a hostile header cannot stall the process.
    pub fn backoff_with_jitter(
        &self,
        attempt: u32,
        retry_after: Option<Duration>,
        jitter: f64,
    ) -> Duration {
        if let Some(server_delay) = retry_after {
            return server_delay.min(self.max_delay);
        }
        let exponent = attempt.saturating_sub(1).min(16);
        let scaled = self.base_delay.saturating_mul(1u32 << exponent);
        let capped = scaled.min(self.max_delay);
        let jitter = jitter.clamp(0.0, 1.0);
        capped.mul_f64(0.5 + 0.5 * jitter)
    }

    pub fn backoff(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let jitter = rand::thread_rng().gen_range(0.0..1.0);
        self.backoff_with_jitter(attempt, retry_after, jitter)
    }
}

/// Statuses worth retrying: transient rate limiting and gateway failures.
/// A 403 is deliberately excluded — it means a challenge that retrying alone
/// will never satisfy, so failing fast gives the caller a truthful error.
pub fn is_retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?;
    if let Ok(secs) = raw.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let when = chrono::DateTime::parse_from_rfc2822(raw.trim()).ok()?;
    let delta = when.with_timezone(&chrono::Utc) - chrono::Utc::now();
    delta.to_std().ok()
}

/// Issues a GET, retrying transient failures with backoff.
///
/// Returns the last response when every attempt fails so the caller can report
/// the real status rather than a generic timeout.
pub async fn get_with_retry(
    client: &reqwest::Client,
    url: &str,
    policy: RetryPolicy,
    referer: Option<&str>,
) -> Result<Response> {
    let mut last_status: Option<StatusCode> = None;

    for attempt in 1..=policy.max_attempts {
        let mut request = client.get(url);
        if let Some(referer) = referer {
            request = request.header(reqwest::header::REFERER, referer);
        }

        match request.send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(response);
                }
                if !is_retryable(status) || attempt == policy.max_attempts {
                    return Ok(response);
                }
                let retry_after = parse_retry_after(response.headers());
                last_status = Some(status);
                tracing::debug!(
                    url,
                    attempt,
                    status = status.as_u16(),
                    "retrying after transient response"
                );
                tokio::time::sleep(policy.backoff(attempt, retry_after)).await;
            }
            Err(err) => {
                if attempt == policy.max_attempts {
                    return Err(err.into());
                }
                tracing::debug!(url, attempt, error = %err, "retrying after transport error");
                tokio::time::sleep(policy.backoff(attempt, None)).await;
            }
        }
    }

    match last_status {
        Some(status) => bail!("{} failed after {} attempts ({})", url, policy.max_attempts, status),
        None => bail!("{} failed after {} attempts", url, policy.max_attempts),
    }
}

/// Fetches a URL and returns its body, mapping a non-success status to an error
/// that names the retailer and status.
pub async fn get_text(
    client: &reqwest::Client,
    url: &str,
    policy: RetryPolicy,
    referer: Option<&str>,
    retailer: &str,
) -> Result<String> {
    let response = get_with_retry(client, url, policy, referer).await?;
    let status = response.status();
    if !status.is_success() {
        bail!("{} returned HTTP {}", retailer, status.as_u16());
    }
    Ok(response.text().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy::new(5, Duration::from_millis(1000), Duration::from_secs(20))
    }

    #[test]
    fn backoff_grows_exponentially() {
        let p = policy();
        assert_eq!(p.backoff_with_jitter(1, None, 1.0), Duration::from_millis(1000));
        assert_eq!(p.backoff_with_jitter(2, None, 1.0), Duration::from_millis(2000));
        assert_eq!(p.backoff_with_jitter(3, None, 1.0), Duration::from_millis(4000));
        assert_eq!(p.backoff_with_jitter(4, None, 1.0), Duration::from_millis(8000));
    }

    #[test]
    fn backoff_jitter_halves_the_delay_at_the_low_end() {
        let p = policy();
        assert_eq!(p.backoff_with_jitter(2, None, 0.0), Duration::from_millis(1000));
        assert_eq!(p.backoff_with_jitter(2, None, 1.0), Duration::from_millis(2000));
    }

    #[test]
    fn backoff_is_clamped_to_max_delay() {
        let p = policy();
        assert_eq!(p.backoff_with_jitter(20, None, 1.0), Duration::from_secs(20));
    }

    #[test]
    fn retry_after_overrides_computed_backoff_but_respects_the_cap() {
        let p = policy();
        assert_eq!(
            p.backoff_with_jitter(1, Some(Duration::from_secs(7)), 1.0),
            Duration::from_secs(7)
        );
        assert_eq!(
            p.backoff_with_jitter(1, Some(Duration::from_secs(600)), 1.0),
            Duration::from_secs(20)
        );
    }

    #[test]
    fn retryable_covers_rate_limiting_but_not_challenges() {
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable(StatusCode::BAD_GATEWAY));
        assert!(!is_retryable(StatusCode::FORBIDDEN));
        assert!(!is_retryable(StatusCode::NOT_FOUND));
        assert!(!is_retryable(StatusCode::OK));
    }

    #[test]
    fn parse_retry_after_reads_delay_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("12"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(12)));
    }

    #[test]
    fn parse_retry_after_ignores_unparseable_values() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("soon"));
        assert_eq!(parse_retry_after(&headers), None);
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn browser_headers_carry_client_hints() {
        let headers = browser_headers("fi-FI,fi;q=0.9");
        assert!(headers.contains_key("sec-ch-ua"));
        assert!(headers.contains_key("sec-fetch-mode"));
        assert_eq!(
            headers.get(reqwest::header::ACCEPT_LANGUAGE).unwrap(),
            "fi-FI,fi;q=0.9"
        );
    }
}
