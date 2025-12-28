// src/bin/worker.rs
use arachne::{CrawlResult, CrawlStatus, data_cleaning, get_domain};
use futures_util::StreamExt;
use rdkafka::ClientConfig;
use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};
use reqwest::{Client, header};
use scraper::{Html, Selector};
use std::collections::HashSet;
use std::env;
use tokio::time::Duration;
use url::{ParseError, Url};
use rdkafka::message::Message;

// --- MEMORY ALLOCATOR SETUP ---
#[cfg(not(target_os = "windows"))]
use tikv_jemallocator::Jemalloc;
#[cfg(not(target_os = "windows"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;
#[cfg(target_os = "windows")]
use mimalloc::MiMalloc;
#[cfg(target_os = "windows")]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const MAX_CONTENT_SIZE: usize = 100 * 1024 * 1024; // 100MB

#[derive(Debug, thiserror::Error)]
enum CrawlerError {
    #[error("Request error: {0}")]
    RequestError(#[from] reqwest::Error),
    #[error("URL parsing error: {0}")]
    UrlParseError(#[from] ParseError),
    #[error("Content too large")]
    ContentTooLarge,
    #[error("Invalid content type")]
    InvalidContentType,
}

async fn crawl_url(client: &Client, url_str: &str) -> Result<CrawlResult, CrawlerError> {
    let base_url = Url::parse(url_str)?;

    // 1. Send Request
    let response = client
        .get(base_url.clone())
        .timeout(Duration::from_secs(30))
        .send()
        .await?;

    if !response.status().is_success() {
        return Ok(CrawlResult {
            source_url: url_str.to_string(),
            status: CrawlStatus::HttpError(response.status().as_u16()),
            content: None,
            discovered_urls: vec![],
            domain: get_domain(&url_str.to_string()),
        });
    }

    // 2. Stream Download (Size Limit)
    let mut stream = response.bytes_stream();
    let mut body_bytes = Vec::new();
    while let Some(item) = stream.next().await {
        let chunk = item?;
        if body_bytes.len() + chunk.len() > MAX_CONTENT_SIZE {
            return Err(CrawlerError::ContentTooLarge);
        }
        body_bytes.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&body_bytes).to_string();

    // 3. Parse HTML
    let document = Html::parse_document(&body);

    // Extract Links (Required for crawling, even if not stored in DB)
    let selector = Selector::parse("a[href]").unwrap();
    let mut found_links = HashSet::new();
    for element in document.select(&selector) {
        if let Some(href) = element.value().attr("href") {
            if let Ok(mut new_url) = base_url.join(href) {
                new_url.set_fragment(None);
                if new_url.scheme().starts_with("http") {
                    found_links.insert(new_url.to_string());
                }
            }
        }
    }

    // Extract Tag Sequence (The "DNA")
    // Ensure your data_cleaning module returns ONLY the tag sequence string
    let tag_sequence = data_cleaning::extract_skeleton_from_doc(&document);

    Ok(CrawlResult {
        source_url: url_str.to_string(),
        status: CrawlStatus::Success,
        content: Some(tag_sequence),
        discovered_urls: found_links.into_iter().collect(),
        domain: Some(url_str.to_string()),
    })
}




#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let bootstrap_servers = env::var("KAFKA_SERVER").expect("KAFKA_SERVER not in .env");
    let consume_topic = "urls-to-crawl";
    let produce_topic = "crawl-results";
    let group_id = "arachne-worker-group";

    // --- Consumer ---
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", group_id)
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()
        .expect("Consumer creation failed");

    consumer
        .subscribe(&[consume_topic])
        .expect("Can't subscribe");

    // --- Producer ---
    // Note: queue.buffering.max.messages is low to prevent RAM explosion
    // when processing large MAX_CONTENT_SIZE files.
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("queue.buffering.max.messages", "50") // Low count because messages are huge
        .set("queue.buffering.max.kbytes", "512000") // 500MB Buffer limit
        .set("linger.ms", "100")
        .set("batch.size", "10485760") // 10MB Batch size
        .set("compression.type", "lz4")
        .set("message.max.bytes", "524288000") // Allow sending 100MB messages
        .set("acks", "1")
        .create()
        .expect("Producer creation failed");

    // --- Client (No Pooling) ---
    let http_client = Client::builder().pool_max_idle_per_host(0).build().unwrap();

    println!(
        "Worker started (Text-Only, {}MB Limit). Waiting for URLs...",
        MAX_CONTENT_SIZE / 1024 * 1024
    );

    loop {
        match consumer.recv().await {
            Err(e) => eprintln!("Kafka error: {}", e),
            Ok(m) => {
                let payload = match m.payload_view::<str>() {
                    Some(Ok(s)) => s.to_string(),
                    _ => continue,
                };

                println!("Crawling: {}", payload);

                let crawl_result = match crawl_url(&http_client, &payload).await {
                    Ok(result) => result,
                    Err(e) => {
                        // Log specific reasons for skipping
                        match &e {
                            CrawlerError::InvalidContentType => {
                                println!("SKIP (Type): {} ", payload)
                            }
                            CrawlerError::ContentTooLarge => {
                                println!("SKIP (Size): {} [>100MB]", payload)
                            }
                            _ => eprintln!("Error {}: {}", payload, e),
                        }

                        // Return a failure result so we track it in DB
                        CrawlResult {
                            source_url: payload.clone(),
                            status: CrawlStatus::FetchError(e.to_string()),
                            content: None,
                            discovered_urls: vec![],
                            domain: Some(payload.clone()),
                        }
                    }
                };

                let result_json =
                    serde_json::to_string(&crawl_result).expect("JSON Serialization failed");

                // --- BACKPRESSURE LOOP ---
                loop {
                    let record = FutureRecord::to(produce_topic)
                        .key(&payload)
                        .payload(&result_json);

                    // We do not clone 'record'. We hand it over to 'send'.
                    match producer.send(record, Duration::from_secs(0)).await {
                        Ok(_) => break, // Success! Exit loop.
                        Err((e, _)) => {
                            // The error tuple returns (KafkaError, OriginalRecord)
                            // But since we re-create the record at the top of the loop,
                            // we can just ignore the returned record and wait.
                            eprintln!("Queue full ({}). Waiting...", e);
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                }

                // Commit offset
                if let Err(e) = consumer.commit_message(&m, CommitMode::Async) {
                    eprintln!("Commit failed: {}", e);
                }
            }
        }
    }
}
