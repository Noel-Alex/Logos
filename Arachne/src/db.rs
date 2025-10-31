use anyhow::Result;
use scylla::statement::batch::Batch;
use scylla::statement::prepared::PreparedStatement;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use std::env;
use crate::{CrawlResult, CrawlStatus};



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
pub async fn add_crawled_page(session: &Session, page: &CrawlResult) -> Result<()> {
    let insert_cql = "
        INSERT INTO Arachne.crawled_pages
        (source_url, content, http_status_code)
        VALUES (?, ?, ?)";

    let prepared = session.prepare(insert_cql).await?;

    // This now works because the "chrono" feature is enabled in Cargo.toml
    let values = (
        &page.source_url,
        &page.content,
        &page.status,
    );

    // Use the standard `execute` method
    session.execute_unpaged(&prepared, values).await?;

    println!("Successfully inserted data for URL: {}", page.source_url);
    Ok(())
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
