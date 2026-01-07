//lib.rs
use anyhow::Result;
use chrono::{DateTime, NaiveDateTime, Utc};
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use serde::{Deserialize, Serialize};
use std::env;
pub mod data_cleaning;
pub mod db;
use wreq::{Client, ClientBuilder};
use wreq_util::Emulation;
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlResult {
    pub source_url: String,
    pub status: CrawlStatus,
    pub domain: Option<String>,
    pub content: Option<String>,
    pub discovered_urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CrawlStatus {
    Success,
    HttpError(u16),
    FetchError(String),
    InvalidContentType,
}

impl CrawlStatus {
    /// Converts the status to an integer for database storage.
    /// Returns 200 for Success, the actual code for HttpError,
    /// and 0 for FetchError (DNS/Network issues).
    /// 1000 => Invalid content type
    pub fn as_i32(&self) -> i32 {
        match self {
            CrawlStatus::Success => 200,
            CrawlStatus::HttpError(code) => *code as i32,
            CrawlStatus::FetchError(_) => 0,
            CrawlStatus::InvalidContentType => 1000,
        }
    }
}

pub fn client() {
    let http_client = Client::builder()
        .emulation(Emulation::Chrome137)
        .build()
        .unwrap();
}

pub fn get_domain(url_str: &str) -> Option<String> {
    Url::parse(url_str).ok().and_then(|u| u.domain().map(|d| d.to_string()))
}