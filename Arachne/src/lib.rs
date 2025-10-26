use serde::{Deserialize, Serialize};
use anyhow::Result;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use std::env;
use chrono::{DateTime, Utc, NaiveDateTime};
pub mod db;

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