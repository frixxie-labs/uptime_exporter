use std::time::Instant;

use axum::Router;
use axum::extract::{MatchedPath, Request, State};
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
    info!("Handling {} at {}", method, path);

    let now = Instant::now();
    let labels = [("method", method.clone()), ("path", path.clone())];
    let response = next.run(request).await;
    let elapsed = now.elapsed();

    ::metrics::histogram!("handler", &labels).record(elapsed);

    info!(
        "Finished handling {} at {}, used {} ms",
        method,
        path,
        elapsed.as_millis()
    );
    response
}

#[instrument]
async fn metrics_handler(State(handle): State<PrometheusHandle>) -> String {
    handle.render()
}

pub fn create_router(prometheus: PrometheusHandle) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
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
}
