// bin/coordinatir.rs
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::message::Message;
use rdkafka::ClientConfig;
use tokio::time::{self, Duration};
use arachne::{CrawlResult, CrawlStatus, db};
use std::env;

#[tokio::main]
async fn main(){
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

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("queue.buffering.max.messages", "100000")
        .set("linger.ms", "1000")
        .set("batch.size", "6553600")
        .set("compression.type", "lz4")
        .set("acks", "1")
        .create()
        .expect("Producer creation failed");


    let record = FutureRecord::to(produce_topic).key(&source_url);
    loop {
        match consumer.recv().await{
            Err(e) => eprintln!("Kafka error: {}", e),
            Ok(m)=> {
                let payload = match m.payload_view::<str>(){
                    None => {
                        eprintln!("Message with empty payload");
                        continue;
                    }
                    Some(Ok(s)) => s,
                    Some(Err(e)) => {
                        eprintln!("Error viewing message payload as string");
                        continue;
                    }
                };


            }
        }
    }


}