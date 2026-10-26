//! pulse: tiny HTTP server that answers `200 ok` so a monitor can ping it
//! and keep a VPS from idling down.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tokio::net::TcpListener;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::{limit::RequestBodyLimitLayer, timeout::TimeoutLayer};

const DEFAULT_PORT: u16 = 8080;
const DEFAULT_HEALTH_PATH: &str = "/health";
const DEFAULT_RATE_LIMIT_RPS: u32 = 20;
const BODY_LIMIT_BYTES: usize = 1024;
const MAX_CONCURRENT_REQUESTS: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Answer for the health path. HEAD is handled by axum automatically.
async fn ok() -> &'static str {
    "ok"
}

/// Body-less 404 for every unmatched path or unsupported method.
async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// Global token bucket shared by every request, regardless of client.
struct TokenBucket {
    capacity: f64,
    tokens: f64,
    rps: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// Starts full: a burst of `rps` requests is allowed, refilled at `rps`
    /// requests per second.
    fn new(rps: f64) -> Self {
        Self {
            capacity: rps,
            tokens: rps,
            rps,
            last_refill: Instant::now(),
        }
    }

    fn try_take(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rps).min(self.capacity);
        self.last_refill = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Rejects requests over the global rate with an empty 429.
async fn rate_limit(
    State(bucket): State<Arc<Mutex<TokenBucket>>>,
    request: Request,
    next: Next,
) -> Response {
    let admitted = bucket.lock().expect("rate limit mutex poisoned").try_take();
    if admitted {
        next.run(request).await
    } else {
        StatusCode::TOO_MANY_REQUESTS.into_response()
    }
}

fn app(health_path: &str, rate_limit_rps: u32) -> Router {
    let bucket = Arc::new(Mutex::new(TokenBucket::new(rate_limit_rps as f64)));

    Router::new()
        .route(health_path, get(ok))
        .fallback(not_found)
        // Path matched but method unsupported (e.g. POST on the health path):
        // still 404, not 405, so the server reveals nothing about its surface.
        .method_not_allowed_fallback(not_found)
        // Layers run in reverse registration order: the rate limit sheds
        // excess traffic first, then the body limit, then the timeout (which
        // also bounds time spent waiting for a concurrency slot), then the
        // concurrency limit.
        .layer(ConcurrencyLimitLayer::new(MAX_CONCURRENT_REQUESTS))
        .layer(TimeoutLayer::new(REQUEST_TIMEOUT))
        .layer(RequestBodyLimitLayer::new(BODY_LIMIT_BYTES))
        // Belt and suspenders: also cap bodies for any future body extractor.
        .layer(DefaultBodyLimit::max(BODY_LIMIT_BYTES))
        .layer(middleware::from_fn_with_state(bucket, rate_limit))
}

fn port() -> u16 {
    env("PORT").and_then(|value| value.parse().ok()).unwrap_or(DEFAULT_PORT)
}

fn health_path() -> String {
    normalize_health_path(env("HEALTH_PATH").as_deref().unwrap_or(DEFAULT_HEALTH_PATH))
}

/// Axum panics on paths that do not start with `/`, so tolerate a missing one.
fn normalize_health_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn rate_limit_rps() -> u32 {
    env("RATE_LIMIT_RPS")
        .and_then(|value| value.parse().ok())
        .filter(|rps| *rps > 0)
        .unwrap_or(DEFAULT_RATE_LIMIT_RPS)
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Resolves on the first SIGINT or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        signal(SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = port();
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    println!("pulse listening on 0.0.0.0:{port}");

    axum::serve(listener, app(&health_path(), rate_limit_rps()))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{header::CONTENT_LENGTH, Method, Request as HttpRequest};
    use tower::ServiceExt;

    async fn call(app: Router, method: Method, uri: &str) -> (StatusCode, Vec<u8>) {
        let request = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, body.to_vec())
    }

    #[tokio::test]
    async fn get_health_is_ok() {
        let (status, body) = call(app("/health", 20), Method::GET, "/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"ok");
    }

    #[tokio::test]
    async fn head_is_supported_without_body() {
        let (status, body) = call(app("/health", 20), Method::HEAD, "/health").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn root_is_empty_404() {
        let (status, body) = call(app("/health", 20), Method::GET, "/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn unknown_path_is_empty_404() {
        let (status, body) = call(
            app("/health", 20),
            Method::GET,
            "/definitely-not-here",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn wrong_method_is_empty_404() {
        let (status, body) = call(app("/health", 20), Method::POST, "/health").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn custom_health_path_works_and_root_stays_404() {
        let (status, body) = call(app("/status", 20), Method::GET, "/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"ok");

        let (status, body) = call(app("/status", 20), Method::GET, "/health").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.is_empty());

        let (status, body) = call(app("/status", 20), Method::GET, "/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let request = HttpRequest::builder()
            .method(Method::GET)
            .uri("/health")
            .header(CONTENT_LENGTH, "2048")
            .body(Body::from(vec![0u8; 2048]))
            .unwrap();
        let response = app("/health", 20).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn over_rate_limit_is_empty_429() {
        let app = app("/health", 1);

        let (status, _) = call(app.clone(), Method::GET, "/health").await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(app.clone(), Method::GET, "/health").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(body.is_empty());

        // The limit is global, not per path: other routes are shed too.
        let (status, _) = call(app, Method::GET, "/other").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn missing_leading_slash_in_health_path_is_normalized() {
        assert_eq!(normalize_health_path("health"), "/health");
        assert_eq!(normalize_health_path("/health"), "/health");
        assert_eq!(normalize_health_path(" /status "), "/status");

        let (status, body) = call(
            app(&normalize_health_path("status"), 20),
            Method::GET,
            "/status",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"ok");
    }
}
