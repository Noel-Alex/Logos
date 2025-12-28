use csv::ReaderBuilder;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{BaseProducer, BaseRecord, Producer, ProducerContext, DeliveryResult};
use rdkafka::{ClientContext, Message};
use serde::Deserialize;
use std::env;
use std::fs::File;
use std::io::{BufReader, Write};
use std::time::{Duration, Instant};
use url::Url;

// --- CONTEXT FOR LOGGING ERRORS ---
// This allows us to see if the Broker is rejecting messages in the background
struct LoggingContext;

impl ClientContext for LoggingContext {}

impl ProducerContext for LoggingContext {
    type DeliveryOpaque = ();

    fn delivery(&self, delivery_result: &DeliveryResult, _delivery_opaque: Self::DeliveryOpaque) {
        if let Err((e, _)) = delivery_result {
            // If you see this in your terminal, the Broker is reachable but rejecting data
            eprintln!("!! Delivery failed: {:?}", e);
        }
    }
}

// Type alias for our producer with context
type LoggingProducer = BaseProducer<LoggingContext>;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    // Default to localhost:9093 (Common for external Docker access)
    let bootstrap_servers = env::var("KAFKA_SERVER").unwrap_or_else(|_| "localhost:9093".to_string());
    let topic_name = "urls-to-crawl";

    println!("--- Arachne Diagnostic Seeder ---");
    println!("Target Broker: {}", bootstrap_servers);

    // 1. CREATE PRODUCER
    let producer: LoggingProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("message.timeout.ms", "5000")
        .set("queue.buffering.max.messages", "100000")
        .create_with_context(LoggingContext)
        .expect("Failed to create Kafka producer");

    // 2. CONNECTION TEST (The most important part)
    println!("\n[...] Testing connection to Broker...");
    match producer.client().fetch_metadata(None, Duration::from_secs(5)) {
        Ok(metadata) => {
            println!("✔ Connection Successful!");
            println!("✔ Found {} brokers and {} topics.", metadata.brokers().len(), metadata.topics().len());

            // Check if topic exists
            let topic_exists = metadata.topics().iter().any(|t| t.name() == topic_name);
            if !topic_exists {
                println!("⚠ WARNING: Topic '{}' does not exist yet. Ensure auto-creation is on.", topic_name);
            }
        },
        Err(e) => {
            eprintln!("\n🛑 FATAL ERROR: Could not connect to Kafka at {}.", bootstrap_servers);
            eprintln!("Error Details: {}", e);
            eprintln!("CHECK: Is your Docker container running? Is port 9093 mapped in docker-compose?");
            return; // Stop here
        }
    }

    let start_time = Instant::now();

    // 3. PROCESS FILES
    println!("\n--- Phase 1: Phishing Sites ---");
    let _ = process_csv("phishing.csv", &producer, topic_name, true, 0);

    println!("\n--- Phase 2: Legit Sites ---");
    let _ = process_csv("legit.csv", &producer, topic_name, false, 10_000);

    // 4. FLUSH
    println!("\n[...] Flushing final messages (Do not close)...");
    producer.flush(Duration::from_secs(30));

    println!("✔ Done. Total Runtime: {:.2?}", start_time.elapsed());
}

fn process_csv(
    file_path: &str,
    producer: &LoggingProducer,
    topic: &str,
    is_phishing_format: bool,
    limit: usize,
) -> Result<usize, Box<dyn std::error::Error>> {

    let file = File::open(file_path)?;
    let buf_reader = BufReader::new(file);

    let mut rdr = ReaderBuilder::new()
        .has_headers(is_phishing_format)
        .from_reader(buf_reader);

    let mut count = 0;
    let mut records = rdr.records();

    while let Some(result) = records.next() {
        if limit > 0 && count >= limit { break; }

        let record = result?;
        let url_string;

        if is_phishing_format {
            url_string = record.get(1).unwrap_or("").to_string();
        } else {
            let col = if record.len() >= 2 { 1 } else { 0 };
            let domain = record.get(col).unwrap_or("");
            if domain.eq_ignore_ascii_case("domain") || domain.eq_ignore_ascii_case("url") { continue; }

            url_string = if !domain.starts_with("http") {
                format!("https://{}", domain)
            } else {
                domain.to_string()
            };
        }

        if url_string.is_empty() { continue; }

        let key = match Url::parse(&url_string) {
            Ok(u) => u.domain().unwrap_or("unknown").to_string(),
            Err(_) => "unknown".to_string(),
        };

        // Send to buffer
        if let Err((e, _)) = producer.send(
            BaseRecord::to(topic).payload(&url_string).key(&key),
        ) {
            eprintln!("Buffer Error: {:?}", e);
        }

        // Poll regularly to handle network callbacks
        if count % 1000 == 0 {
            producer.poll(Duration::from_millis(0));
            print!("\r> Queued {}...", count);
            std::io::stdout().flush()?;
        }
        count += 1;
    }
    println!();
    Ok(count)
}