// src/bin/worker.rs
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::message::Message;
use rdkafka::ClientConfig;
use tokio::time::{self, Duration};
use arachne::{CrawlResult, CrawlStatus};
use std::env;

use reqwest::Client;
use scraper::{Html, Selector};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use url::{ParseError, Url};
use arachne::db;

#[derive(Debug, thiserror::Error)]
enum CrawlerError {
    #[error("Request error: {0}")]
    RequestError(#[from] reqwest::Error),
    #[error("URL parsing error: {0}")]
    UrlParseError(#[from] ParseError),
    #[error("Robots.txt parsing error")]
    RobotsParseError,
}


// This is a simplified parser. In reality, you'd use a crate like `scraper`.
fn parse_links(source_url: &str, html: &str) -> Vec<String> {
    // Basic placeholder link parsing
    // A real implementation needs to handle relative URLs, different tags, etc.
    println!("(Parsing links from {})", source_url);
    vec!["https://www.rust-lang.org/learn".to_string()] // Dummy data
}


#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    db::connect_to_db().await.expect("Scylla db connection failed");


    // --- Configuration ---
    let bootstrap_servers = env::var("KAFKA_SERVER").expect("KAFKA_SERVER not implemented");
    let consume_topic = "urls-to-crawl";
    let produce_topic = "crawl-results";
    let group_id = "arachne-worker-group";

    // --- Create Kafka Consumer ---
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", group_id)
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Consumer creation failed");

    consumer.subscribe(&[consume_topic]).expect("Can't subscribe");

    // --- Create Kafka Producer ---
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Producer creation failed");

    println!("Worker started. Waiting for URLs...");

    // --- Main Worker Loop ---
    loop {
        match consumer.recv().await {
            Ok(msg) => {
                let payload = match msg.payload_view::<str>() {
                    Some(Ok(s)) => s,
                    _ => {
                        eprintln!("Error reading message payload");
                        continue;
                    }
                };
                let url_to_fetch = payload.to_string();
                println!("RECEIVED URL: {}", url_to_fetch);

                // 1. FETCH
                let result = match reqwest::get(&url_to_fetch).await {
                    Ok(response) => {
                        if response.status().is_success() {
                            let body = response.text().await.unwrap_or_default();
                            // 2. PARSE (simplified)
                            let new_links = parse_links(&url_to_fetch, &body);
                            CrawlResult {
                                source_url: url_to_fetch,
                                status: CrawlStatus::Success,
                                content: Some(body), // You might not want to send the whole body back
                                discovered_urls: new_links,
                            }
                        } else {
                            CrawlResult {
                                source_url: url_to_fetch,
                                status: CrawlStatus::HttpError(response.status().as_u16()),
                                content: None,
                                discovered_urls: vec![],
                            }
                        }
                    }
                    Err(e) => CrawlResult {
                        source_url: url_to_fetch,
                        status: CrawlStatus::FetchError(e.to_string()),
                        content: None,
                        discovered_urls: vec![],
                    },
                };

                // 3. REPORT RESULTS
                let result_json = serde_json::to_string(&result).unwrap();
                let record = FutureRecord::to(produce_topic)
                    .payload(&result_json)
                    .key(&result.source_url); // Key by the original URL

                println!("REPORTING RESULT for {}", result.source_url);
                if let Err((e, _)) = producer.send(record, Duration::from_secs(0)).await {
                    eprintln!("Error sending result: {}", e);
                }
            }
            Err(e) => {
                eprintln!("Kafka error: {}", e);
            }
        }
    }
}