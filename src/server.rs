use std::time::Instant;

use axum::Router;
use axum::extract::{MatchedPath, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use metrics_exporter_prometheus::PrometheusHandle;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{info, instrument};

#[instrument]
async fn profile_endpoint(request: Request, next: Next) -> Response {
    let method = request.method().clone().to_string();
    let path = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());
    info!(method, path, "request started");

    let now = Instant::now();
    let labels = [("method", method.clone()), ("path", path.clone())];
    let response = next.run(request).await;
    let elapsed = now.elapsed();

    ::metrics::histogram!("handler", &labels).record(elapsed);

    info!(
        method,
        path,
        status_code = response.status().as_u16(),
        duration_milliseconds = elapsed.as_millis(),
        "request completed"
    );
    response
}

#[instrument]
async fn metrics_handler(State(handle): State<PrometheusHandle>) -> String {
    handle.render()
}

#[instrument]
async fn ping_handler() -> StatusCode {
    StatusCode::OK
}

pub fn create_router(prometheus: PrometheusHandle) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/status/ping", get(ping_handler))
        .with_state(prometheus)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(middleware::from_fn(profile_endpoint)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::metrics::record_probe;
    use crate::probe::{ProbeOutcome, ProbeTarget};
    use crate::test_support::{http_metric, recorded_async};

    #[tokio::test]
    async fn metrics_handler_renders_prometheus_text_format() {
        let (_, handler_body) = recorded_async(|handle| async move {
            let target = ProbeTarget::http("https://example.com".to_string());
            record_probe(&target, ProbeOutcome::Http { status: 200 }, 0.25);
            metrics_handler(State(handle)).await
        })
        .await;

        assert!(handler_body.contains("# HELP probe_success"));
        assert!(handler_body.contains(&http_metric("https://example.com", "probe_success", 1)));
    }

    #[tokio::test]
    async fn status_ping_returns_ok() {
        let (_, response) = recorded_async(|handle| async move {
            create_router(handle)
                .oneshot(
                    Request::builder()
                        .uri("/status/ping")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        })
        .await;

        assert_eq!(response.status(), StatusCode::OK);
    }
}
