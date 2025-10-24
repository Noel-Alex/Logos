use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct CrawlResult {
    pub source_url: String,
    pub status: CrawlStatus,
    pub content: Option<String>,
    pub discovered_urls: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum CrawlStatus {
    Success,
    HttpError(u16),
    FetchError(String),
}