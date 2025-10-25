use rdkafka::producer::{FutureProducer, FutureRecord, Producer};
use rdkafka::ClientConfig;
use std::time::Duration;
use url::Url;

/// Extracts the domain from a URL string.
/// This will be used as the Kafka message key.
fn get_domain_from_url(url_str: &str) -> Option<String> {
    Url::parse(url_str)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.to_string()))
}

/// Asynchronously produces messages to a Kafka topic.
///
/// # Arguments
/// * `brokers` - A comma-separated list of Kafka broker addresses.
/// * `topic_name` - The name of the topic to produce messages to.
/// * `urls` - A slice of URL strings to send as messages.
pub async fn produce(brokers: &str, topic_name: &str, urls: &[&str]) {
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("message.timeout.ms", "5000")
        .create()
        .expect("Producer creation error");

    println!("Starting producer...");

    for (i, url) in urls.iter().enumerate() {
        let key = match get_domain_from_url(url) {
            Some(domain) => domain,
            None => {
                eprintln!("Could not parse domain for URL: {}. Skipping.", url);
                continue;
            }
        };

        println!(
            "Preparing to send URL: '{}' with key: '{}'",
            url, key
        );

        let payload = *url;
        let record = FutureRecord::to(topic_name)
            .payload(payload)
            .key(&key); // <-- CRITICAL: Set the domain as the partition key

        match producer.send(record, Duration::from_secs(0)).await {
            Ok(delivery) => println!("Sent message {} -> {:?}", i, delivery),
            Err((e, _)) => eprintln!("Error sending message {}: {}", i, e),
        }
    }

    // Flush any buffered messages to ensure they are all sent
    println!("Flushing producer...");
    producer
        .flush(Duration::from_secs(5))
        .expect("Producer flush failed");
    println!("Producer finished.");
}