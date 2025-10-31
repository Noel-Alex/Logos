use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::message::Message;
use rdkafka::ClientConfig;
use tokio::time::{self, Duration};
use arachne::{CrawlResult, CrawlStatus};
use std::env;

#[tokio::main]
fn main(){
    dotenvy::dotenv().ok();
    // Assuming connect_to_db is an async function you have defined
    // db::connect_to_db().await.expect("Scylla db connection failed");

    // --- Configuration ---
    let bootstrap_servers = env::var("KAFKA_SERVER").expect("KAFKA_SERVER not in .env");
    let consume_topic = "crawl-results";
    let produce_topic = "urls-to-crawl";
    let group_id = "arachne-worker-group";

    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", group_id)
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .create()
        .expect("Consumer creation failed");

    consumer.subscribe(&[consume_topic]).expect("Can't subscribe");


}