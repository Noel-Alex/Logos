// src/bin/coordinator.rs
use arachne::{CrawlResult, db};
use rdkafka::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::Message;
use rdkafka::producer::{FutureProducer, FutureRecord};
use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Semaphore, mpsc, Mutex};
use tokio::time::{self, Duration, Instant};
use url::Url;


const MAX_URLS_QUEUED_PER_SITE: i64 = 500;
const BATCH_SIZE: usize = 100;
const BATCH_TIMEOUT: Duration = Duration::from_millis(200);

struct WorkItem {
    result: CrawlResult,
}


struct Stats {
    processed: AtomicUsize,
    queued: AtomicUsize,
    dropped_limit: AtomicUsize,
    dropped_dupe: AtomicUsize,
}

/// SMART DOMAIN EXTRACTION
fn get_root_domain(url_str: &str) -> Option<String> {
    let url = Url::parse(url_str).ok()?;
    let host = url.domain()?;
    let parts: Vec<&str> = host.split('.').collect();
    let len = parts.len();
    if len < 2 { return Some(host.to_string()); }
    let tld = parts[len-1];
    let second = parts[len-2];
    let is_multipart = ["co", "com", "net", "org", "edu", "gov", "ac"].contains(&second) && tld.len() == 2;
    if is_multipart && len >= 3 { Some(parts[len-3..].join(".")) }
    else { Some(parts[len-2..].join(".")) }
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    println!("Coordinator initializing...");

    let db_repo = Arc::new(db::ArachneRepo::new().await.expect("DB Initialization failed"));

    let bootstrap_servers = env::var("KAFKA_SERVER").expect("KAFKA_SERVER missing");
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("queue.buffering.max.messages", "100000")
        .set("acks", "1")
        .create()
        .expect("Producer failed");

    let (tx, mut rx) = mpsc::channel::<WorkItem>(10_000);

    let global_quota_tracker: Arc<Mutex<HashMap<String, i64>>> = Arc::new(Mutex::new(HashMap::new()));

    let stats = Arc::new(Stats {
        processed: AtomicUsize::new(0),
        queued: AtomicUsize::new(0),
        dropped_limit: AtomicUsize::new(0),
        dropped_dupe: AtomicUsize::new(0),
    });

    // --- 1. MONITORING TASK ---
    let stats_monitor = stats.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let p = stats_monitor.processed.swap(0, Ordering::Relaxed);
            let q = stats_monitor.queued.swap(0, Ordering::Relaxed);
            let dl = stats_monitor.dropped_limit.swap(0, Ordering::Relaxed);
            let dd = stats_monitor.dropped_dupe.swap(0, Ordering::Relaxed);

            if p > 0 || q > 0 || dl > 0 {
                println!(
                    "--------- STATUS (5s) ---------\n\
                     Processed  : {}\n\
                     Queued     : {}\n\
                     Limit Drop : {}\n\
                     Dupe Drop  : {}\n\
                     -------------------------------",
                    p, q, dl, dd
                );
            }
        }
    });

    // --- 2. CONSUMER TASK ---
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
            match consumer.recv().await {
                Ok(m) => {
                    if let Some(Ok(payload)) = m.payload_view::<str>() {
                        if let Ok(result) = serde_json::from_str::<CrawlResult>(payload) {
                            if tx.send(WorkItem { result }).await.is_err() { break; }
                        }
                    }
                }
                Err(e) => eprintln!("Kafka Error: {}", e),
            }
        }
    });

    // --- 3. PROCESSOR LOOP ---
    let mut buffer: Vec<WorkItem> = Vec::with_capacity(BATCH_SIZE);
    let mut last_flush = Instant::now();
    // High concurrency because batch size is small now
    let semaphore = Arc::new(Semaphore::new(50));

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
            let stats = stats.clone();

            tokio::spawn(async move {
                let _permit = permit;
                let batch_count = batch.len();

                // --- PHASE 1: PREPARATION ---
                let mut pages_by_root: HashMap<String, Vec<CrawlResult>> = HashMap::new();
                let mut db_inserts = Vec::new();
                let mut db_increments: HashMap<String, i64> = HashMap::new();

                for item in batch {
                    if let Some(root) = get_root_domain(&item.result.source_url) {
                        // Prepare Insert
                        db_inserts.push((root.clone(), item.result.clone()));
                        *db_increments.entry(root.clone()).or_insert(0) += 1;

                        // Filter Internal Links
                        let internal_links: Vec<String> = item.result.discovered_urls
                            .into_iter()
                            .filter(|link| get_root_domain(link).map_or(false, |d| d == root))
                            .collect();

                        let filtered_result = CrawlResult {
                            discovered_urls: internal_links,
                            ..item.result
                        };
                        pages_by_root.entry(root).or_default().push(filtered_result);
                    }
                }

                // --- PHASE 2: DB WRITE (Content) ---
                // We save content immediately.
                if !db_inserts.is_empty() {
                    let _ = db_repo.insert_pages(db_inserts).await;
                }
                if !db_increments.is_empty() {
                    let _ = db_repo.increment_domain_counts(db_increments).await;
                }
                stats.processed.fetch_add(batch_count, Ordering::Relaxed);

                // --- PHASE 3: QUOTA PRE-CHECK (Optimization) ---
                // Only collect candidate URLs for domains that HAVE SPACE.
                // This saves us from asking Scylla to dedup 10,000 links for a site that is already full.

                let mut tracker = tracker_lock.lock().await;

                // Sync missing
                let mut missing = Vec::new();
                for d in pages_by_root.keys() {
                    if !tracker.contains_key(d) { missing.push(d.clone()); }
                }
                if !missing.is_empty() {
                    let counts = db_repo.get_domain_counts(missing).await.unwrap_or_default();
                    for (d, c) in counts { tracker.insert(d, c); }
                }

                let mut candidate_urls: HashSet<(String, String)> = HashSet::new();

                for (domain, pages) in &pages_by_root {
                    let current_total = *tracker.get(domain).unwrap_or(&0);

                    if current_total >= MAX_URLS_QUEUED_PER_SITE {
                        // Domain is full. Count these as dropped.
                        let mut dropped = 0;
                        for p in pages { dropped += p.discovered_urls.len(); }
                        stats.dropped_limit.fetch_add(dropped, Ordering::Relaxed);
                        continue;
                    }

                    // Domain has space. Collect candidates.
                    for page in pages {
                        for url in &page.discovered_urls {
                            candidate_urls.insert((domain.clone(), url.clone()));
                        }
                    }
                }

                // Release lock while we talk to DB (Dedup)
                drop(tracker);

                if candidate_urls.is_empty() { return; }

                // --- PHASE 4: DEDUPLICATION ---
                let candidates_vec: Vec<(String, String)> = candidate_urls.into_iter().collect();
                let existing = db_repo.check_existing_urls(candidates_vec.clone())
                    .await
                    .unwrap_or_default();

                let mut true_new_urls_by_root: HashMap<String, Vec<String>> = HashMap::new();
                let mut dupes_found = 0;

                for (domain, url) in candidates_vec {
                    if !existing.contains(&url) {
                        true_new_urls_by_root.entry(domain).or_default().push(url);
                    } else {
                        dupes_found += 1;
                    }
                }
                stats.dropped_dupe.fetch_add(dupes_found, Ordering::Relaxed);

                // --- PHASE 5: FINAL QUOTA & QUEUE ---
                // Re-acquire lock to verify space and update count
                let mut tracker = tracker_lock.lock().await;

                for (domain, new_urls) in true_new_urls_by_root {
                    let current_total = *tracker.get(&domain).unwrap_or(&0);
                    let slots = if current_total < MAX_URLS_QUEUED_PER_SITE {
                        MAX_URLS_QUEUED_PER_SITE - current_total
                    } else { 0 };

                    if slots > 0 {
                        let to_queue = std::cmp::min(new_urls.len(), slots as usize);
                        let mut queued = 0;
                        for i in 0..to_queue {
                            let _ = producer.send(
                                FutureRecord::to("urls-to-crawl").key(&domain).payload(&new_urls[i]),
                                Duration::from_secs(0),
                            ).await;
                            queued += 1;
                        }

                        // Update Tracker
                        *tracker.entry(domain).or_insert(0) += queued;
                        stats.queued.fetch_add(queued as usize, Ordering::Relaxed);

                        let dropped = new_urls.len() - to_queue;
                        if dropped > 0 {
                            stats.dropped_limit.fetch_add(dropped, Ordering::Relaxed);
                        }
                    } else {
                        stats.dropped_limit.fetch_add(new_urls.len(), Ordering::Relaxed);
                    }
                }
            });

            last_flush = Instant::now();
        }
    }
}