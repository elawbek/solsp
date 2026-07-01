//! Optional performance tracing for LSP hot paths.

use std::sync::OnceLock;
use std::time::Instant;

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("SOLSP_TRACE_TIMING")
            .map(|value| !matches!(value.as_str(), "" | "0" | "false" | "off"))
            .unwrap_or(false)
    })
}

pub(crate) fn log_elapsed(label: impl FnOnce() -> String, started: Instant) {
    if enabled() {
        eprintln!("solsp: {} in {:?}", label(), started.elapsed());
    }
}
