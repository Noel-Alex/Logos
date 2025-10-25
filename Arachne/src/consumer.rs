// src/consumer.rs

use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::Message;
use rdkafka::ClientConfig;
use tokio::time::{self, Duration};

/// Asynchronously consumes messages from a Kafka topic.
///
/// # Arguments
/// * `brokers` - A comma-separated list of Kafka broker addresses.
/// * `group_id` - The consumer group ID.
/// * `topic_name` - The name of the topic to subscribe to.
pub async fn consume(brokers: &str, group_id: &str, topic_name: &str) {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", brokers)
        .set("group.id", group_id)
        .set("enable.partition.eof", "false")
        .set("session.timeout.ms", "6000")
        .set("auto.offset.reset", "earliest") // Start reading from the beginning of the topic
        .create()
        .expect("Consumer creation failed");

    consumer
        .subscribe(&[topic_name])
        .expect("Can't subscribe to specified topic");

    println!("Starting consumer... Waiting for messages.");
    println!("(Will stop after 10 seconds of inactivity)");

    // The consumer stream will block until a message is available.
    // We add a timeout to stop the demo gracefully.
    loop {
        match time::timeout(Duration::from_secs(10), consumer.recv()).await {
            Ok(Ok(msg)) => {
                let key = msg.key().and_then(|k| std::str::from_utf8(k).ok()).unwrap_or("N/A");
                let payload = msg.payload_view::<str>().unwrap_or(Ok("N/A")).unwrap_or("N/A");

                println!(
                    "WORKER RECEIVED: topic='{}', partition={}, offset={}, key='{}', payload='{}'",
                    msg.topic(),
                    msg.partition(),
                    msg.offset(),
                    key,
                    payload
                );
            }
            Ok(Err(e)) => {
                eprintln!("Kafka error: {}", e);
            }
            Err(_) => {
                // Timeout occurred
                println!("No messages received for 10 seconds. Shutting down consumer.");
                break;
            }
        }
    }
}