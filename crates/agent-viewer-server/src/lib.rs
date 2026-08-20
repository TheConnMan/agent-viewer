//! HTTP companion server for agent-viewer.

use axum::Router;

/// Constructs the server router.
#[must_use]
pub fn router() -> Router {
    Router::new()
}
