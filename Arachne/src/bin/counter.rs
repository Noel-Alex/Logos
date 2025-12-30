// src/bin/counter.rs
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use std::env;
use std::sync::Arc;
use futures::StreamExt;
use std::time::Instant;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let uri = env::var("SCYLLA_URI").unwrap_or_else(|_| "127.0.0.1:9042".to_string());
    println!("Connecting to Scylla...");

    let session = SessionBuilder::new().known_node(uri).build().await.expect("Connection failed");
    let session = Arc::new(session);

    println!("Starting Count...");
    let start = Instant::now();

    // We select ONLY the columns we need to check to make it fast
    // We don't filter in the DB (ALLOW FILTERING), we stream and filter here.
    let stmt = session.prepare("SELECT is_phishing, http_status FROM Arachne.labeled_dataset").await.unwrap();

    let mut rows_stream = session
        .execute_iter(stmt, &[])
        .await
        .unwrap()
        .rows_stream::<(bool, i32)>() // Typed tuple: (is_phishing, http_status)
        .unwrap();

    let mut count = 0;
    let mut total_scanned = 0;

    while let Some(row_res) = rows_stream.next().await {
        if let Ok((is_phishing, status)) = row_res {
            total_scanned += 1;

            // --- THE FILTER LOGIC ---
            if is_phishing == true && status == 200 {
                count += 1;
            }
        }
    }

    println!("\n---------------- RESULTS ----------------");
    println!("Total Rows Scanned: {}", total_scanned);
    println!("Phishing Sites (200 OK): {}", count);
    println!("Time Elapsed: {:.2?}", start.elapsed());
}