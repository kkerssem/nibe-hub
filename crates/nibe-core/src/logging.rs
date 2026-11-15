//! Log output shared by the collector and the MCP server.

use std::io::IsTerminal;

/// Sets up logging to stdout, filtered by `RUST_LOG` (default `info`).
///
/// - Colours only when stdout is a terminal and `NO_COLOR` is not set, so a container runtime
///   that stores the log (RouterOS, `container logs`) gets plain text instead of escape codes.
/// - `LOG_TIMESTAMPS=false` leaves out the timestamp, for a runtime that adds its own (RouterOS).
pub fn init() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(colours(std::io::stdout().is_terminal(), std::env::var_os("NO_COLOR").is_some()));
    if timestamps(crate::config::env("LOG_TIMESTAMPS").as_deref()) {
        builder.init();
    } else {
        builder.without_time().init();
    }
}

fn colours(is_terminal: bool, no_color: bool) -> bool {
    is_terminal && !no_color
}

/// On unless `LOG_TIMESTAMPS` is `false`, `0`, `no` or `off`.
fn timestamps(setting: Option<&str>) -> bool {
    !matches!(setting.map(|s| s.trim().to_ascii_lowercase()).as_deref(), Some("false" | "0" | "no" | "off"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn colours_only_on_a_terminal_without_no_color() {
        assert!(super::colours(true, false));
        assert!(!super::colours(false, false));
        assert!(!super::colours(true, true));
    }

    #[test]
    fn timestamps_on_unless_turned_off() {
        assert!(super::timestamps(None));
        assert!(super::timestamps(Some("true")));
        assert!(!super::timestamps(Some("false")));
        assert!(!super::timestamps(Some(" OFF ")));
        assert!(!super::timestamps(Some("0")));
    }
}
