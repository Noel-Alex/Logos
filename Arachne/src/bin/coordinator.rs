// src/bin/coordinator.rs
use arachne::{CrawlResult, db};
use rdkafka::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::Message;
use rdkafka::producer::{FutureProducer, FutureRecord};
use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::Arc;
use tokio::sync::{Semaphore, mpsc};
use tokio::time::{self, Duration, Instant};
use url::Url;

// Hyperparameters
const MAX_PAGES_PER_SITE: i64 = 500;
const BATCH_SIZE: usize = 1000;
const BATCH_TIMEOUT: Duration = Duration::from_millis(500);

struct WorkItem {
    result: CrawlResult,
}

/// Helper to extract "example.com" from "https://example.com/foo"
fn get_domain(url_str: &str) -> Option<String> {
    Url::parse(url_str)
        .ok()
        .and_then(|u| u.domain().map(|d| d.to_string()))
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    println!("Coordinator initializing...");

    // 1. Setup Database Repository
    // The repo handles all connections and prepared statements internally.
    let db_repo = Arc::new(db::ArachneRepo::new().await.expect("DB Initialization failed"));

    // 2. Setup Kafka (Producer & Consumer)
    let bootstrap_servers = env::var("KAFKA_SERVER").expect("KAFKA_SERVER missing");

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("queue.buffering.max.messages", "100000")
        .set("acks", "1")
        .create()
        .expect("Producer failed");

    let (tx, mut rx) = mpsc::channel::<WorkItem>(10_000);

    // Kafka Consumer Task
    let bs_clone = bootstrap_servers.clone();
    tokio::spawn(async move {
        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &bs_clone)
            .set("group.id", "arachne-coordinator")
            .set("enable.auto.commit", "true")
            .set("auto.offset.reset", "earliest")
            .create()
            .expect("Consumer failed");
        consumer.subscribe(&["crawl-results"]).unwrap();
        println!("Coordinator listening on Kafka...");

        loop {
            if let Ok(m) = consumer.recv().await {
                if let Some(Ok(payload)) = m.payload_view::<str>() {
                    if let Ok(result) = serde_json::from_str::<CrawlResult>(payload) {
                        let _ = tx.send(WorkItem { result }).await;
                    }
                }
            }
        }
    });

    // 3. Processing Loop
    let mut buffer: Vec<WorkItem> = Vec::with_capacity(BATCH_SIZE);
    let mut last_flush = Instant::now();
    let semaphore = Arc::new(Semaphore::new(20)); // Max concurrent DB batches

    println!("Coordinator Loop Started.");

    loop {
        // A. Fill Buffer
        let fill_buffer = async {
            while buffer.len() < BATCH_SIZE {
                match time::timeout(Duration::from_millis(50), rx.recv()).await {
                    Ok(Some(item)) => buffer.push(item),
                    _ => break,
                }
            }
        };
        fill_buffer.await;

        let should_flush = !buffer.is_empty()
            && (buffer.len() >= BATCH_SIZE || last_flush.elapsed() > BATCH_TIMEOUT);

        if should_flush {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let batch: Vec<WorkItem> = std::mem::replace(&mut buffer, Vec::with_capacity(BATCH_SIZE));

            // Clone Arcs for async task
            let db_repo = db_repo.clone();
            let producer = producer.clone();

            tokio::spawn(async move {
                let _permit = permit;

                // 1. Group by Domain & Filter Links
                let mut pages_by_domain: HashMap<String, Vec<CrawlResult>> = HashMap::new();
                let mut domains_to_check = HashSet::new();

                for item in batch {
                    if let Some(source_domain) = get_domain(&item.result.source_url) {
                        // Strict Filtering: Only keep links from same domain
                        let internal_links: Vec<String> = item.result.discovered_urls
                            .into_iter()
                            .filter(|link| {
                                get_domain(link).map_or(false, |d| d == source_domain)
                            })
                            .collect();

                        let filtered_result = CrawlResult {
                            discovered_urls: internal_links,
                            ..item.result
                        };

                        domains_to_check.insert(source_domain.clone());
                        pages_by_domain.entry(source_domain).or_default().push(filtered_result);
                    }
                }

                // 2. CALL DB: Check Counts
                let current_counts = db_repo.get_domain_counts(domains_to_check.into_iter().collect())
                    .await
                    .unwrap_or_default();

                // 3. Prepare Logic
                let mut pages_to_insert = Vec::new();
                let mut increments: HashMap<String, i64> = HashMap::new();
                let mut candidate_new_urls: HashSet<(String, String)> = HashSet::new();

                for (domain, pages) in pages_by_domain {
                    let existing_count = *current_counts.get(&domain).unwrap_or(&0);
                    let mut accepted_count = 0;

                    for page in pages {
                        if existing_count + accepted_count < MAX_PAGES_PER_SITE {
                            for url in &page.discovered_urls {
                                candidate_new_urls.insert((domain.clone(), url.clone()));
                            }
                            pages_to_insert.push((domain.clone(), page));
                            accepted_count += 1;
                        }
                    }

                    if accepted_count > 0 {
                        increments.insert(domain, accepted_count);
                    }
                }

                // 4. CALL DB: Insert Pages
                if !pages_to_insert.is_empty() {
                    let _ = db_repo.insert_pages(pages_to_insert).await;
                }

                // 5. CALL DB: Increment Counts
                if !increments.is_empty() {
                    let _ = db_repo.increment_domain_counts(increments).await;
                }

                // 6. CALL DB & Kafka: Queue New URLs
                if !candidate_new_urls.is_empty() {
                    let candidates_vec: Vec<(String, String)> = candidate_new_urls.into_iter().collect();

                    let existing = db_repo.check_existing_urls(candidates_vec.clone())
                        .await
                        .unwrap_or_default();

                    for (domain, url) in candidates_vec {
                        if !existing.contains(&url) {
                            let _ = producer.send(
                                FutureRecord::to("urls-to-crawl").key(&domain).payload(&url),
                                Duration::from_secs(0),
                            ).await;
                        }
                    }
                }
            });

            last_flush = Instant::now();
        }
    }
}