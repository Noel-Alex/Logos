// src/main.rs

// Declare the producer and consumer modules

mod consumer;
mod producer;

// Use tokio for our async main function
#[tokio::main]
async fn main() {
    // --- Configuration ---
    let bootstrap_servers = "localhost:9093";
    let topic_name = "urls-to-crawl";
    let group_id = "arachne-worker-group-1";

    // A sample list of URLs to crawl
    // Note the different domains, which will be used as partition keys
    let urls_to_process = vec![
        "https://www.rust-lang.org/",
        "https://en.wikipedia.org/wiki/Rust_(programming_language)",
        "https://github.com/rust-lang/rust",
        "https://www.reddit.com/r/rust/",
        "https://en.wikipedia.org/wiki/Concurrency",
        "https://www.rust-lang.org/community",
    ];

    println!("--- Arachne Kafka Demo ---");

    // --- Run the Producer ---
    // In a real application, the producer (Coordinator) would run in a separate process.
    producer::produce(bootstrap_servers, topic_name, &urls_to_process).await;

    println!("\n----------------------------------\n");

    // --- Run the Consumer ---
    // In a real application, the consumers (Workers) would be a separate, scalable fleet.
    consumer::consume(bootstrap_servers, group_id, topic_name).await;

    println!("\n--- Demo Finished ---");
}

