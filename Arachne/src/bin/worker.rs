// src/bin/worker.rs
use arachne::{CrawlResult, CrawlStatus, data_cleaning, get_domain};
use futures_util::{StreamExt, TryStreamExt};
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
use rdkafka::message::{OwnedMessage, ToBytes};

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

    // --- CONFIGURATION ---
    let bootstrap_servers = env::var("KAFKA_SERVER").expect("KAFKA_SERVER not in .env");
    let consume_topic = "urls-to-crawl";
    let produce_topic = "crawl-results";
    let group_id = "arachne-worker-group";

    // Concurrency Limit
    const PARALLEL_REQUESTS: usize = 1000;

    // --- KAFKA CONSUMER ---
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", group_id)
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "true")
        .set("auto.commit.interval.ms", "5000")
        .set("fetch.min.bytes", "1000000")
        .create()
        .expect("Consumer creation failed");

    consumer.subscribe(&[consume_topic]).expect("Can't subscribe");

    // --- KAFKA PRODUCER ---
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("queue.buffering.max.messages", "2000")
        .set("message.max.bytes", "524288000")
        .set("compression.type", "lz4")
        .set("acks", "1")
        .create()
        .expect("Producer creation failed");

    // --- OPTIMIZED HTTP CLIENT ---
    let http_client = Client::builder()
        .trust_dns(true) // Key for performance
        .tcp_keepalive(None)
        .pool_idle_timeout(Duration::from_secs(0))
        .pool_max_idle_per_host(0)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent("ArachneBot/1.0")
        .build()
        .unwrap();

    println!("=================================================");
    println!("🕷️  ARACHNE WORKER STARTED");
    println!("=================================================");
    println!("TARGET:     {}", bootstrap_servers);
    println!("THREADS:    {}", PARALLEL_REQUESTS);
    println!("DNS:        Internal (Trust-DNS)");
    println!("LOGGING:    ENABLED");
    println!("=================================================");

    let stream_processor = consumer.stream()
        .map(|result| {
            match result {
                Ok(msg) => msg,
                Err(e) => {
                    eprintln!("⚠️  Kafka Recv Error: {}", e);
                    panic!("Kafka connection lost"); // Panic to restart if connection dies
                }
            }
        })
        .map(|borrowed_msg| borrowed_msg.detach())
        .map(|owned_msg: OwnedMessage| {
            let producer = producer.clone();
            let http_client = http_client.clone();

            async move {
                let payload = match owned_msg.payload_view::<str>() {
                    Some(Ok(s)) => s.to_string(),
                    _ => return,
                };

                // LOG: Start
                // (Optional: Comment this out if it's too fast to read)
                // println!("..  Fetching: {}", payload);

                let crawl_result = match crawl_url(&http_client, &payload).await {
                    Ok(res) => {
                        // LOG: Success
                        // We check the status inside the result to see if it was a 200 or 404
                        match res.status {
                            CrawlStatus::Success => {
                                let size_kb = res.content.as_ref().map(|s| s.len()).unwrap_or(0) / 1024;
                                println!("✅  OK   [{}KB] {}", size_kb, payload);
                            }
                            CrawlStatus::HttpError(code) => {
                                println!("⚠️  HTTP [{}]   {}", code, payload);
                            }
                            _ => println!("✅  DONE        {}", payload),
                        }
                        res
                    },
                    Err(e) => {
                        // LOG: Failure
                        match e {
                             CrawlerError::RequestError(ref e) if e.is_timeout() => {
                                 println!("⏳  TIMEOUT     {}", payload);
                             },
                             CrawlerError::ContentTooLarge => {
                                 println!("📦  TOO BIG     {}", payload);
                             },
                             _ => {
                                 eprintln!("❌  ERR  [{}] {}", e, payload);
                             }
                        }

                        // Return error result for DB tracking
                        CrawlResult {
                            source_url: payload.clone(),
                            status: CrawlStatus::FetchError(e.to_string()),
                            content: None,
                            discovered_urls: vec![],
                            domain: Some(payload.clone()),
                        }
                    }
                };

                let result_json = serde_json::to_string(&crawl_result).unwrap_or_default();

                // Send to Kafka
                loop {
                    let record = FutureRecord::to(produce_topic)
                        .key(&payload)
                        .payload(&result_json);

                    match producer.send(record, Duration::from_secs(0)).await {
                        Ok(_) => break, // Success
                        Err(_) => {
                            eprintln!("full... waiting");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    }
                }
            }
        })
        .buffer_unordered(PARALLEL_REQUESTS);

    // Drive the stream
    stream_processor.for_each(|_| async {}).await;
}