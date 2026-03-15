use std::sync::Arc;

use reqwest::Client;

use crate::{cache::CacheStore, config::Config};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub client: Client,
    pub cache: Arc<dyn CacheStore>,
}
