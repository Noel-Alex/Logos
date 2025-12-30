// src/bin/merger.rs
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::statement::prepared::PreparedStatement;
// Fix 1: Import PoolSize and NonZeroUsize
use scylla::client::PoolSize;
use std::num::NonZeroUsize;
use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use futures::{stream, StreamExt};
use std::time::Instant;

// --- LUDICROUS SPEED SETTINGS ---
const CONCURRENT_DOMAINS: usize = 1024; // Process 1024 websites in parallel
const CONCURRENT_WRITES: usize = 256;   // 256 Async Inserts per website
const POOL_SIZE_PER_SHARD: usize = 16;  // 16 Connections per CPU Core

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let uri = env::var("SCYLLA_URI").unwrap_or_else(|_| "127.0.0.1:9042".to_string());
    println!("Connecting to Local Scylla at {}...", uri);

    // Fix 1: Correctly construct NonZeroUsize for the pool size
    let pool_size = NonZeroUsize::new(POOL_SIZE_PER_SHARD).unwrap();

    let session = SessionBuilder::new()
        .known_node(uri)
        .pool_size(PoolSize::PerShard(pool_size)) // Applied here
        .tcp_nodelay(true)
        .build()
        .await
        .expect("Connection failed");
    let session = Arc::new(session);

    println!("Preparing statements...");

    let get_labels_stmt = session.prepare("SELECT domain, is_phishing FROM Arachne.domain_labels").await.unwrap();

    let get_pages_stmt = Arc::new(session.prepare(
        "SELECT url, tag_sequence, http_status, crawled_at FROM Arachne.crawled_pages WHERE domain = ?"
    ).await.unwrap());

    let insert_new_stmt = Arc::new(session.prepare(
        "INSERT INTO Arachne.labeled_dataset (domain, url, tag_sequence, http_status, crawled_at, is_phishing) VALUES (?, ?, ?, ?, ?, ?)"
    ).await.unwrap());

    println!("🚀 LUDICROUS MODE ENGAGED");
    println!("   > Concurrency: {} Domains", CONCURRENT_DOMAINS);
    println!("   > Write Depth: {} per Domain", CONCURRENT_WRITES);

    let start = Instant::now();
    let total_processed = Arc::new(AtomicUsize::new(0));

    // 2. Stream Labels
    let mut rows_stream = session
        .execute_iter(get_labels_stmt, &[])
        .await
        .unwrap()
        .rows_stream::<(String, bool)>()
        .unwrap();

    // 3. Transform Stream into Async Work
    let processing_stream = rows_stream.map(|row_result| {
        let session = session.clone();
        let get_pages = get_pages_stmt.clone();
        let insert_new = insert_new_stmt.clone();
        let counter = total_processed.clone();

        async move {
            match row_result {
                Ok((domain, is_phishing)) => {
                    process_domain(&session, &get_pages, &insert_new, domain, is_phishing, &counter).await
                }
                Err(_) => 0,
            }
        }
    });

    // 4. Execute
    processing_stream
        .buffer_unordered(CONCURRENT_DOMAINS)
        .count()
        .await;

    println!("\n\n✨ Done! Created 'labeled_dataset' with {} rows.", total_processed.load(Ordering::Relaxed));
    println!("⏱ Total Time: {:.2?}", start.elapsed());
}

async fn process_domain(
    session: &Session,
    get_pages: &PreparedStatement,
    insert_new: &PreparedStatement,
    domain: String,
    is_phishing: bool,
    global_counter: &AtomicUsize
) -> usize {
    let mut local_count = 0;

    // A. Fetch Pages
    if let Ok(query_result) = session.execute_unpaged(get_pages, (&domain,)).await {

        // Fix 2: Use the working API pattern (into_rows_result -> rows::<T>)
        if let Ok(rows_result) = query_result.into_rows_result() {
            if let Ok(rows_iter) = rows_result.rows::<(String, String, i32, i64)>() {

                // Collect rows into memory so we can iterate them multiple times or move them into futures.
                // .flatten() automatically unwraps Ok() and skips Err() rows
                let rows: Vec<_> = rows_iter.flatten().collect();

                local_count = rows.len();

                if local_count > 0 {
                    // B. Write Pages Concurrently
                    // We create a stream of futures for the inserts
                    let insert_stream = stream::iter(rows).map(|(url, tag_seq, status, time)| {
                        let domain_clone = domain.clone();
                        async move {
                            let _ = session.execute_unpaged(
                                insert_new,
                                (domain_clone, url, tag_seq, status, time, is_phishing)
                            ).await;
                        }
                    });

                    // Execute writes for this domain in parallel (This makes it fast)
                    insert_stream.buffer_unordered(CONCURRENT_WRITES).count().await;

                    // Update stats
                    let total = global_counter.fetch_add(local_count, Ordering::Relaxed);
                    if total % 5000 < local_count {
                        use std::io::Write;
                        print!("\r> Total Merged: {} ... ", total + local_count);
                        let _ = std::io::stdout().flush();
                    }
                }
            }
        }
    }

    local_count
}