//! Re-export of the shared PR-status cache. The implementation lives in `-core` so the
//! companion server can color phone badges the same way as the TUI.

pub use agent_viewer_core::pr_cache::{PrKey, PrStatusCache};
