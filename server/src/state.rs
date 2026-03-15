use std::sync::Arc;

use reqwest::Client;
use tokio::sync::Semaphore;

use crate::{cache::CacheStore, config::Config, image_cache::ImageCacheStore};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub client: Client,
    pub cache: Arc<dyn CacheStore>,
    pub image_cache: Arc<dyn ImageCacheStore>,
    pub api_miss_limiter: Arc<Semaphore>,
    pub image_miss_limiter: Arc<Semaphore>,
}
