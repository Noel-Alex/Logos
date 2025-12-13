//db.rs
use anyhow::Result;
use scylla::statement::batch::Batch;
use scylla::statement::prepared::PreparedStatement;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use std::env;
use crate::{CrawlResult, CrawlStatus};
use futures::{stream, StreamExt};
use std::sync::{Arc, Mutex};
use std::collections::HashSet;



/// Establishes a connection to the database and returns a Session.
pub async fn connect_to_db() -> Result<Session> {
    let uri = env::var("SCYLLA_URI").unwrap_or_else(|_| "127.0.0.1:9042".to_string());
    println!("Connecting to ScyllaDB at {}...", uri);

    let session = SessionBuilder::new().known_node(uri).build().await?;

    println!("Connection successful.");

    setup_schema(&session).await.unwrap();
    Ok(session)
}

/// Sets up the necessary keyspace and table in the database.
pub async fn setup_schema(session: &Session) -> Result<()> {
    println!("Setting up database schema...");

    let keyspace_cql = "
        CREATE KEYSPACE IF NOT EXISTS Arachne
        WITH replication = {'class': 'SimpleStrategy', 'replication_factor': '1'}";

    let table_cql = "
        CREATE TABLE IF NOT EXISTS Arachne.crawled_pages (
            source_url TEXT PRIMARY KEY,
            content TEXT,
            http_status_code INT
        )";

    // Use the standard `query` method
    session.query_unpaged(keyspace_cql, &[]).await?;
    println!("Keyspace 'Arachne' is ready.");

    // Use the standard `query` method
    session.query_unpaged(table_cql, &[]).await?;
    println!("Table 'crawled_pages' is ready.");

    println!("Schema setup complete.");
    Ok(())
}

/// Inserts or updates a crawled page's data in the database.
pub async fn add_crawled_pages_concurrently(
    session: &Session,
    pages: &[CrawlResult],
    prepared: &PreparedStatement
) -> Result<()> {
    // We create a stream of futures
    let bodies = stream::iter(pages)
        .map(|page| {
            let values = (
                &page.source_url,
                &page.content,
                page.status.as_i32(),
            );
            session.execute_unpaged(prepared, values)
        })
        .buffer_unordered(100); // Execute 100 inserts in parallel

    // Await all results
    bodies.for_each(|res| async {
        if let Err(e) = res {
            eprintln!("Error inserting page: {}", e);
        }
    }).await;

    Ok(())
}

pub async fn check_existing_urls(
    session: &Session,
    urls: Vec<String>,
    prepared: &PreparedStatement
) -> Result<HashSet<String>> {

    let existing_urls = Arc::new(Mutex::new(HashSet::new()));

    let checks = stream::iter(urls)
        .map(|url| {
            let existing_clone = existing_urls.clone();
            async move {
                // Execute the check
                match session.execute_unpaged(prepared, (&url,)).await?.into_rows_result()? {
                    Ok(result) => {
                        // --- FIX IS HERE ---
                        // Instead of accessing result.rows, we try to iterate over typed results.
                        // We expect a single column of type String: (String,)
                        if let Ok(mut iter) = result.rows::<(String,)>() {
                            // If the iterator produces at least one item, the URL exists.
                            if iter.next().is_some() {
                                existing_clone.lock().unwrap().insert(url);
                            }
                        }
                    }
                    Err(e) => eprintln!("Error checking URL {}: {}", url, e),
                }
            }
        })
        .buffer_unordered(100); // Process 100 reads in parallel

    checks.collect::<()>().await;

    let result = Arc::try_unwrap(existing_urls).unwrap().into_inner().unwrap();
    Ok(result)
}
/*
#[tokio::main]
async fn main() -> Result<()> {
    let session = connect_to_db().await?;

    // --- DEMONSTRATION ---
    let page1 = CrawledPage {
        source_url: "https://example.com/".to_string(),
        content: "<html><body><h1>Welcome!</h1></body></html>".to_string(),
        content_type: "text/html".to_string(),
        http_status_code: 200,
    };
    add_crawled_page(&session, &page1).await?;

    let page2 = CrawledPage {
        source_url: "https://example.com/non-existent".to_string(),
        content: "".to_string(),
        content_type: "text/plain".to_string(),
        http_status_code: 404,
    };
    add_crawled_page(&session, &page2).await?;

    let page3 = CrawledPage {
        source_url: "https://example.com/large-image.jpg".to_string(),
        content: "s3://my-crawl-bucket/images/large-image.jpg".to_string(),
        content_type: "s3_link/jpeg".to_string(),
        http_status_code: 200,
    };
    add_crawled_page(&session, &page3).await?;

    println!("\nScript finished successfully.");
    Ok(())
}
*/
