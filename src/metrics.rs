use crate::probe::{ProbeOutcome, ProbeTarget};

pub fn describe_metrics() {
    ::metrics::describe_gauge!(
        "probe_success",
        "Whether the probe succeeded (HTTP 200 OK or ping response) (1) or not (0)"
    );
    ::metrics::describe_gauge!(
        "probe_http_status_code",
        "HTTP status code returned by an HTTP probe (0 if the request failed)"
    );
    ::metrics::describe_gauge!(
        "probe_duration_seconds",
        "Duration of the probe request in seconds"
    );
}

/// Records the outcome of a single probe for `target`.
pub fn record_probe(target: &ProbeTarget, outcome: ProbeOutcome, elapsed_secs: f64) {
    let labels = [
        ("probe_type", target.kind().as_label().to_string()),
        ("target", target.label().to_string()),
    ];
    let success = if outcome.is_success() { 1.0 } else { 0.0 };

    ::metrics::gauge!("probe_success", &labels).set(success);
    ::metrics::gauge!("probe_duration_seconds", &labels).set(elapsed_secs);

    if let ProbeOutcome::Http { status } = outcome {
        ::metrics::gauge!("probe_http_status_code", &labels).set(status as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck_macros::quickcheck;

    use crate::test_support::{SafeLabel, http_metric, ping_metric, recorded};

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
}
