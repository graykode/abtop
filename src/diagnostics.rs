use chrono::Local;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static LOGGER: OnceLock<Option<Logger>> = OnceLock::new();

struct Logger {
    file: Mutex<File>,
    level: LogLevel,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn from_env(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "error" => Self::Error,
            "warn" | "warning" => Self::Warn,
            "debug" => Self::Debug,
            "trace" => Self::Trace,
            _ => Self::Info,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }
}

pub fn init() {
    let _ = LOGGER.get_or_init(|| {
        let path = log_path()?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        let level = std::env::var("ABTOP_LOG_LEVEL")
            .map(|v| LogLevel::from_env(&v))
            .unwrap_or(LogLevel::Info);
        Some(Logger {
            file: Mutex::new(file),
            level,
        })
    });
}

pub(crate) fn write(level: &str, target: &str, message: String) {
    let Some(Some(logger)) = LOGGER.get() else {
        return;
    };
    let event_level = LogLevel::from_env(level);
    if event_level > logger.level {
        return;
    }
    let now = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
    if let Ok(mut file) = logger.file.lock() {
        let _ = writeln!(
            file,
            "{} {:<5} {:<24} {}",
            now,
            event_level.as_str(),
            target,
            message
        );
    }
}

fn log_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ABTOP_LOG_FILE") {
        let path = path.trim();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    let enabled = std::env::var("ABTOP_LOG")
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false);
    if !enabled {
        return None;
    }

    dirs::cache_dir()
        .or_else(dirs::home_dir)
        .map(|base| base.join("abtop").join("abtop.log"))
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::diagnostics::write("error", module_path!(), format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::diagnostics::write("warn", module_path!(), format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::diagnostics::write("info", module_path!(), format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::diagnostics::write("debug", module_path!(), format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_trace {
    ($($arg:tt)*) => {
        $crate::diagnostics::write("trace", module_path!(), format!($($arg)*))
    };
}
