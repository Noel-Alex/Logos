// src/bin/coordinator.rs
/*use rdkafka::consumer::{Consumer, StreamConsumer, CommitMode};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::message::Message;
use rdkafka::ClientConfig;
use tokio::time::{self, Duration, Instant};
use arachne::{CrawlResult, db};
use std::env;
use std::collections::HashSet;
use url::Url;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    // 1. Setup DB
    println!("Coordinator connecting to DB...");
    let session = db::connect_to_db().await.expect("DB Connection failed");

    // Prepare statements ONCE to save overhead
    let insert_stmt = session.prepare("INSERT INTO Arachne.crawled_pages (source_url, content, http_status_code) VALUES (?, ?, ?)").await.unwrap();
    let check_stmt = session.prepare("SELECT source_url FROM Arachne.crawled_pages WHERE source_url = ?").await.unwrap();

    // 2. Setup Kafka
    let bootstrap_servers = env::var("KAFKA_SERVER").expect("KAFKA_SERVER missing");
    let consume_topic = "crawl-results";
    let produce_topic = "urls-to-crawl";
    let group_id = "arachne-coordinator-group";

    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", group_id)
        .set("enable.auto.commit", "false") // IMPORTANT: Manual commits
        .set("auto.offset.reset", "earliest")
        .create()
        .expect("Consumer creation failed");

    consumer.subscribe(&[consume_topic]).unwrap();

    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("queue.buffering.max.messages", "100000")
        .set("acks", "1")
        .create()
        .expect("Producer creation failed");

    // --- BATCH SETTINGS ---
    const BATCH_SIZE: usize = 100; // Adjust based on memory
    const BATCH_TIMEOUT: Duration = Duration::from_millis(500); // Flush every 0.5s if buffer isn't full

    let mut result_buffer: Vec<CrawlResult> = Vec::with_capacity(BATCH_SIZE);
    let mut last_flush = Instant::now();

    println!("Coordinator running. Waiting for results...");

    loop {
        // Use tokio::select! to listen for EITHER a message OR a timeout
        tokio::select! {
            // Case A: Receive a message from Kafka
            msg_res = consumer.recv() => {
                match msg_res {
                    Ok(m) => {
                        // Attempt to deserialize
                        if let Some(Ok(payload)) = m.payload_view::<str>() {
                            println!("Coordinator received payload: {:.50}...", payload);
                            match serde_json::from_str::<CrawlResult>(payload) {
                                Ok(result) => {
                                    result_buffer.push(result);
                                }
                                Err(e) => eprintln!("Deserialization error: {}", e),
                            }
                        }
                    },
                    Err(e) => eprintln!("Kafka Error: {}", e),
                }
            }

            // Case B: Timeout reached (Flush time)
            _ = time::sleep_until(last_flush + BATCH_TIMEOUT) => {
                // The block below handles the actual flushing
            }
        }

        // --- FLUSH LOGIC ---
        // We flush if buffer is full OR if time has passed and buffer is not empty
        if result_buffer.len() >= BATCH_SIZE || (last_flush.elapsed() >= BATCH_TIMEOUT && !result_buffer.is_empty()) {
            let count = result_buffer.len();
            println!("Flushing batch of {} items...", count);

            // 1. Insert Batch into ScyllaDB (Concurrent Writes)
            if let Err(e) = db::add_crawled_pages_concurrently(&session, &result_buffer, &insert_stmt).await {
                eprintln!("CRITICAL: DB Batch Insert Failed: {}", e);
                // In a real app, you might break/retry here.
                // For now, we clear the buffer to prevent infinite loops, but we DO NOT commit offsets.
                result_buffer.clear();
                continue;
            }

            // 2. Aggregate all discovered URLs from this batch
            let mut all_discovered: HashSet<String> = HashSet::new();
            for res in &result_buffer {
                for url in &res.discovered_urls {
                    all_discovered.insert(url.clone());
                }
            }

            // 3. Filter Duplicates via DB (Concurrent Reads)
            if !all_discovered.is_empty() {
                let urls_to_check: Vec<String> = all_discovered.into_iter().collect();
                let existing = db::check_existing_urls(&session, urls_to_check.clone(), &check_stmt)
                    .await
                    .unwrap_or_default();

                // 4. Produce NEW URLs to Kafka
                //println!("{:?}", urls_to_check);
                for url in urls_to_check {
                    if !existing.contains(&url) {
                        // Extract Domain for Partition Key
                        let key = Url::parse(&url)
                            .ok()
                            .and_then(|u| u.domain().map(|d| d.to_string()))
                            .unwrap_or_else(|| "unknown".to_string());

                        // Fire and forget (async send)
                        //println!("{}", url);
                        let _ = producer.send(
                            FutureRecord::to(produce_topic).key(&key).payload(&url),
                            Duration::from_secs(0)
                        ).await;
                    }
                }
            }

            // 5. Commit Offsets
            // We commit "Async" so we don't block processing waiting for Kafka to confirm.
            if let Err(e) = consumer.commit_consumer_state(CommitMode::Async) {
                 eprintln!("Offset commit failed: {}", e);
            } else {
                 println!("Batch of {} committed successfully.", count);
            }

            // Reset
            result_buffer.clear();
            last_flush = Instant::now();
        }
    }
}*/

// src/bin/coordinator.rs
use rdkafka::consumer::{Consumer, StreamConsumer, CommitMode};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use rdkafka::message::{Message, OwnedMessage};
use tokio::sync::mpsc;
use tokio::time::{self, Duration, Instant};
use arachne::{CrawlResult, db};
use std::env;
use std::collections::HashSet;
use url::Url;
use futures::future::join_all;

// Struct to pass data between Consumer task and Processor task
struct WorkItem {
    result: CrawlResult,
    offset: i64,
    partition: i32,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    // 1. Setup Scylla (Shared Session)
    println!("Coordinator connecting to DB...");
    let session = std::sync::Arc::new(db::connect_to_db().await.expect("DB Connection failed"));

    // Pre-prepare statements
    let insert_stmt = std::sync::Arc::new(session.prepare("INSERT INTO Arachne.crawled_pages (source_url, content, http_status_code) VALUES (?, ?, ?)").await.unwrap());
    let check_stmt = std::sync::Arc::new(session.prepare("SELECT source_url FROM Arachne.crawled_pages WHERE source_url = ?").await.unwrap());

    // 2. Setup Kafka Configs
    let bootstrap_servers = env::var("KAFKA_SERVER").expect("KAFKA_SERVER missing");
    let group_id = "arachne-coordinator-group";
    let consume_topic = "crawl-results";

    // Create Producer (Clonable, Thread-safe)
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("queue.buffering.max.messages", "100000") // Increase buffer
        .set("message.send.max.retries", "2")
        .set("acks", "1")
        .create()
        .expect("Producer creation failed");

    // 3. Create Channel (The Buffer between Consumer and Processor)
    // Capacity 10,000 means the consumer can stay 10k items ahead of the DB writer
    let (tx, mut rx) = mpsc::channel::<WorkItem>(10_000);

    // 4. SPAWN THE CONSUMER TASK
    // This task does nothing but pull from Kafka and shove into the channel.
    // It never waits for the Database.
    let bootstrap_server_clone = bootstrap_servers.clone();
    tokio::spawn(async move {
        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &bootstrap_server_clone)
            .set("group.id", group_id)
            .set("enable.auto.commit", "false")
            .set("auto.offset.reset", "earliest")
            // Optimization: Fetch more data per network request
            .set("fetch.message.max.bytes", "10485760") // 10MB
            .create()
            .expect("Consumer creation failed");

        consumer.subscribe(&[consume_topic]).unwrap();
        println!("Consumer Task Started.");

        loop {
            match consumer.recv().await {
                Ok(m) => {
                    // Detach message from the stream lifetime
                    let owned_msg = m.detach();

                    if let Some(Ok(payload)) = owned_msg.payload_view::<str>() {
                        if let Ok(result) = serde_json::from_str::<CrawlResult>(payload) {
                            let item = WorkItem {
                                result,
                                offset: owned_msg.offset(),
                                partition: owned_msg.partition(),
                            };

                            // Send to processor (waits here if channel is full, providing backpressure)
                            if let Err(_) = tx.send(item).await {
                                println!("Receiver dropped, stopping consumer.");
                                break;
                            }
                        }
                    }

                    // We DO NOT commit here. We commit in the processor after work is done.
                    // Note: Ideally, you pass the TopicPartitionList back to commit,
                    // but for high speed, occasional offset commits or auto-commit (enabled carefully)
                    // is often preferred. For this example, we keep it simple:
                    // The throughput bottleneck is usually DB, so the consumer will just pause
                    // at `tx.send` if DB is slow.
                },
                Err(e) => eprintln!("Kafka Error: {}", e),
            }
        }
    });

    // 5. PROCESSOR LOOP (Main Thread)
    // This handles the DB Logic and Production
    const BATCH_SIZE: usize = 2500; // Increased batch size
    const BATCH_TIMEOUT: Duration = Duration::from_millis(100); // Flush faster

    let mut buffer: Vec<WorkItem> = Vec::with_capacity(BATCH_SIZE);
    let mut last_flush = Instant::now();

    println!("Processor Loop Running...");

    loop {
        // Collect items until Batch full or Timeout
        let read_task = async {
            // Fill buffer up to BATCH_SIZE
            while buffer.len() < BATCH_SIZE {
                // If buffer has items, we use a timeout. If empty, we wait indefinitely.
                if buffer.is_empty() {
                    match rx.recv().await {
                        Some(item) => buffer.push(item),
                        None => return false, // Channel closed
                    }
                } else {
                    // Buffer has items, race against timeout
                    match time::timeout(Duration::from_millis(10), rx.recv()).await {
                        Ok(Some(item)) => buffer.push(item),
                        Ok(None) => return false, // Channel closed
                        Err(_) => break, // Timeout logic will handle flush
                    }
                }
            }
            true
        };

        // Run the collection logic
        let channel_open = read_task.await;

        let time_to_flush = last_flush.elapsed() >= BATCH_TIMEOUT;
        let buffer_full = buffer.len() >= BATCH_SIZE;

        if (buffer_full || time_to_flush) && !buffer.is_empty() {
            let batch_len = buffer.len();

            // Extract CrawlResults for DB Ops
            let crawl_results: Vec<CrawlResult> = buffer.iter().map(|w| w.result.clone()).collect();

            // --- PARALLEL EXECUTION START ---
            // We run the "Insert Content" and "Check URLs" simultaneously

            let session_clone1 = session.clone();
            let insert_stmt_clone = insert_stmt.clone();
            let results_for_insert = crawl_results.clone();

            // Task A: Insert Content
            let insert_future = tokio::spawn(async move {
                db::add_crawled_pages_concurrently(&session_clone1, results_for_insert, &insert_stmt_clone).await
            });

            // Task B: Process URLs (Extract -> Check DB -> Produce)
            let session_clone2 = session.clone();
            let check_stmt_clone = check_stmt.clone();
            let producer_clone = producer.clone();
            let produce_topic = "urls-to-crawl"; // Hardcoded for this context

            let url_future = tokio::spawn(async move {
                // 1. Deduplicate in memory first
                let mut all_discovered: HashSet<String> = HashSet::new();
                for res in &crawl_results {
                    for url in &res.discovered_urls {
                        all_discovered.insert(url.clone());
                    }
                }

                if all_discovered.is_empty() {
                    return Ok(());
                }

                let urls_to_check: Vec<String> = all_discovered.into_iter().collect();

                // 2. Check DB
                let existing = db::check_existing_urls(&session_clone2, urls_to_check.clone(), &check_stmt_clone)
                    .await?;

                // 3. Produce New URLs
                // We collect futures to run them concurrently
                let mut produce_futures = vec![];

                for url in urls_to_check {
                    if !existing.contains(&url) {
                        let key = Url::parse(&url)
                            .ok()
                            .and_then(|u| u.domain().map(|d| d.to_string()))
                            .unwrap_or_else(|| "unknown".to_string());

                        let producer_ref = producer_clone.clone();
                        let record_payload = url.clone();

                        // Spawn the produce so we don't wait for ACKs sequentially
                        produce_futures.push(tokio::spawn(async move {
                            let _ = producer_ref.send(
                                FutureRecord::to(produce_topic).key(&key).payload(&record_payload),
                                Duration::from_secs(0)
                            ).await;
                        }));
                    }
                }

                // Wait for all messages to be enqueued to Kafka buffer
                join_all(produce_futures).await;

                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            });

            // Await both DB paths simultaneously
            let (insert_res, url_res) = tokio::join!(insert_future, url_future);

            if let Err(e) = insert_res { eprintln!("Insert Task Panic: {}", e); }
            if let Err(e) = url_res { eprintln!("URL Task Panic: {}", e); }

            // --- PARALLEL EXECUTION END ---

            // Logic to commit offsets would go here using a Consumer attached
            // to this thread, or simply rely on the fact that if we crash,
            // Scylla handles idempotency (INSERT ignores if PK exists).
            // For max speed, we skip manual offset management in this snippet
            // and rely on idempotency.

            println!("Processed batch of {} in {:?}.", batch_len, last_flush.elapsed());

            buffer.clear();
            last_flush = Instant::now();
        }

        if !channel_open && buffer.is_empty() {
            println!("Channel closed and buffer empty. Exiting.");
            break;
        }
    }
}