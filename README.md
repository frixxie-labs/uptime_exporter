# uptime_exporter

`uptime_exporter` probes HTTP URLs and ICMP ping targets on a fixed interval, then exposes the latest probe results as Prometheus metrics.

## Features

- HTTP GET probes for one or more URLs.
- ICMP ping probes for one or more addresses.
- Concurrent probing across all configured targets.
- Prometheus metrics served from `/metrics`.
- Docker image support, including `ping` and CA certificates in the runtime image.
- Structured JSON logs suitable for VictoriaLogs ingestion.

## Metrics

All probe metrics include these labels:

- `target`: the configured URL or ping address.
- `probe_type`: `http` or `ping`.

| Metric | Type | Description |
| --- | --- | --- |
| `probe_success` | gauge | `1` when the latest probe succeeded, otherwise `0`. HTTP probes only count an exact final `200 OK` as success. Ping probes count a successful ping response as success. |
| `probe_duration_seconds` | gauge | Duration of the latest probe in seconds. |
| `probe_http_status_code` | gauge | HTTP status code from the latest HTTP probe. `0` means the HTTP request failed before receiving a response. This metric is only emitted for HTTP targets. |

The exporter also emits the Prometheus recorder/runtime metrics produced by `metrics-exporter-prometheus`.

## Run Locally

```sh
cargo run -- https://example.com https://status.example.com --ping 1.1.1.1
```

By default, the exporter probes every 30 seconds and serves metrics on port `9200`:

```sh
curl http://localhost:9200/metrics
```

Example output:

```prometheus
probe_success{probe_type="http",target="https://example.com"} 1
probe_duration_seconds{probe_type="http",target="https://example.com"} 0.123
probe_http_status_code{probe_type="http",target="https://example.com"} 200
probe_success{probe_type="ping",target="1.1.1.1"} 1
probe_duration_seconds{probe_type="ping",target="1.1.1.1"} 0.021
```

## Configuration

Targets are configured as command-line arguments:

```sh
uptime_exporter [OPTIONS] [URL]... --ping <ADDRESS> --ping <ADDRESS>
```

At least one URL or ping address is required.

| Option | Environment Variable | Default | Description |
| --- | --- | --- | --- |
| `--interval <SECONDS>` | `UPTIME_EXPORTER_INTERVAL` | `30` | How often each target is probed. Must be at least `1`. |
| `--port <PORT>` | `UPTIME_EXPORTER_PORT` | `9200` | Port used for the `/metrics` endpoint. |
| `--log-level <LEVEL>` | `UPTIME_EXPORTER_LOG_LEVEL` | `info` | Application log level. |
| `--ping <ADDRESS>` | | | Address to probe with ICMP ping. May be repeated. |

HTTP URLs are positional arguments and may also be repeated.

## Logging

Logs are written to standard output as newline-delimited JSON. To ingest them through VictoriaLogs' JSON stream API, configure the collector to send each line to:

```text
http://victorialogs:9428/insert/jsonline?_time_field=timestamp&_msg_field=message
```

Probe logs include `probe_type`, `target`, `success`, and `duration_seconds` fields. HTTP request logs include `method`, `path`, `status_code`, and `duration_milliseconds` fields.

## Docker

Build the image:

```sh
docker build -t uptime_exporter .
```

Run the exporter:

```sh
docker run --rm -p 9200:9200 uptime_exporter \
  https://example.com \
  --ping 1.1.1.1
```

Depending on the container runtime and host settings, ICMP ping may require additional permissions:

```sh
docker run --rm --cap-add NET_RAW -p 9200:9200 uptime_exporter --ping 1.1.1.1
```

## Prometheus Scrape Config

```yaml
scrape_configs:
  - job_name: uptime_exporter
    static_configs:
      - targets:
          - localhost:9200
```

## Grafana

Import `dashboards/uptime-exporter.json` into Grafana and select a Prometheus data source. The dashboard includes panels for:

- current target health,
- success rate by target,
- failures by target,
- probe latency,
- HTTP status codes,
- current failing targets.

The dashboard uses `$target` and `$probe_type` variables so it can filter down to a specific monitored endpoint or probe kind.

## Development

Run the test suite:

```sh
cargo test --workspace
```

Build the project:

```sh
cargo build --workspace
```
