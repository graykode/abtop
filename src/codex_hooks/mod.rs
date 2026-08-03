//! Native Codex hook integration.
//!
//! Public setup commands install an isolated local plugin. Runtime hook input
//! is consumed by a private, silent entry point and reduced to content-free
//! state for the Codex collector.

pub(crate) mod ingest;
pub(crate) mod migration;
pub(crate) mod plugin;
pub(crate) mod state;

use std::ffi::OsString;

/// Consume one hook invocation without ever affecting Codex.
///
/// The generated launcher already redirects both output streams. This second
/// fail-open boundary is intentional: malformed, stale, or untrusted input is
/// ignored and represented by unavailable evidence instead of delaying the
/// provider.
pub(crate) fn ingest_silently(args: Vec<OsString>) {
    let _ = ingest::run_from_environment(args);
}
