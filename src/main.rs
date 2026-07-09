use std::process::Stdio;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{MatchedPath, Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use clap::{ArgGroup, Parser};
use metrics::{describe_gauge, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::task::JoinSet;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;
use tracing::{Level, error, info, instrument};
use tracing_subscriber::FmtSubscriber;

/// Probes HTTP URLs and ping addresses on an interval and exposes the results as Prometheus metrics.
#[derive(Parser, Debug)]
#[command(name = "uptime_exporter", version, about)]
#[command(group(
    ArgGroup::new("targets")
        .required(true)
        .multiple(true)
        .args(["urls", "ping_addresses"])
))]
struct Args {
    /// URLs to probe with HTTP GET requests
    #[arg(value_name = "URL")]
    urls: Vec<String>,

    /// Addresses to probe with ICMP ping
    #[arg(long = "ping", value_name = "ADDRESS")]
    ping_addresses: Vec<String>,

    /// How often to (re-)probe each target, in seconds
    #[arg(
        long,
        env = "UPTIME_EXPORTER_INTERVAL",
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    interval: u64,

    /// Port for the Prometheus /metrics HTTP endpoint
    #[arg(long, env = "UPTIME_EXPORTER_PORT", default_value_t = 9200)]
    port: u16,

    /// Logging level for the application.
    #[arg(long, env = "UPTIME_EXPORTER_LOG_LEVEL", default_value = "info")]
    log_level: LogLevel,
}

impl Args {
    fn targets(&self) -> Vec<ProbeTarget> {
        self.urls
            .iter()
            .cloned()
            .map(ProbeTarget::http)
            .chain(self.ping_addresses.iter().cloned().map(ProbeTarget::ping))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeKind {
    Http,
    Ping,
}

impl ProbeKind {
    fn as_label(self) -> &'static str {
        match self {
            ProbeKind::Http => "http",
            ProbeKind::Ping => "ping",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeTarget {
    Http { url: String },
    Ping { address: String },
}

impl ProbeTarget {
    fn http(url: String) -> Self {
        ProbeTarget::Http { url }
    }

    fn ping(address: String) -> Self {
        ProbeTarget::Ping { address }
    }

    fn kind(&self) -> ProbeKind {
        match self {
            ProbeTarget::Http { .. } => ProbeKind::Http,
            ProbeTarget::Ping { .. } => ProbeKind::Ping,
        }
    }

    fn label(&self) -> &str {
        match self {
            ProbeTarget::Http { url } => url,
            ProbeTarget::Ping { address } => address,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOutcome {
    Http { status: u16 },
    Ping { success: bool },
}

impl ProbeOutcome {
    fn is_success(self) -> bool {
        match self {
            ProbeOutcome::Http { status } => is_http_probe_success(status),
            ProbeOutcome::Ping { success } => success,
        }
    }
}

#[derive(Debug, Clone)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl std::str::FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "trace" => Ok(LogLevel::Trace),
            "debug" => Ok(LogLevel::Debug),
            "info" => Ok(LogLevel::Info),
            "warn" => Ok(LogLevel::Warn),
            "error" => Ok(LogLevel::Error),
            _ => Err("unknown log level".to_string()),
        }
    }
}

impl From<LogLevel> for Level {
    fn from(log_level: LogLevel) -> Self {
        match log_level {
            LogLevel::Trace => Level::TRACE,
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Info => Level::INFO,
            LogLevel::Warn => Level::WARN,
            LogLevel::Error => Level::ERROR,
        }
    }
}

fn describe_metrics() {
    describe_gauge!(
        "probe_success",
        "Whether the probe succeeded (HTTP 200 OK or ping response) (1) or not (0)"
    );
    describe_gauge!(
        "probe_http_status_code",
        "HTTP status code returned by an HTTP probe (0 if the request failed)"
    );
    describe_gauge!(
        "probe_duration_seconds",
        "Duration of the probe request in seconds"
    );
}

/// Records the outcome of a single probe for `target`.
fn record_probe(target: &ProbeTarget, outcome: ProbeOutcome, elapsed_secs: f64) {
    let labels = [
        ("probe_type", target.kind().as_label().to_string()),
        ("target", target.label().to_string()),
    ];
    let success = if outcome.is_success() { 1.0 } else { 0.0 };

    gauge!("probe_success", &labels).set(success);
    gauge!("probe_duration_seconds", &labels).set(elapsed_secs);

    if let ProbeOutcome::Http { status } = outcome {
        gauge!("probe_http_status_code", &labels).set(status as f64);
    }
}

/// Whether an HTTP status code should be considered a successful probe.
///
/// Only an exact 200 counts as success, matching the "returns 200 OK" requirement
/// literally (a 201 or a 301-to-200 redirect target still count, since reqwest
/// follows redirects and reports the final status; a 204 or 404 do not).
fn is_http_probe_success(status: u16) -> bool {
    status == 200
}

fn ping_args(address: &str) -> [&str; 3] {
    ["-c", "1", address]
}

async fn ping_address(address: &str) -> std::io::Result<bool> {
    let mut command = Command::new("ping");
    command
        .args(ping_args(address))
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = command.spawn()?;
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(status) => status.map(|status| status.success()),
        Err(_) => {
            let _ = child.kill().await;
            Ok(false)
        }
    }
}

/// Runs a single probe against `target` and records the outcome.
#[instrument(skip(client))]
async fn probe_once(client: &reqwest::Client, target: &ProbeTarget) {
    let start = Instant::now();

    match target {
        ProbeTarget::Http { url } => {
            let result = client.get(url).send().await;
            let elapsed = start.elapsed().as_secs_f64();
            let status = match &result {
                Ok(response) => response.status().as_u16(),
                Err(_) => 0,
            };

            record_probe(target, ProbeOutcome::Http { status }, elapsed);

            match result {
                Ok(response) => info!(
                    "http probe {} -> {} ({:.3}s)",
                    url,
                    response.status(),
                    elapsed
                ),
                Err(err) => error!("http probe {} -> error: {err} ({:.3}s)", url, elapsed),
            }
        }
        ProbeTarget::Ping { address } => {
            let result = ping_address(address).await;
            let elapsed = start.elapsed().as_secs_f64();
            let success = matches!(&result, Ok(true));

            record_probe(target, ProbeOutcome::Ping { success }, elapsed);

            match result {
                Ok(true) => info!("ping probe {} -> success ({:.3}s)", address, elapsed),
                Ok(false) => info!("ping probe {} -> failure ({:.3}s)", address, elapsed),
                Err(err) => error!("ping probe {} -> error: {err} ({:.3}s)", address, elapsed),
            }
        }
    }
}

/// Probes every target concurrently, then waits for `interval` before repeating, forever.
async fn probe_loop(targets: Vec<ProbeTarget>, interval: Duration) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client");

    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;

        let mut set = JoinSet::new();
        for target in &targets {
            let client = client.clone();
            let target = target.clone();
            set.spawn(async move {
                probe_once(&client, &target).await;
            });
        }
        while set.join_next().await.is_some() {}
    }
}

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

    histogram!("handler", &labels).record(elapsed);

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

fn create_router(prometheus: PrometheusHandle) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(prometheus)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                .layer(middleware::from_fn(profile_endpoint)),
        )
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let args = Args::parse();
    let level: Level = args.log_level.clone().into();
    let subscriber = FmtSubscriber::builder().with_max_level(level).finish();

    tracing::subscriber::set_global_default(subscriber).unwrap();
    let prometheus = PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install recorder/exporter");
    describe_metrics();

    let interval = Duration::from_secs(args.interval);
    let targets = args.targets();
    let target_count = targets.len();
    tokio::spawn(probe_loop(targets, interval));

    let app = create_router(prometheus);

    let addr = format!("0.0.0.0:{}", args.port);
    let listener = TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|err| panic!("failed to bind {addr}: {err}"));

    info!(
        "uptime_exporter listening on http://{addr}/metrics, probing {} target(s) every {}s",
        target_count, args.interval
    );

    axum::serve(listener, app)
        .await
        .expect("metrics server error");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::with_local_recorder;
    use metrics_exporter_prometheus::PrometheusRecorder;
    use quickcheck::{Arbitrary, Gen};
    use quickcheck_macros::quickcheck;
    use std::future::Future;
    use std::str::FromStr;

    fn new_recorder() -> (PrometheusRecorder, PrometheusHandle) {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        (recorder, handle)
    }

    fn recorded<T>(f: impl FnOnce(PrometheusHandle) -> T) -> (String, T) {
        let (recorder, handle) = new_recorder();
        let result = with_local_recorder(&recorder, || {
            describe_metrics();
            f(handle.clone())
        });

        (handle.render(), result)
    }

    async fn recorded_async<T, Fut>(f: impl FnOnce(PrometheusHandle) -> Fut) -> (String, T)
    where
        Fut: Future<Output = T>,
    {
        let (recorder, handle) = new_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        describe_metrics();
        let result = f(handle.clone()).await;

        (handle.render(), result)
    }

    async fn test_server(status: u16) -> String {
        let app = Router::new()
            .fallback(move || async move { axum::http::StatusCode::from_u16(status).unwrap() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        format!("http://{addr}")
    }

    // ---------------------------------------------------------------------
    // ProbeKind / ProbeTarget / ProbeOutcome / is_http_probe_success
    // ---------------------------------------------------------------------

    #[test]
    fn probe_kind_labels_are_stable() {
        assert_eq!(ProbeKind::Http.as_label(), "http");
        assert_eq!(ProbeKind::Ping.as_label(), "ping");
    }

    #[test]
    fn probe_targets_expose_kind_and_label() {
        let http = ProbeTarget::http("https://example.com".to_string());
        let ping = ProbeTarget::ping("127.0.0.1".to_string());

        assert_eq!(http.kind(), ProbeKind::Http);
        assert_eq!(http.label(), "https://example.com");
        assert_eq!(ping.kind(), ProbeKind::Ping);
        assert_eq!(ping.label(), "127.0.0.1");
    }

    #[test]
    fn probe_outcome_success_depends_on_probe_kind() {
        assert!(ProbeOutcome::Http { status: 200 }.is_success());
        assert!(!ProbeOutcome::Http { status: 204 }.is_success());
        assert!(ProbeOutcome::Ping { success: true }.is_success());
        assert!(!ProbeOutcome::Ping { success: false }.is_success());
    }

    #[test]
    fn http_success_only_for_exact_200() {
        assert!(is_http_probe_success(200));
        assert!(!is_http_probe_success(0));
        assert!(!is_http_probe_success(199));
        assert!(!is_http_probe_success(201));
        assert!(!is_http_probe_success(301));
        assert!(!is_http_probe_success(404));
        assert!(!is_http_probe_success(500));
        assert!(!is_http_probe_success(u16::MAX));
    }

    #[quickcheck]
    fn prop_http_success_iff_status_is_exactly_200(status: u16) -> bool {
        is_http_probe_success(status) == (status == 200)
    }

    #[test]
    fn ping_args_use_one_packet_on_unix() {
        assert_eq!(ping_args("127.0.0.1"), ["-c", "1", "127.0.0.1"]);
    }

    // ---------------------------------------------------------------------
    // record_probe + metrics_handler
    // ---------------------------------------------------------------------

    fn http_metric(target: &str, metric: &str, value: impl std::fmt::Display) -> String {
        format!("{metric}{{probe_type=\"http\",target=\"{target}\"}} {value}")
    }

    fn ping_metric(target: &str, metric: &str, value: impl std::fmt::Display) -> String {
        format!("{metric}{{probe_type=\"ping\",target=\"{target}\"}} {value}")
    }

    /// Records a single HTTP outcome into a fresh recorder and returns the rendered body.
    fn rendered_http(url: &str, status: u16, elapsed: f64) -> String {
        let target = ProbeTarget::http(url.to_string());
        recorded(|_| record_probe(&target, ProbeOutcome::Http { status }, elapsed)).0
    }

    /// Records a single ping outcome into a fresh recorder and returns the rendered body.
    fn rendered_ping(address: &str, success: bool, elapsed: f64) -> String {
        let target = ProbeTarget::ping(address.to_string());
        recorded(|_| record_probe(&target, ProbeOutcome::Ping { success }, elapsed)).0
    }

    #[test]
    fn exposes_help_and_type_for_all_three_metrics() {
        let body = rendered_http("https://example.com", 200, 0.25);

        assert!(body.contains("# HELP probe_success"));
        assert!(body.contains("# TYPE probe_success gauge"));
        assert!(body.contains("# HELP probe_http_status_code"));
        assert!(body.contains("# TYPE probe_http_status_code gauge"));
        assert!(body.contains("# HELP probe_duration_seconds"));
        assert!(body.contains("# TYPE probe_duration_seconds gauge"));
    }

    #[test]
    fn record_200_is_success() {
        let body = rendered_http("https://example.com", 200, 0.25);
        assert!(body.contains(&http_metric("https://example.com", "probe_success", 1)));
        assert!(body.contains(&http_metric(
            "https://example.com",
            "probe_http_status_code",
            200
        )));
        assert!(body.contains(&http_metric(
            "https://example.com",
            "probe_duration_seconds",
            0.25
        )));
    }

    #[test]
    fn record_404_is_failure() {
        let body = rendered_http("https://example.com", 404, 0.1);
        assert!(body.contains(&http_metric("https://example.com", "probe_success", 0)));
        assert!(body.contains(&http_metric(
            "https://example.com",
            "probe_http_status_code",
            404
        )));
    }

    #[test]
    fn record_500_is_failure() {
        let body = rendered_http("https://example.com", 500, 0.1);
        assert!(body.contains(&http_metric("https://example.com", "probe_success", 0)));
        assert!(body.contains(&http_metric(
            "https://example.com",
            "probe_http_status_code",
            500
        )));
    }

    #[test]
    fn record_connection_failure_is_status_zero() {
        let body = rendered_http("https://unreachable.example", 0, 0.0);
        assert!(body.contains(&http_metric(
            "https://unreachable.example",
            "probe_success",
            0
        )));
        assert!(body.contains(&http_metric(
            "https://unreachable.example",
            "probe_http_status_code",
            0
        )));
    }

    #[test]
    fn record_ping_success_is_success() {
        let body = rendered_ping("127.0.0.1", true, 0.25);
        assert!(body.contains(&ping_metric("127.0.0.1", "probe_success", 1)));
        assert!(body.contains(&ping_metric("127.0.0.1", "probe_duration_seconds", 0.25)));
    }

    #[test]
    fn record_ping_failure_is_failure_without_http_status_series() {
        let body = rendered_ping("192.0.2.1", false, 0.1);
        assert!(body.contains(&ping_metric("192.0.2.1", "probe_success", 0)));
        assert!(!body.contains("probe_http_status_code{probe_type=\"ping\",target=\"192.0.2.1\"}"));
    }

    #[test]
    fn distinct_urls_get_distinct_label_series() {
        let a = ProbeTarget::http("https://a.example".to_string());
        let b = ProbeTarget::http("https://b.example".to_string());
        let body = recorded(|_| {
            record_probe(&a, ProbeOutcome::Http { status: 200 }, 0.1);
            record_probe(&b, ProbeOutcome::Http { status: 404 }, 0.2);
        })
        .0;
        assert!(body.contains(&http_metric("https://a.example", "probe_success", 1)));
        assert!(body.contains(&http_metric("https://b.example", "probe_success", 0)));
    }

    #[test]
    fn http_and_ping_targets_get_distinct_label_series() {
        let http = ProbeTarget::http("https://example.com".to_string());
        let ping = ProbeTarget::ping("example.com".to_string());
        let body = recorded(|_| {
            record_probe(&http, ProbeOutcome::Http { status: 200 }, 0.1);
            record_probe(&ping, ProbeOutcome::Ping { success: true }, 0.2);
        })
        .0;

        assert!(body.contains(&http_metric("https://example.com", "probe_success", 1)));
        assert!(body.contains(&ping_metric("example.com", "probe_success", 1)));
    }

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

    /// Characters that are safe to use unescaped inside a Prometheus label value
    /// and don't collide with clap's flag syntax when used as a CLI argument.
    const SAFE_ALPHABET: &[u8] =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789:/.-_";

    #[derive(Clone, Debug)]
    struct SafeLabel(String);

    impl Arbitrary for SafeLabel {
        fn arbitrary(g: &mut Gen) -> Self {
            let len = u8::arbitrary(g) as usize % 32;
            let s = (0..len)
                .map(|_| SAFE_ALPHABET[u8::arbitrary(g) as usize % SAFE_ALPHABET.len()] as char)
                .collect();
            SafeLabel(s)
        }
    }

    #[quickcheck]
    fn prop_record_roundtrips_through_render(
        label: SafeLabel,
        status: u16,
        elapsed_millis: u16,
    ) -> bool {
        let elapsed = elapsed_millis as f64 / 1000.0;
        let expected_success = if status == 200 { 1 } else { 0 };

        let body = rendered_http(&label.0, status, elapsed);

        body.contains(&format!(
            "probe_success{{probe_type=\"http\",target=\"{}\"}} {expected_success}",
            label.0
        )) && body.contains(&format!(
            "probe_http_status_code{{probe_type=\"http\",target=\"{}\"}} {status}",
            label.0
        ))
    }

    // ---------------------------------------------------------------------
    // CLI parsing (Args)
    // ---------------------------------------------------------------------

    #[test]
    fn defaults_are_30s_interval_and_9200_port() {
        let args = Args::try_parse_from(["uptime_exporter", "https://example.com"]).unwrap();
        assert_eq!(args.interval, 30);
        assert_eq!(args.port, 9200);
        assert_eq!(args.urls, vec!["https://example.com".to_string()]);
        assert!(args.ping_addresses.is_empty());
        assert!(matches!(args.log_level, LogLevel::Info));
    }

    #[test]
    fn parses_multiple_urls_and_custom_flags() {
        let args = Args::try_parse_from([
            "uptime_exporter",
            "--interval",
            "5",
            "--port",
            "8080",
            "--log-level",
            "debug",
            "https://a.example",
            "https://b.example",
        ])
        .unwrap();
        assert_eq!(args.interval, 5);
        assert_eq!(args.port, 8080);
        assert!(matches!(args.log_level, LogLevel::Debug));
        assert_eq!(
            args.urls,
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string()
            ]
        );
        assert!(args.ping_addresses.is_empty());
    }

    #[test]
    fn parses_ping_addresses_without_urls() {
        let args = Args::try_parse_from(["uptime_exporter", "--ping", "127.0.0.1"]).unwrap();
        assert!(args.urls.is_empty());
        assert_eq!(args.ping_addresses, vec!["127.0.0.1".to_string()]);
        assert_eq!(
            args.targets(),
            vec![ProbeTarget::ping("127.0.0.1".to_string())]
        );
    }

    #[test]
    fn parses_mixed_http_and_ping_targets() {
        let args = Args::try_parse_from([
            "uptime_exporter",
            "--ping",
            "127.0.0.1",
            "https://example.com",
            "--ping",
            "example.com",
        ])
        .unwrap();

        assert_eq!(args.urls, vec!["https://example.com".to_string()]);
        assert_eq!(
            args.ping_addresses,
            vec!["127.0.0.1".to_string(), "example.com".to_string()]
        );
        assert_eq!(
            args.targets(),
            vec![
                ProbeTarget::http("https://example.com".to_string()),
                ProbeTarget::ping("127.0.0.1".to_string()),
                ProbeTarget::ping("example.com".to_string()),
            ]
        );
    }

    #[test]
    fn rejects_missing_targets() {
        assert!(Args::try_parse_from(["uptime_exporter"]).is_err());
    }

    #[test]
    fn rejects_zero_interval() {
        let result =
            Args::try_parse_from(["uptime_exporter", "--interval", "0", "https://example.com"]);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_port_out_of_u16_range() {
        let result =
            Args::try_parse_from(["uptime_exporter", "--port", "70000", "https://example.com"]);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_garbage_interval() {
        let result = Args::try_parse_from([
            "uptime_exporter",
            "--interval",
            "not-a-number",
            "https://example.com",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_unknown_log_level() {
        let result = Args::try_parse_from([
            "uptime_exporter",
            "--log-level",
            "verbose",
            "https://example.com",
        ]);
        assert!(result.is_err());
    }

    const VALID_LEVELS: &[&str] = &["trace", "debug", "info", "warn", "error"];

    #[test]
    fn valid_log_levels_parse_successfully() {
        for &s in VALID_LEVELS {
            assert!(
                LogLevel::from_str(s).is_ok(),
                "expected '{s}' to parse as a valid LogLevel"
            );
        }
    }

    #[quickcheck]
    fn unknown_log_level_returns_err(s: String) -> bool {
        if VALID_LEVELS.contains(&s.as_str()) {
            return true;
        }

        LogLevel::from_str(&s).is_err()
    }

    #[quickcheck]
    fn log_level_from_is_total(raw: u8) -> bool {
        let level = match raw % 5 {
            0 => LogLevel::Trace,
            1 => LogLevel::Debug,
            2 => LogLevel::Info,
            3 => LogLevel::Warn,
            _ => LogLevel::Error,
        };
        let tracing_level: Level = level.into();

        matches!(
            tracing_level,
            Level::TRACE | Level::DEBUG | Level::INFO | Level::WARN | Level::ERROR
        )
    }

    const ARG_FIRST_CHAR_ALPHABET: &[u8] =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

    /// A non-empty string that never starts with '-', so clap always treats it as a
    /// positional value rather than an (unrecognized) flag.
    #[derive(Clone, Debug)]
    struct SafeArg(String);

    impl Arbitrary for SafeArg {
        fn arbitrary(g: &mut Gen) -> Self {
            let len = 1 + (u8::arbitrary(g) as usize % 16);
            let first = ARG_FIRST_CHAR_ALPHABET
                [u8::arbitrary(g) as usize % ARG_FIRST_CHAR_ALPHABET.len()]
                as char;
            let rest: String = (0..len - 1)
                .map(|_| SAFE_ALPHABET[u8::arbitrary(g) as usize % SAFE_ALPHABET.len()] as char)
                .collect();
            SafeArg(format!("{first}{rest}"))
        }
    }

    #[quickcheck]
    fn prop_cli_roundtrips_interval_port_and_urls(
        interval_offset: u32,
        port: u16,
        first_url: SafeArg,
        rest_urls: Vec<SafeArg>,
    ) -> bool {
        let interval = 1u64 + interval_offset as u64;

        let mut argv = vec![
            "uptime_exporter".to_string(),
            "--interval".to_string(),
            interval.to_string(),
            "--port".to_string(),
            port.to_string(),
            first_url.0.clone(),
        ];
        argv.extend(rest_urls.iter().map(|u| u.0.clone()));

        match Args::try_parse_from(&argv) {
            Ok(args) => {
                let mut expected_urls = vec![first_url.0.clone()];
                expected_urls.extend(rest_urls.iter().map(|u| u.0.clone()));
                let expected_targets = expected_urls
                    .iter()
                    .cloned()
                    .map(ProbeTarget::http)
                    .collect::<Vec<_>>();

                args.interval == interval
                    && args.port == port
                    && args.urls == expected_urls
                    && args.ping_addresses.is_empty()
                    && args.targets() == expected_targets
            }
            Err(_) => false,
        }
    }

    #[quickcheck]
    fn prop_cli_roundtrips_ping_addresses(
        first_address: SafeArg,
        rest_addresses: Vec<SafeArg>,
    ) -> bool {
        let mut argv = vec!["uptime_exporter".to_string()];
        argv.push("--ping".to_string());
        argv.push(first_address.0.clone());
        for address in &rest_addresses {
            argv.push("--ping".to_string());
            argv.push(address.0.clone());
        }

        match Args::try_parse_from(&argv) {
            Ok(args) => {
                let mut expected_addresses = vec![first_address.0.clone()];
                expected_addresses.extend(rest_addresses.iter().map(|address| address.0.clone()));
                let expected_targets = expected_addresses
                    .iter()
                    .cloned()
                    .map(ProbeTarget::ping)
                    .collect::<Vec<_>>();

                args.urls.is_empty()
                    && args.ping_addresses == expected_addresses
                    && args.targets() == expected_targets
            }
            Err(_) => false,
        }
    }

    // ---------------------------------------------------------------------
    // probe_once against a local mock HTTP server (no real network calls)
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn probe_once_records_200_as_success() {
        let client = reqwest::Client::new();
        let url = test_server(200).await;
        let target = ProbeTarget::http(url.clone());

        let body = recorded_async(|_| async {
            probe_once(&client, &target).await;
        })
        .await
        .0;
        assert!(body.contains(&http_metric(&url, "probe_success", 1)));
        assert!(body.contains(&http_metric(&url, "probe_http_status_code", 200)));
    }

    #[tokio::test]
    async fn probe_once_records_404_as_failure() {
        let client = reqwest::Client::new();
        let url = test_server(404).await;
        let target = ProbeTarget::http(url.clone());

        let body = recorded_async(|_| async {
            probe_once(&client, &target).await;
        })
        .await
        .0;
        assert!(body.contains(&http_metric(&url, "probe_success", 0)));
        assert!(body.contains(&http_metric(&url, "probe_http_status_code", 404)));
    }

    #[tokio::test]
    async fn probe_once_records_500_as_failure() {
        let client = reqwest::Client::new();
        let url = test_server(500).await;
        let target = ProbeTarget::http(url.clone());

        let body = recorded_async(|_| async {
            probe_once(&client, &target).await;
        })
        .await
        .0;
        assert!(body.contains(&http_metric(&url, "probe_success", 0)));
        assert!(body.contains(&http_metric(&url, "probe_http_status_code", 500)));
    }

    #[tokio::test]
    async fn probe_once_records_status_zero_on_connection_failure() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        // Port 0 is never a valid connection target, so this fails immediately and
        // deterministically without depending on any external network state.
        let url = "http://127.0.0.1:0/";
        let target = ProbeTarget::http(url.to_string());

        let body = recorded_async(|_| async {
            probe_once(&client, &target).await;
        })
        .await
        .0;
        assert!(body.contains(&http_metric(url, "probe_success", 0)));
        assert!(body.contains(&http_metric(url, "probe_http_status_code", 0)));
    }

    // ---------------------------------------------------------------------
    // probe_loop
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn probe_loop_probes_all_urls_concurrently_on_each_tick() {
        let server = test_server(200).await;
        let urls = vec![format!("{server}/a"), format!("{server}/b")];
        let targets = urls
            .iter()
            .cloned()
            .map(ProbeTarget::http)
            .collect::<Vec<_>>();

        let body = recorded_async(|_| async {
            let handle = tokio::spawn(probe_loop(targets.clone(), Duration::from_millis(20)));
            tokio::time::sleep(Duration::from_millis(150)).await;
            handle.abort();
        })
        .await
        .0;
        for url in &urls {
            assert!(body.contains(&http_metric(url, "probe_success", 1)));
        }
    }
}
