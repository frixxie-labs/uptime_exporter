use std::time::Duration;

use clap::{ArgGroup, Parser};

use crate::logging::LogLevel;
use crate::probe::ProbeTarget;

/// Probes HTTP URLs and ping addresses on an interval and exposes the results as Prometheus metrics.
#[derive(Parser, Debug)]
#[command(name = "uptime_exporter", version, about)]
#[command(group(
    ArgGroup::new("targets")
        .required(true)
        .multiple(true)
        .args(["urls", "ping_addresses"])
))]
pub struct Args {
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
    pub fn targets(&self) -> Vec<ProbeTarget> {
        self.urls
            .iter()
            .cloned()
            .map(ProbeTarget::http)
            .chain(self.ping_addresses.iter().cloned().map(ProbeTarget::ping))
            .collect()
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval)
    }

    pub fn interval_secs(&self) -> u64 {
        self.interval
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn log_level(&self) -> LogLevel {
        self.log_level
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use quickcheck::{Arbitrary, Gen};
    use quickcheck_macros::quickcheck;

    use crate::test_support::SAFE_ALPHABET;

    #[test]
    fn defaults_are_30s_interval_and_9200_port() {
        let args = Args::try_parse_from(["uptime_exporter", "https://example.com"]).unwrap();
        assert_eq!(args.interval_secs(), 30);
        assert_eq!(args.interval(), Duration::from_secs(30));
        assert_eq!(args.port(), 9200);
        assert_eq!(args.urls, vec!["https://example.com".to_string()]);
        assert!(args.ping_addresses.is_empty());
        assert!(matches!(args.log_level(), LogLevel::Info));
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
        assert_eq!(args.interval_secs(), 5);
        assert_eq!(args.interval(), Duration::from_secs(5));
        assert_eq!(args.port(), 8080);
        assert!(matches!(args.log_level(), LogLevel::Debug));
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

    const ARG_FIRST_CHAR_ALPHABET: &[u8] =
        b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

    /// A non-empty string that never starts with '-', so clap always treats it as a
    /// positional value rather than an unrecognized flag.
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

                args.interval_secs() == interval
                    && args.port() == port
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
}
