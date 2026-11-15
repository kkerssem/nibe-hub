//! Settings and command-line handling shared by the collector and the MCP server.

use std::path::PathBuf;

/// Where the database lives when `DB_PATH` is not set.
pub const DEFAULT_DB_PATH: &str = "/data/nibe.db";

/// An environment variable, treating an empty or blank value as not set.
pub fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// The database path: `DB_PATH`, or [`DEFAULT_DB_PATH`].
pub fn db_path() -> PathBuf {
    env("DB_PATH").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DEFAULT_DB_PATH))
}

/// Whether the binary was started as `<binary> health`, for a container health check.
///
/// Any other argument is ignored with a warning on stderr rather than refused: a runtime that
/// sets the entrypoint may still append the image's CMD (`/nibe-collector`), and refusing it
/// would stop the service from starting.
pub fn health_requested() -> bool {
    health_arg(std::env::args().nth(1).as_deref())
}

fn health_arg(arg: Option<&str>) -> bool {
    match arg {
        Some("health") => true,
        Some(other) => {
            eprintln!("ignoring argument {other:?}; the only argument is `health`, for a health check");
            false
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn health_arg() {
        assert!(super::health_arg(Some("health")));
        assert!(!super::health_arg(None));
        // the image's CMD appended after an entrypoint must not stop the service
        assert!(!super::health_arg(Some("/nibe-collector")));
    }
}
