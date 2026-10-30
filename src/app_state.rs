use std::{collections::HashMap, sync::Arc};

use tokio::sync::RwLock;

use crate::{
    browser::pool::ObscuraPool,
    chat_service::ChatService,
    config::AppConfig,
    memory::UnifiedMemory,
    relay::RelayState,
    router::ProviderRouter,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthKeyContext {
    /// Stable configured key name; never the raw bearer secret.
    pub(crate) key_id: String,
    pub(crate) provider_allowlist: Option<Vec<String>>,
}

pub(crate) struct AppState {
    pub(crate) cfg: Arc<AppConfig>,
    pub(crate) pool: Arc<ObscuraPool>,
    pub(crate) chat_service: ChatService,
    pub(crate) memory: Arc<UnifiedMemory>,
    pub(crate) router: ProviderRouter,
    pub(crate) models_cache: Arc<RwLock<HashMap<String, Vec<String>>>>,
    pub(crate) relay: Arc<RelayState>,
}
