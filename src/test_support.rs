use std::future::Future;

use axum::Router;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle, PrometheusRecorder};
use quickcheck::{Arbitrary, Gen};
use tokio::net::TcpListener;

use crate::metrics::describe_metrics;

pub fn new_recorder() -> (PrometheusRecorder, PrometheusHandle) {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    (recorder, handle)
}

pub fn recorded<T>(f: impl FnOnce(PrometheusHandle) -> T) -> (String, T) {
    let (recorder, handle) = new_recorder();
    let result = ::metrics::with_local_recorder(&recorder, || {
        describe_metrics();
        f(handle.clone())
    });

    (handle.render(), result)
}

pub async fn recorded_async<T, Fut>(f: impl FnOnce(PrometheusHandle) -> Fut) -> (String, T)
where
    Fut: Future<Output = T>,
{
    let (recorder, handle) = new_recorder();
    let _guard = ::metrics::set_default_local_recorder(&recorder);
    describe_metrics();
    let result = f(handle.clone()).await;

    (handle.render(), result)
}

pub async fn test_server(status: u16) -> String {
    let app = Router::new()
        .fallback(move || async move { axum::http::StatusCode::from_u16(status).unwrap() });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    format!("http://{addr}")
}

pub fn http_metric(target: &str, metric: &str, value: impl std::fmt::Display) -> String {
    format!("{metric}{{probe_type=\"http\",target=\"{target}\"}} {value}")
}

pub fn ping_metric(target: &str, metric: &str, value: impl std::fmt::Display) -> String {
    format!("{metric}{{probe_type=\"ping\",target=\"{target}\"}} {value}")
}

/// Characters that are safe to use unescaped inside a Prometheus label value
/// and don't collide with clap's flag syntax when used as a CLI argument.
pub const SAFE_ALPHABET: &[u8] =
    b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789:/.-_";

#[derive(Clone, Debug)]
pub struct SafeLabel(pub String);

impl Arbitrary for SafeLabel {
    fn arbitrary(g: &mut Gen) -> Self {
        let len = u8::arbitrary(g) as usize % 32;
        let s = (0..len)
            .map(|_| SAFE_ALPHABET[u8::arbitrary(g) as usize % SAFE_ALPHABET.len()] as char)
            .collect();
        SafeLabel(s)
    }
}
