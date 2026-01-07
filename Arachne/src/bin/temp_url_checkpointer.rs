use anyhow::Result;
use arrow_array::{RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use futures::StreamExt;
use parquet::arrow::AsyncArrowWriter;
use parquet::basic::{Compression, Encoding};
use parquet::file::properties::WriterProperties;
use rdkafka::config::ClientConfig;
use rdkafka::consumer::{Consumer, StreamConsumer};
use rdkafka::message::Message;
use std::fs::File;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

// --- CONFIGURATION ---
const KAFKA_BROKERS: &str = "127.0.0.1:9093";
const TOPIC: &str = "urls-to-crawl";
const CONSUMER_GROUP: &str = "parquet_archiver_v2";
const PARQUET_OUTPUT: &str = "kafka_dump.parquet";

// BATCH SETTINGS
const BATCH_SIZE: usize = 100_000;
const IDLE_TIMEOUT_SECS: u64 = 5;

#[tokio::main]
async fn main() -> Result<()> {
    println!("Init: Connecting to Redpanda at {}...", KAFKA_BROKERS);

    // 1. Setup Kafka Consumer
    let consumer: StreamConsumer = ClientConfig::new()
        .set("group.id", CONSUMER_GROUP)
        .set("bootstrap.servers", KAFKA_BROKERS)
        .set("enable.partition.eof", "false")
        .set("session.timeout.ms", "6000")
        .set("enable.auto.commit", "true")
        // Important: Start from beginning if we haven't seen this group before
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Consumer creation failed");

    consumer
        .subscribe(&[TOPIC])
        .expect("Can't subscribe to topic");

    let schema = Arc::new(Schema::new(vec![Field::new("url", DataType::Utf8, false)]));

    let (tx, mut rx) = mpsc::channel::<RecordBatch>(10);
    let schema_clone = schema.clone();

    let writer_handle = tokio::spawn(async move {
        println!("Writer: Creating Parquet file...");
        let file = File::create(PARQUET_OUTPUT).expect("Failed to create file");

        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .set_encoding(Encoding::PLAIN)
            .set_max_row_group_size(BATCH_SIZE)
            .build();

        let mut writer =
            AsyncArrowWriter::try_new(tokio::fs::File::from(file), schema_clone, Some(props))
                .expect("Failed to create arrow writer");

        let mut total_rows = 0;

        while let Some(batch) = rx.recv().await {
            let batch_rows = batch.num_rows();
            if batch_rows > 0 {
                writer.write(&batch).await.expect("Failed to write batch");
                total_rows += batch_rows;
                println!(
                    "Writer: >> Flushed {} rows (Total: {})",
                    batch_rows, total_rows
                );
            }
        }

        writer
            .close()
            .await
            .expect("Failed to finalize parquet file");
        println!("Writer: CLOSED. Total written: {}", total_rows);
    });

    println!("Reader: Consuming topic '{}'...", TOPIC);

    let mut url_buffer = Vec::with_capacity(BATCH_SIZE);
    let mut stream = consumer.stream();

    loop {
        let msg_result = timeout(Duration::from_secs(IDLE_TIMEOUT_SECS), stream.next()).await;

        match msg_result {
            Ok(Some(Ok(borrowed_message))) => {
                if let Some(payload) = borrowed_message.payload_view::<str>() {
                    if let Ok(url_str) = payload {
                        url_buffer.push(url_str.to_string());
                    }
                }

                if url_buffer.len() >= BATCH_SIZE {
                    send_batch(&tx, &schema, &mut url_buffer).await;
                }
            }

            Ok(Some(Err(e))) => eprintln!("Kafka error: {}", e),

            Ok(None) => break,

            Err(_) => {
                println!(
                    "Reader: No messages for {}s. Assuming topic drained.",
                    IDLE_TIMEOUT_SECS
                );
                break;
            }
        }
    }

    if !url_buffer.is_empty() {
        println!("Reader: Flushing final {} items...", url_buffer.len());
        send_batch(&tx, &schema, &mut url_buffer).await;
    }

    drop(tx);

    writer_handle.await?;

    println!("Done.");
    Ok(())
}

async fn send_batch(tx: &mpsc::Sender<RecordBatch>, schema: &Arc<Schema>, urls: &mut Vec<String>) {
    let url_array = StringArray::from(std::mem::take(urls));

    let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(url_array)])
        .expect("Failed to create batch");

    if tx.send(batch).await.is_err() {
        eprintln!("Receiver dropped, stopping reader.");
    }
}
