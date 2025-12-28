// src/bin/coordinator.rs
use arachne::{CrawlResult, db};
use rdkafka::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::Message;
use rdkafka::producer::{FutureProducer, FutureRecord};
use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::Arc;
use tokio::sync::{Semaphore, mpsc, Mutex};
use tokio::time::{self, Duration, Instant};
use url::Url;

// --- CONFIGURATION ---
const MAX_URLS_QUEUED_PER_SITE: i64 = 1000;
const BATCH_SIZE: usize = 1000;
const BATCH_TIMEOUT: Duration = Duration::from_millis(500);

struct WorkItem {
    result: CrawlResult,
}

/// SMART DOMAIN EXTRACTION
/// Turns "en.wikipedia.org" -> "wikipedia.org"
/// Turns "tieba.baidu.com" -> "baidu.com"
/// Turns "google.co.uk" -> "google.co.uk"
fn get_root_domain(url_str: &str) -> Option<String> {
    let url = Url::parse(url_str).ok()?;
    let host = url.domain()?;

    let parts: Vec<&str> = host.split('.').collect();
    let len = parts.len();

    // 1. Basic check
    if len < 2 { return Some(host.to_string()); }

    let tld = parts[len-1];
    let second = parts[len-2];

    // 2. Handle Multipart TLDs (e.g., .co.uk, .com.cn)
    // If the 2nd part is a generic code (co, com, org) AND the TLD is 2 chars (country code)
    let is_multipart = ["co", "com", "net", "org", "edu", "gov", "ac"].contains(&second)
        && tld.len() == 2;

    if is_multipart && len >= 3 {
        // Take last 3 parts: "google.co.uk"
        Some(parts[len-3..].join("."))
    } else {
        // Take last 2 parts: "wikipedia.org", "baidu.com"
        Some(parts[len-2..].join("."))
    }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    println!("Coordinator initializing...");

    // 1. Initialize DB
    let db_repo = Arc::new(db::ArachneRepo::new().await.expect("DB Initialization failed"));

    // 2. Initialize Kafka
    let bootstrap_servers = env::var("KAFKA_SERVER").expect("KAFKA_SERVER missing");
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("queue.buffering.max.messages", "100000")
        .set("acks", "1")
        .create()
        .expect("Producer failed");

    let (tx, mut rx) = mpsc::channel::<WorkItem>(10_000);

    // --- GLOBAL STATE ---
    // Key: ROOT Domain (e.g., "wikipedia.org", NOT "en.wikipedia.org")
    // Value: Projected Count
    let global_quota_tracker: Arc<Mutex<HashMap<String, i64>>> = Arc::new(Mutex::new(HashMap::new()));

    // 3. Consumer Task
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

    // 4. Processing Loop
    let mut buffer: Vec<WorkItem> = Vec::with_capacity(BATCH_SIZE);
    let mut last_flush = Instant::now();
    let semaphore = Arc::new(Semaphore::new(10));

    println!("Processor Loop Started. Quota Limit: {}", MAX_URLS_QUEUED_PER_SITE);

    loop {
        let fill_buffer = async {
            while buffer.len() < BATCH_SIZE {
                match time::timeout(Duration::from_millis(50), rx.recv()).await {
                    Ok(Some(item)) => buffer.push(item),
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
        };
        fill_buffer.await;

        let should_flush = !buffer.is_empty()
            && (buffer.len() >= BATCH_SIZE || last_flush.elapsed() > BATCH_TIMEOUT);

        if should_flush {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let batch: Vec<WorkItem> = std::mem::replace(&mut buffer, Vec::with_capacity(BATCH_SIZE));

            let db_repo = db_repo.clone();
            let producer = producer.clone();
            let tracker_lock = global_quota_tracker.clone();

            tokio::spawn(async move {
                let _permit = permit;

                // --- PHASE 1: PREPARATION ---
                // Key = Root Domain ("wikipedia.org")
                let mut pages_by_root_domain: HashMap<String, Vec<CrawlResult>> = HashMap::new();
                let mut db_inserts = Vec::new();
                let mut db_increments: HashMap<String, i64> = HashMap::new();

                for item in batch {
                    if let Some(root_domain) = get_root_domain(&item.result.source_url) {

                        // 1. Prepare DB Insert Data
                        // We use root_domain as the partition key.
                        // Note: You can still store the specific source_url in the URL column.
                        db_inserts.push((root_domain.clone(), item.result.clone()));
                        *db_increments.entry(root_domain.clone()).or_insert(0) += 1;

                        // 2. Filter Links (STRICT ROOT DOMAIN MATCHING)
                        // If seed is "en.wikipedia.org", root is "wikipedia.org".
                        // If link is "fr.wikipedia.org", root is "wikipedia.org".
                        // Match = YES.
                        let internal_links: Vec<String> = item.result.discovered_urls
                            .into_iter()
                            .filter(|link| get_root_domain(link).map_or(false, |d| d == root_domain))
                            .collect();

                        // 3. Group
                        let filtered_result = CrawlResult {
                            discovered_urls: internal_links,
                            ..item.result
                        };
                        pages_by_root_domain.entry(root_domain).or_default().push(filtered_result);
                    }
                }

                // --- PHASE 2: COMMIT TO DB ---
                if !db_inserts.is_empty() {
                    let _ = db_repo.insert_pages(db_inserts).await;
                }
                if !db_increments.is_empty() {
                    let _ = db_repo.increment_domain_counts(db_increments).await;
                }

                // --- PHASE 3: DEDUPLICATION ---
                let mut all_candidate_urls: HashSet<(String, String)> = HashSet::new();
                for (domain, pages) in &pages_by_root_domain {
                    for page in pages {
                        for url in &page.discovered_urls {
                            all_candidate_urls.insert((domain.clone(), url.clone()));
                        }
                    }
                }

                if all_candidate_urls.is_empty() { return; }

                let candidates_vec: Vec<(String, String)> = all_candidate_urls.into_iter().collect();

                // Ask Scylla: "Which of these already exist?"
                let existing_urls = db_repo.check_existing_urls(candidates_vec.clone())
                    .await
                    .unwrap_or_default();

                let mut true_new_urls_by_root: HashMap<String, Vec<String>> = HashMap::new();
                for (domain, url) in candidates_vec {
                    if !existing_urls.contains(&url) {
                        true_new_urls_by_root.entry(domain).or_default().push(url);
                    }
                }

                // --- PHASE 4: QUOTA CHECK & QUEUEING ---
                let mut tracker = tracker_lock.lock().await;

                // Sync missing
                let mut missing_domains = Vec::new();
                for domain in true_new_urls_by_root.keys() {
                    if !tracker.contains_key(domain) {
                        missing_domains.push(domain.clone());
                    }
                }
                if !missing_domains.is_empty() {
                    let counts = db_repo.get_domain_counts(missing_domains).await.unwrap_or_default();
                    for (d, c) in counts { tracker.insert(d, c); }
                }

                // Push to Kafka
                for (domain, new_urls) in true_new_urls_by_root {
                    let current_total = *tracker.get(&domain).unwrap_or(&0);

                    let slots_remaining = if current_total < MAX_URLS_QUEUED_PER_SITE {
                        MAX_URLS_QUEUED_PER_SITE - current_total
                    } else {
                        0
                    };

                    if slots_remaining > 0 {
                        let count_to_queue = std::cmp::min(new_urls.len(), slots_remaining as usize);

                        let mut added_count = 0;
                        for i in 0..count_to_queue {
                            let url = &new_urls[i];
                            let _ = producer.send(
                                FutureRecord::to("urls-to-crawl").key(&domain).payload(url),
                                Duration::from_secs(0),
                            ).await;
                            added_count += 1;
                        }

                        *tracker.entry(domain).or_insert(0) += added_count;
                    }
                }

                drop(tracker);
                // --- END ---
            });

            last_flush = Instant::now();
        }
    }
}