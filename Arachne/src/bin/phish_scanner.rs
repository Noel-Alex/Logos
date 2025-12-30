use chrono::Local;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, ProducerContext, DeliveryResult};
use rdkafka::ClientContext;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::statement::prepared::PreparedStatement;
use serde::Deserialize;
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use url::Url;

// --- CONFIGURATION ---
const CHECK_INTERVAL_MINUTES: u64 = 5; // Run every 5 minutes
const URLSCAN_SEARCH_LIMIT: u32 = 1000; // Max URLs to fetch per call (max 10,000)
const KAFKA_TOPIC: &str = "urls-to-crawl";
const STORAGE_DIR: &str = "urlscan_batches"; // Folder to save backup files

// --- API RESPONSE STRUCTURES ---
#[derive(Deserialize, Debug)]
struct UrlScanResponse {
    results: Vec<UrlScanResult>,
}

#[derive(Deserialize, Debug)]
struct UrlScanResult {
    task: UrlScanTask,
}

#[derive(Deserialize, Debug)]
struct UrlScanTask {
    url: String,
}

// --- KAFKA CONTEXT ---
struct LoggingContext;
impl ClientContext for LoggingContext {}
impl ProducerContext for LoggingContext {
    type DeliveryOpaque = ();
    fn delivery(&self, delivery_result: &DeliveryResult, _delivery_opaque: Self::DeliveryOpaque) {
        if let Err((e, _)) = delivery_result {
            eprintln!("!! Kafka Delivery failed: {:?}", e);
        }
    }
}
type LoggingProducer = BaseProducer<LoggingContext>;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    // 1. SETUP ENVIRONMENT
    let bootstrap_servers = env::var("KAFKA_SERVER").unwrap_or_else(|_| "localhost:9093".to_string());
    let scylla_uri = env::var("SCYLLA_URI").unwrap_or_else(|_| "127.0.0.1:9042".to_string());
    let _ = fs::create_dir_all(STORAGE_DIR); // Ensure storage folder exists

    println!("--- Arachne URLScan Monitor ---");
    println!("Target:      https://urlscan.io/api/v1/search/");
    println!("Interval:    Every {} minutes", CHECK_INTERVAL_MINUTES);
    println!("Storage:     ./{}/", STORAGE_DIR);

    // 2. CONNECT TO SCYLLA
    println!("[1/3] Connecting to ScyllaDB...");
    let session = SessionBuilder::new()
        .known_node(scylla_uri)
        .build()
        .await
        .expect("Failed to connect to Scylla");

    // Ensure Schema
    session.query_unpaged("CREATE KEYSPACE IF NOT EXISTS Arachne WITH replication = {'class': 'SimpleStrategy', 'replication_factor': '1'}", &[]).await.ok();
    session.query_unpaged("CREATE TABLE IF NOT EXISTS Arachne.domain_labels (domain TEXT PRIMARY KEY, is_phishing BOOLEAN, source TEXT)", &[]).await.unwrap();

    let insert_stmt = Arc::new(session.prepare(
        "INSERT INTO Arachne.domain_labels (domain, is_phishing, source) VALUES (?, ?, ?)"
    ).await.unwrap());

    let check_stmt = Arc::new(session.prepare(
        "SELECT domain FROM Arachne.domain_labels WHERE domain = ?"
    ).await.unwrap());

    // 3. CONNECT TO KAFKA
    println!("[2/3] Connecting to Kafka...");
    let producer: LoggingProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("message.timeout.ms", "5000")
        .set("queue.buffering.max.messages", "100000")
        .create_with_context(LoggingContext)
        .expect("Failed to create Kafka producer");

    println!("✔ System Ready. Starting Loop...");

    // 4. INFINITE LOOP
    loop {
        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        println!("\n[{}] 🚀 Starting Batch...", timestamp);

        // A. FETCH from API
        match fetch_urlscan_phishing().await {
            Ok(urls) => {
                if urls.is_empty() {
                    println!("> No URLs found in this batch.");
                } else {
                    println!("> Fetched {} URLs.", urls.len());

                    // B. SAVE to File
                    let filename = save_batch_to_file(&urls);

                    // C. PROCESS (Check Scylla -> Add to Kafka)
                    process_url_list(
                        &urls,
                        &producer,
                        &session,
                        &insert_stmt,
                        &check_stmt
                    ).await;

                    println!("> Batch complete. Saved to '{}'", filename);
                }
            }
            Err(e) => eprintln!("!! API Error: {}", e),
        }

        println!("> Sleeping for {} minutes...", CHECK_INTERVAL_MINUTES);
        sleep(Duration::from_secs(CHECK_INTERVAL_MINUTES * 60)).await;
    }
}

// --- LOGIC FUNCTIONS ---

async fn fetch_urlscan_phishing() -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();

    // Query: task.tags:phishing (Finds tasks explicitly tagged as phishing)
    // Sort: date (implied default by search API usually, but good to be aware)
    let url = format!(
        "https://urlscan.io/api/v1/search/?q=task.tags:phishing&size={}",
        URLSCAN_SEARCH_LIMIT
    );

    let resp = client.get(&url).send().await?;

    if !resp.status().is_success() {
        return Err(format!("HTTP Error: {}", resp.status()).into());
    }

    let json: UrlScanResponse = resp.json().await?;

    // Extract just the URL strings
    let urls: Vec<String> = json.results
        .into_iter()
        .map(|r| r.task.url)
        .collect();

    Ok(urls)
}

fn save_batch_to_file(urls: &[String]) -> String {
    let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();
    let filename = format!("{}/batch_{}.txt", STORAGE_DIR, timestamp);

    let mut file = File::create(&filename).expect("Could not create backup file");
    for url in urls {
        writeln!(file, "{}", url).unwrap();
    }
    filename
}

async fn process_url_list(
    urls: &[String],
    producer: &LoggingProducer,
    session: &Session,
    insert_stmt: &PreparedStatement,
    check_stmt: &PreparedStatement,
) {
    let mut new_count = 0;
    let mut dup_count = 0;

    for url in urls {
        let added = submit_url_to_system(
            url,
            session,
            insert_stmt,
            check_stmt,
            producer
        ).await;

        if added { new_count += 1; } else { dup_count += 1; }
    }

    // Flush briefly to ensure network transmission
    producer.poll(Duration::from_millis(100));
    println!("> Processing Stats: {} New / {} Duplicates", new_count, dup_count);
}

// Identical logic to your 'seeder.rs' but refactored for the loop
async fn submit_url_to_system(
    url_raw: &str,
    session: &Session,
    insert_stmt: &PreparedStatement,
    check_stmt: &PreparedStatement,
    producer: &LoggingProducer,
) -> bool {
    // 1. Normalize
    let full_url = if !url_raw.starts_with("http") {
        format!("https://{}", url_raw)
    } else {
        url_raw.to_string()
    };

    let root_domain = extract_root_domain(&full_url);

    // 2. Check Scylla (Is it already in our system?)
    match session.execute_unpaged(check_stmt, (&root_domain,)).await {
        Ok(result) => {
            if let Ok(rows) = result.into_rows_result() {
                if rows.rows_num() > 0 {
                    return false; // Duplicate
                }
            }
        },
        Err(e) => {
            eprintln!("!! DB Read Error: {}", e);
            return false;
        }
    }

    // 3. Insert into Scylla
    if let Err(e) = session.execute_unpaged(insert_stmt, (&root_domain, true, "urlscan_api")).await {
        eprintln!("!! DB Write Error: {}", e);
    }

    // 4. Send to Kafka
    if let Err(_) = producer.send(
        BaseRecord::to(KAFKA_TOPIC).payload(&full_url).key(&root_domain),
    ) {
        eprintln!("!! Kafka Buffer Full");
    }

    true
}

// --- DOMAIN EXTRACTION UTILS (Copied from your snippet) ---

fn extract_root_domain(url_str: &str) -> String {
    if let Ok(url) = Url::parse(url_str) {
        if let Some(host) = url.domain() {
            return clean_host_string(host);
        }
    }
    // Fallback for messy strings
    let cleaned = url_str
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or(url_str);
    clean_host_string(cleaned)
}

fn clean_host_string(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').collect();
    let len = parts.len();
    if len < 2 { return host.to_string(); }

    let tld = parts[len-1];
    let second = parts[len-2];

    let is_multipart = ["co", "com", "net", "org", "edu", "gov", "ac"].contains(&second) && tld.len() == 2;

    if is_multipart && len >= 3 {
        parts[len-3..].join(".")
    } else {
        parts[len-2..].join(".")
    }
}