use anyhow::Context;
use tracing_subscriber::{fmt, EnvFilter};

pub fn init_logging(debug: bool) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all("logs").context("create logs directory")?;
    let file_appender = tracing_appender::rolling::daily("logs", "borderless.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let filter = if debug { "debug" } else { "info" };

    fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_writer(non_blocking)
        .try_init()
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("initialize tracing subscriber")?;

    Ok(guard)
}
