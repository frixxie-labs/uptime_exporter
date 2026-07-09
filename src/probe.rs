use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::task::JoinSet;
use tracing::{error, info, instrument};

use crate::metrics::record_probe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeKind {
    Http,
    Ping,
}

impl ProbeKind {
    pub fn as_label(self) -> &'static str {
        match self {
            ProbeKind::Http => "http",
            ProbeKind::Ping => "ping",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeTarget {
    Http { url: String },
    Ping { address: String },
}

impl ProbeTarget {
    pub fn http(url: String) -> Self {
        ProbeTarget::Http { url }
    }

    pub fn ping(address: String) -> Self {
        ProbeTarget::Ping { address }
    }

    pub fn kind(&self) -> ProbeKind {
        match self {
            ProbeTarget::Http { .. } => ProbeKind::Http,
            ProbeTarget::Ping { .. } => ProbeKind::Ping,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            ProbeTarget::Http { url } => url,
            ProbeTarget::Ping { address } => address,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    Http { status: u16 },
    Ping { success: bool },
}

impl ProbeOutcome {
    pub fn is_success(self) -> bool {
        match self {
            ProbeOutcome::Http { status } => is_http_probe_success(status),
            ProbeOutcome::Ping { success } => success,
        }
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
pub async fn probe_loop(targets: Vec<ProbeTarget>, interval: Duration) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck_macros::quickcheck;

    use crate::test_support::{http_metric, recorded_async, test_server};

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
