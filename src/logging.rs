use tracing::Level;
use tracing_subscriber::FmtSubscriber;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
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

pub fn init(log_level: LogLevel) {
    let level: Level = log_level.into();
    let subscriber = FmtSubscriber::builder().with_max_level(level).finish();

    tracing::subscriber::set_global_default(subscriber).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck_macros::quickcheck;
    use std::str::FromStr;

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
}
