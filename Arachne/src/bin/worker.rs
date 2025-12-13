use rdkafka::consumer::{Consumer, StreamConsumer, CommitMode};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::message::Message;
use rdkafka::ClientConfig;
use tokio::time::Duration;
use tokio::sync::Semaphore; // Import Semaphore
use arachne::{CrawlResult, CrawlStatus};
use std::env;
use std::sync::Arc;
use reqwest;
use wreq::{Client, ClientBuilder};
use wreq_util::Emulation;
use scraper::{Html, Selector};
use std::collections::HashSet;
use url::{ParseError, Url};

#[derive(Debug, thiserror::Error)]
enum CrawlerError {
    #[error("Request error: {0}")]
    RequestError(#[from] wreq::Error),
//    RequestError(#[from] rquest::Error),
    #[error("URL parsing error: {0}")]
    UrlParseError(#[from] ParseError),
}

/// (Kept your original logic, just ensuring it's efficient)
async fn crawl_url(client: &Client, url_str: &str) -> Result<CrawlResult, CrawlerError> {
    let base_url = Url::parse(url_str)?;

    // Set a timeout for the request so slow sites don't hang a worker slot forever
    let response = client.get(base_url.clone())
        .timeout(Duration::from_secs(10))
        .send().await?;

    if !response.status().is_success() {
        return Ok(CrawlResult {
            source_url: url_str.to_string(),
            status: CrawlStatus::HttpError(response.status().as_u16()),
            content: None,
            discovered_urls: vec![],
        });
    }

    let body = response.text().await.unwrap();

    // Parsing happens here (CPU bound task)
    // In a massive scale system, we might spawn_blocking here, but for now this is fine.
    let document = Html::parse_document(&body);
    let selector = Selector::parse("a[href]").unwrap();

    let mut found_links = HashSet::new();

    for element in document.select(&selector) {
        if let Some(href) = element.value().attr("href") {
            if let Ok(mut new_url) = base_url.join(href) {
                new_url.set_fragment(None);
                found_links.insert(new_url.to_string());
            }
        }
    }

    Ok(CrawlResult {
        source_url: url_str.to_string(),
        status: CrawlStatus::Success,
        content: Some(body),
        discovered_urls: found_links.into_iter().collect(),
    })
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    // --- Configuration ---
    let bootstrap_servers = env::var("KAFKA_SERVER").expect("KAFKA_SERVER not in .env");
    let consume_topic = "urls-to-crawl";
    let produce_topic = "crawl-results";
    let group_id = "arachne-worker-group";

    // --- CONCURRENCY CONTROL ---
    // This allows 50 URLs to be processed at the EXACT same time.
    // Increase this if you have more RAM/CPU, decrease if you get banned.
    let max_concurrent_crawls = 50;
    let semaphore = Arc::new(Semaphore::new(max_concurrent_crawls));

    // --- Create Kafka Consumer ---
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", group_id)
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "true") // We switch to auto-commit for speed/simplicity in parallel mode
        .set("auto.commit.interval.ms", "5000")
        .create()
        .expect("Consumer creation failed");

    consumer.subscribe(&[consume_topic]).expect("Can't subscribe");

    // --- Create Kafka Producer ---
    // Producers are thread-safe and cheap to clone
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("queue.buffering.max.messages", "100000")
        .set("linger.ms", "100")
        .set("batch.size", "1048576")
        .set("compression.type", "lz4")
        .set("message.max.bytes", "10485760") // 10MB limit support
        .set("acks", "1")
        .create()
        .expect("Producer creation failed");

    // --- Create HTTP Client ---
    // Clients are thread-safe and cheap to clone
        let http_client = Client::builder()
        .emulation(Emulation::Chrome137)
        .build().unwrap();

/*    let http_client = Client::builder()
        //.user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .user_agent("Arachne/1.0")
        //.timeout(Duration::from_secs(15))
        // 3. Handle Redirects
        // Many sites redirect http -> https or www -> non-www. Follow them.
        //.redirect(reqwest::redirect::Policy::limited(5))
        // 4. SSL/TLS Lenience
        // Some older sites have expired certs. We still want to crawl them.
        //.danger_accept_invalid_certs(true)// Time just to establish the TCP connection
        //.connect_timeout(Duration::from_secs(8)) // Be polite, identify yourself
        //.connect_timeout(Duration::from_secs(5))
        .build()
        .unwrap();
*/
    println!("Worker started (Concurrent Mode: {} slots). Waiting for URLs...", max_concurrent_crawls);

    // --- High Speed Loop ---
    loop {
        // 1. Acquire a permit.
        // If 50 tasks are running, this line WAITS until one finishes.
        let permit = semaphore.clone().acquire_owned().await.unwrap();

        match consumer.recv().await {
            Err(e) => {
                eprintln!("Kafka error: {}", e);
                // If we errored reading from Kafka, we don't need the permit.
                drop(permit);
            }
            Ok(m) => {
                // 2. Extract Data immediately
                // We must extract the string NOW because we can't pass the borrowed message to a thread.
                let payload = match m.payload_view::<str>() {
                    Some(Ok(s)) => s.to_string(), // Clone to String
                    _ => { drop(permit); continue; }
                };

                // 3. Clone required resources for the task
                let producer_clone = producer.clone();
                let client_clone = http_client.clone();
                let produce_topic = produce_topic.to_string(); // Clone string for the thread

                // 4. SPAWN THE TASK
                // This block runs in the background. The main loop immediately goes back to get the next URL.
                tokio::spawn(async move {
                    // The permit is moved here. It will be dropped (and released) when this block ends.
                    let _permit = permit;

                    // println!("Crawling: {}", payload); // Optional: Comment out for max speed

                    let crawl_result = match crawl_url(&client_clone, &payload).await {
                        Ok(res) => res,
                        Err(e) => {
                            eprintln!("Failed {}: {}", payload, e);
                            CrawlResult {
                                source_url: payload.clone(),
                                status: CrawlStatus::FetchError(e.to_string()),
                                content: None,
                                discovered_urls: vec![],
                            }
                        }
                    };

                    // Handle Message Size issues (Lite version fallback)
                    let result_json = serde_json::to_string(&crawl_result).unwrap();
                    let record = FutureRecord::to(&produce_topic)
                        .key(&payload)
                        .payload(&result_json);

                    match producer_clone.send(record, Duration::from_secs(0)).await {
                        Ok(_) => {
                            // Success
                        },
                        Err((e, _)) => {
                            if e.to_string().contains("Message size too large") {
                                eprintln!("⚠️ Too large: {}. Sending lite version.", payload);
                                let mut lite = crawl_result.clone();
                                lite.content = Some("<<CONTENT_TOO_LARGE>>".to_string());
                                let lite_json = serde_json::to_string(&lite).unwrap();
                                let _ = producer_clone.send(
                                    FutureRecord::to(&produce_topic).key(&payload).payload(&lite_json),
                                    Duration::from_secs(0)
                                ).await;
                            } else {
                                eprintln!("Producer error for {}: {}", payload, e);
                            }
                        }
                    }
                });
            }
        }
    }
}