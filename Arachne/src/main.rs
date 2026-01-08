// src/bin/seeder.rs
use csv::ReaderBuilder;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, Producer, ProducerContext, DeliveryResult};
use rdkafka::ClientContext;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

const PHISHING_CSV_LIMIT: usize = 0;
const PHISHING_TXT_LIMIT: usize = 0;
const LEGIT_CSV_LIMIT: usize = 10_000;

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


fn extract_root_domain(url_str: &str) -> String {
    if let Ok(url) = Url::parse(url_str) {
        if let Some(host) = url.domain() {
            return clean_host_string(host);
        }
    }

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

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let bootstrap_servers = env::var("KAFKA_SERVER").unwrap_or_else(|_| "localhost:9093".to_string());
    let scylla_uri = env::var("SCYLLA_URI").unwrap_or_else(|_| "127.0.0.1:9042".to_string());
    let topic_name = "urls-to-crawl";

    println!("--- Arachne Master Seeder (Limited) ---");
    println!("Kafka:  {}", bootstrap_servers);
    println!("Scylla: {}", scylla_uri);

    // 1. CONNECT TO SCYLLA
    println!("[1/5] Connecting to ScyllaDB...");
    let session = SessionBuilder::new()
        .known_node(scylla_uri)
        .build()
        .await
        .expect("Failed to connect to Scylla");

    session.query_unpaged("CREATE KEYSPACE IF NOT EXISTS Arachne WITH replication = {'class': 'SimpleStrategy', 'replication_factor': '1'}", &[]).await.ok();
    session.query_unpaged("CREATE TABLE IF NOT EXISTS Arachne.domain_labels (url TEXT PRIMARY KEY, is_phishing BOOLEAN, source TEXT)", &[]).await.unwrap();

    let insert_stmt = Arc::new(session.prepare(
        "INSERT INTO Arachne.domain_labels (url, is_phishing, source) VALUES (?, ?, ?)"
    ).await.unwrap());

    let check_stmt = Arc::new(session.prepare(
        "SELECT url FROM Arachne.domain_labels WHERE url = ?"
    ).await.unwrap());

    println!("✔ Scylla Ready.");

    println!("[2/5] Connecting to Kafka...");
    let producer: LoggingProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("message.timeout.ms", "5000")
        .set("queue.buffering.max.messages", "100000")
        .set("socket.keepalive.enable", "true")
        .set("socket.nagle.disable", "true")
        .set("message.timeout.ms", "300000") // 5 minutes (allow for network blips)
        .set("compression.type", "lz4")
        .create_with_context(LoggingContext)
        .expect("Failed to create Kafka producer");

    println!("✔ Kafka Ready.");

    let start_time = Instant::now();


    println!("\n[3/5] Processing Phishing CSV (Limit: {})...", if PHISHING_CSV_LIMIT == 0 { "ALL".to_string() } else { PHISHING_CSV_LIMIT.to_string() });
    process_csv("phishing.csv", &producer, &session, &insert_stmt, &check_stmt, topic_name, true, PHISHING_CSV_LIMIT).await;

    println!("\n[4/5] Processing Phishing TXT (Limit: {})...", if PHISHING_TXT_LIMIT == 0 { "ALL".to_string() } else { PHISHING_TXT_LIMIT.to_string() });
    process_text_file("phishing.txt", &producer, &session, &insert_stmt, &check_stmt, topic_name, true, PHISHING_TXT_LIMIT).await;

    println!("\n[5/5] Processing Legit CSV (Limit: {})...", if LEGIT_CSV_LIMIT == 0 { "ALL".to_string() } else { LEGIT_CSV_LIMIT.to_string() });
    process_csv("legit.csv", &producer, &session, &insert_stmt, &check_stmt, topic_name, false, LEGIT_CSV_LIMIT).await;

    println!("\n[...] Flushing Kafka buffers (waiting 10s)...");
    producer.flush(Duration::from_secs(10));

    println!("\n✔ All Done. Total Runtime: {:.2?}", start_time.elapsed());
}

/*
async fn submit_url_to_system(
    url_raw: &str,
    is_phishing: bool,
    source_tag: &str,
    session: &Session,
    insert_stmt: &scylla::statement::prepared::PreparedStatement,
    check_stmt: &scylla::statement::prepared::PreparedStatement,
    producer: &LoggingProducer,
    topic: &str
) -> bool {
    let full_url = if !url_raw.starts_with("http") {
        format!("https://{}", url_raw)
    } else {
        url_raw.to_string()
    };

    let root_domain = extract_root_domain(&full_url);

    // 1. Check if exists (Read) - LOGIC PRESERVED
    match session.execute_unpaged(check_stmt, (&root_domain,)).await {
        Ok(result) => {
            if let Ok(rows) = result.into_rows_result() {
                if rows.rows_num() > 0 {
                    return false;
                }
            }
        },
        Err(e) => {
            eprintln!("DB Read Error {}: {}", root_domain, e);
            return false;
        }
    }

    // 2. Insert into DB (Write) - LOGIC PRESERVED
    if let Err(e) = session.execute_unpaged(insert_stmt, (&root_domain, is_phishing, source_tag)).await {
        eprintln!("DB Write Error {}: {}", root_domain, e);
    }

    // 3. Send to Kafka with BACKPRESSURE (Fixing Buffer Full)
    loop {
        let record = BaseRecord::to(topic).payload(&full_url).key(&root_domain);

        match producer.send(record) {
            Ok(_) => break, // Successfully enqueued
            Err((rdkafka::error::KafkaError::MessageProduction(rdkafka::error::RDKafkaErrorCode::QueueFull), _)) => {
                // Buffer is full. Poll to allow the driver to send batch and free space.
                // This slows down the loop to match Redpanda's speed.
                producer.poll(Duration::from_millis(50));
            },
            Err((e, _)) => {
                eprintln!("Kafka Critical Error sending {}: {:?}", full_url, e);
                break; // Stop retrying on fatal errors
            }
        }
    }

    true
}
 */

async fn submit_url_to_system(
    url_raw: &str,
    is_phishing: bool,
    source_tag: &str,
    session: &Session,
    insert_stmt: &scylla::statement::prepared::PreparedStatement,
    check_stmt: &scylla::statement::prepared::PreparedStatement,
    producer: &LoggingProducer,
    topic: &str
) -> bool {
    // 1. Standardize the URL (ensure https://)
    let full_url = if !url_raw.contains("://") {
        format!("https://{}", url_raw)
    } else {
        url_raw.to_string()
    };

    // We still calculate root_domain for Kafka Partitioning (Load Balancing)
    // allowing the Coordinator to throttle by site later.
    let root_domain = extract_root_domain(&full_url);

    // 2. CHECK: Use the FULL URL for the DB check (Fixes the "Duplicate" issue)
    match session.execute_unpaged(check_stmt, (&full_url,)).await {
        Ok(result) => {
            if let Ok(rows) = result.into_rows_result() {
                if rows.rows_num() > 0 {
                    return false; // Actually exists
                }
            }
        },
        Err(e) => {
            eprintln!("DB Read Error {}: {}", full_url, e);
            return false;
        }
    }

    // 3. WRITE: Insert the FULL URL into the DB
    if let Err(e) = session.execute_unpaged(insert_stmt, (&full_url, is_phishing, source_tag)).await {
        eprintln!("DB Write Error {}: {}", full_url, e);
    }

    // 4. KAFKA: Send with Backpressure
    loop {
        // Payload = Full URL
        // Key = Root Domain (Keeps all 'blogspot.com' URLs on the same partition for throttling)
        let record = BaseRecord::to(topic).payload(&full_url).key(&root_domain);

        match producer.send(record) {
            Ok(_) => break,
            Err((rdkafka::error::KafkaError::MessageProduction(rdkafka::error::RDKafkaErrorCode::QueueFull), _)) => {
                producer.poll(Duration::from_millis(50));
            },
            Err((e, _)) => {
                eprintln!("Kafka Critical Error: {:?}", e);
                break;
            }
        }
    }

    true
}


async fn process_text_file(
    filename: &str,
    producer: &LoggingProducer,
    session: &Session,
    insert_stmt: &scylla::statement::prepared::PreparedStatement,
    check_stmt: &scylla::statement::prepared::PreparedStatement,
    topic: &str,
    is_phishing: bool,
    limit: usize,
) {
    if !Path::new(filename).exists() {
        eprintln!("⚠ Note: File '{}' not found. Skipping.", filename);
        return;
    }

    let file = File::open(filename).unwrap();
    let reader = BufReader::new(file);
    let source_tag = if is_phishing { "custom_txt_list" } else { "legit_txt_list" };

    let mut processed = 0;
    let mut skipped = 0;

    println!("> Reading '{}'...", filename);

    for line_res in reader.lines() {
        if limit > 0 && processed >= limit {
            println!("\nReached limit of {} items for {}.", limit, filename);
            break;
        }

        if let Ok(line) = line_res {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }

            let added = submit_url_to_system(
                trimmed, is_phishing, source_tag,
                session, insert_stmt, check_stmt,
                producer, topic
            ).await;

            if added {
                processed += 1;
            } else {
                skipped += 1;
            }

            if (processed + skipped) % 1000 == 0 {
                producer.poll(Duration::from_millis(0));
                print!("\r> Progress: {} New / {} Duplicates", processed, skipped);
                let _ = std::io::stdout().flush();
            }
        }
    }
    println!("\r> Finished '{}': {} New URLs Queued, {} Duplicates Skipped.", filename, processed, skipped);
}

async fn process_csv(
    filename: &str,
    producer: &LoggingProducer,
    session: &Session,
    insert_stmt: &scylla::statement::prepared::PreparedStatement,
    check_stmt: &scylla::statement::prepared::PreparedStatement,
    topic: &str,
    is_phishing: bool,
    limit: usize,
) {
    if !Path::new(filename).exists() {
        eprintln!("Note: File '{}' not found. Skipping.", filename);
        return;
    }

    let file = File::open(filename).unwrap();
    let buf_reader = BufReader::new(file);
    let source_tag = if is_phishing { "phishtank" } else { "tranco" };
    let has_headers = true;

    let mut rdr = ReaderBuilder::new()
        .has_headers(has_headers)
        .from_reader(buf_reader);

    let mut processed = 0;
    let mut skipped = 0;
    println!("> Reading '{}'...", filename);

    for result in rdr.records() {
        if limit > 0 && processed >= limit {
            println!("\nReached limit of {} items for {}.", limit, filename);
            break;
        }

        if let Ok(record) = result {
            let raw_input = record.get(1).unwrap_or("").trim();

            if !raw_input.is_empty() {
                let added = submit_url_to_system(
                    raw_input, is_phishing, source_tag,
                    session, insert_stmt, check_stmt,
                    producer, topic
                ).await;

                if added {
                    processed += 1;
                } else {
                    skipped += 1;
                }
            }
        }

        if (processed + skipped) % 5000 == 0 {
            producer.poll(Duration::from_millis(0));
            print!("\r> Progress: {} New / {} Duplicates", processed, skipped);
            let _ = std::io::stdout().flush();
        }
    }
    println!("\r> Finished '{}': {} New URLs Queued, {} Duplicates Skipped.", filename, processed, skipped);
}