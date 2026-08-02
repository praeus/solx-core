use std::sync::Arc;

use solx_manager::App;

/// Shared state threaded through the bearer-auth middleware and every
/// route handler.
#[derive(Clone)]
pub struct AppState {
    pub app: Arc<App>,
    pub token: Arc<str>,
}
