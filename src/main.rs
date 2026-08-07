pub mod cli;
pub mod logging;
pub mod metrics;
pub mod probe;
pub mod server;

#[cfg(test)]
mod test_support;

use clap::Parser;
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::net::TcpListener;
use tracing::info;

use crate::cli::Args;
use crate::metrics::describe_metrics;
use crate::probe::probe_loop;
use crate::server::create_router;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let args = Args::parse();

    logging::init(args.log_level());
    let prometheus = PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install recorder/exporter");
    describe_metrics();

    let interval = args.interval();
    let targets = args.targets();
    let target_count = targets.len();
    tokio::spawn(probe_loop(targets, interval));

    let app = create_router(prometheus);

    let addr = format!("0.0.0.0:{}", args.port());
    let listener = TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|err| panic!("failed to bind {addr}: {err}"));

    info!(
        address = %addr,
        target_count,
        interval_seconds = args.interval_secs(),
        "uptime_exporter started"
    );

    axum::serve(listener, app)
        .await
        .expect("metrics server error");

    Ok(())
}
