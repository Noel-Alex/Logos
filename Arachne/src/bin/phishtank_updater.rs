use csv::ReaderBuilder;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, Producer, ProducerContext, DeliveryResult};
use rdkafka::ClientContext;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use serde::Deserialize;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use url::Url;

// --- CONFIGURATION ---
const UPDATE_INTERVAL_MINUTES: u64 = 60;
const USER_AGENT: &str = "ArachneCrawler/1.0 (Student Project)";


const DATA_SOURCES: &[(&str, Format)] = &[
    ("http://data.phishtank.com/data/online-valid.json", Format::Json),
    ("http://data.phishtank.com/data/online-valid.csv", Format::Csv),
    // ("http://data.phishtank.com/data/<KEY>/online-valid.json", Format::Json),
];

#[derive(Clone, Copy, Debug)]
enum Format {
    Json,
    Csv,
}

// --- DATA STRUCTURES ---

#[derive(Deserialize, Debug)]
struct PhishTankJsonEntry {
    url: String,
}

#[derive(Deserialize, Debug)]
struct PhishTankCsvEntry {
    url: String,
}

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

    println!("--- Arachne PhishTank Auto-Updater (Multi-Source) ---");

    // 1. SETUP SCYLLA
    println!("[Init] Connecting to ScyllaDB...");
    let session = SessionBuilder::new()
        .known_node(scylla_uri)
        .build()
        .await
        .expect("Failed to connect to Scylla");

    session.query_unpaged("CREATE KEYSPACE IF NOT EXISTS Arachne WITH replication = {'class': 'SimpleStrategy', 'replication_factor': '1'}", &[]).await.ok();
    session.query_unpaged("CREATE TABLE IF NOT EXISTS Arachne.domain_labels (domain TEXT PRIMARY KEY, is_phishing BOOLEAN, source TEXT)", &[]).await.unwrap();

    let insert_stmt = Arc::new(session.prepare(
        "INSERT INTO Arachne.domain_labels (domain, is_phishing, source) VALUES (?, ?, ?)"
    ).await.unwrap());

    let check_stmt = Arc::new(session.prepare(
        "SELECT domain FROM Arachne.domain_labels WHERE domain = ?"
    ).await.unwrap());

    // 2. SETUP KAFKA
    println!("[Init] Connecting to Kafka...");
    let producer: LoggingProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("message.timeout.ms", "5000")
        .set("queue.buffering.max.messages", "100000")
        .create_with_context(LoggingContext)
        .expect("Failed to create Kafka producer");

    // 3. SETUP HTTP CLIENT
    let http_client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(120))
        .build()
        .expect("Failed to build HTTP client");

    println!("System Ready. Starting Update Loop (Every {} mins).", UPDATE_INTERVAL_MINUTES);

    let mut interval = time::interval(Duration::from_secs(UPDATE_INTERVAL_MINUTES * 60));

    loop {
        interval.tick().await;
        println!("\n[Task] Starting Update Cycle...");

        let mut success = false;

        // ITERATE THROUGH SOURCES
        for (url, format) in DATA_SOURCES {
            println!("> Trying source: {} ({:?})", url, format);
            match fetch_and_process(url, *format, &http_client, &producer, &session, &insert_stmt, &check_stmt, topic_name).await {
                Ok((new, skipped)) => {
                    println!("✔ Success! Added: {} | Skipped: {}", new, skipped);
                    success = true;
                    break;
                }
                Err(e) => {
                    eprintln!("⚠ Failed to fetch from {}: {}", url, e);
                    eprintln!("> Trying next source...");
                }
            }
        }

        if !success {
            eprintln!("All data sources failed. Will retry in {} mins.", UPDATE_INTERVAL_MINUTES);
        }
    }
}

async fn fetch_and_process(
    url: &str,
    format: Format,
    client: &reqwest::Client,
    producer: &LoggingProducer,
    session: &Session,
    insert_stmt: &scylla::statement::prepared::PreparedStatement,
    check_stmt: &scylla::statement::prepared::PreparedStatement,
    topic: &str
) -> Result<(usize, usize), Box<dyn std::error::Error>> {

    // 1. Download
    let response = client.get(url).send().await?;
    if !response.status().is_success() {
        return Err(format!("HTTP Error: {}", response.status()).into());
    }

    // Read bytes (we need raw bytes for CSV or JSON parsing)
    let content = response.bytes().await?;
    println!("> Downloaded {:.2} MB. Parsing...", content.len() as f64 / 1024.0 / 1024.0);

    let urls_to_process: Vec<String>;

    // 2. Parse based on Format
    match format {
        Format::Json => {
            let entries: Vec<PhishTankJsonEntry> = serde_json::from_slice(&content)?;
            urls_to_process = entries.into_iter().map(|e| e.url).collect();
        },
        Format::Csv => {
            let mut rdr = ReaderBuilder::new()
                .has_headers(true)
                .from_reader(content.as_ref());

            let mut list = Vec::new();
            for result in rdr.deserialize() {
                let entry: PhishTankCsvEntry = result?;
                list.push(entry.url);
            }
            urls_to_process = list;
        }
    }

    println!("> Found {} URLs. Checking against Database...", urls_to_process.len());

    // 3. Process
    let mut new_count = 0;
    let mut dup_count = 0;

    for (i, url) in urls_to_process.iter().enumerate() {
        if i % 2000 == 0 && i > 0 {
            print!("\r> Checking entry {}/{}...", i, urls_to_process.len());
            use std::io::Write;
            std::io::stdout().flush().ok();
        }

        let is_new = submit_url_to_system(
            url,
            true,
            "phishtank_auto",
            session,
            insert_stmt,
            check_stmt,
            producer,
            topic
        ).await;

        if is_new {
            new_count += 1;
        } else {
            dup_count += 1;
        }
    }

    println!();
    producer.flush(Duration::from_secs(5));

    Ok((new_count, dup_count))
}

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

    // 1. CHECK EXISTENCE
    match session.execute_unpaged(check_stmt, (&full_url,)).await {
        Ok(result) => {
            if let Ok(rows) = result.into_rows_result() {
                if rows.rows_num() > 0 {
                    return false; // Exists -> SKIP
                }
            }
        },
        Err(e) => {
            eprintln!("DB Read Error {}: {}", root_domain, e);
            return false;
        }
    }

    // 2. INSERT LABEL
    if let Err(e) = session.execute_unpaged(insert_stmt, (&root_domain, is_phishing, source_tag)).await {
        eprintln!("DB Write Error {}: {}", root_domain, e);
    }

    // 3. KAFKA QUEUE
    if let Err(_) = producer.send(
        BaseRecord::to(topic).payload(&full_url).key(&root_domain),
    ) {
        eprintln!("Kafka Buffer Full!");
    }

    true
}