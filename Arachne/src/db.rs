// src/db.rs
use crate::CrawlResult;
use anyhow::Result;
use futures::{stream, StreamExt};
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::statement::prepared::PreparedStatement;
use std::collections::{HashMap, HashSet};
use std::env;
use std::time::Duration;

pub struct ArachneRepo {
    session: Session,
    insert_stmt: PreparedStatement,
    get_count_stmt: PreparedStatement,
    check_exist_stmt: PreparedStatement,
}

impl ArachneRepo {
    pub async fn new() -> Result<Self> {
        let uri = env::var("SCYLLA_URI").unwrap_or_else(|_| "127.0.0.1:9042".to_string());
        println!("Connecting to ScyllaDB at {}...", uri);

        let session = SessionBuilder::new().known_node(uri).build().await?;

        // 1. SETUP SCHEMA (With Safety Waits)
        setup_schema(&session).await?;

        println!("Preparing statements...");

        // 2. PREPARE STATEMENTS
        // We explicitly use the keyspace prefix to be safe
        let insert_stmt = session.prepare(
            "INSERT INTO Arachne.crawled_pages (domain, url, tag_sequence, http_status, crawled_at) VALUES (?, ?, ?, ?, ?)"
        ).await?;

        let get_count_stmt = session.prepare(
            "SELECT page_count FROM Arachne.domain_stats WHERE domain = ?"
        ).await?;

        let check_exist_stmt = session.prepare(
            "SELECT url FROM Arachne.crawled_pages WHERE domain = ? AND url = ?"
        ).await?;

        Ok(Self {
            session,
            insert_stmt,
            get_count_stmt,
            check_exist_stmt,
        })
    }

    pub async fn get_domain_counts(&self, domains: Vec<String>) -> Result<HashMap<String, i64>> {
        const CONCURRENCY_LIMIT: usize = 64;
        let results = stream::iter(domains)
            .map(|domain| async move {
                let result = self.session.execute_unpaged(&self.get_count_stmt, (domain.clone(),)).await;
                match result {
                    Ok(res) => {
                        match res.into_rows_result().ok().and_then(|r| r.first_row::<(i64,)>().ok()) {
                            Some((count,)) => Some((domain, count)),
                            None => Some((domain, 0)),
                        }
                    },
                    Err(_) => None,
                }
            })
            .buffer_unordered(CONCURRENCY_LIMIT)
            .filter_map(|res| async { res })
            .collect::<HashMap<String, i64>>()
            .await;
        Ok(results)
    }

    pub async fn insert_pages(&self, pages: Vec<(String, CrawlResult)>) -> Result<()> {
        const CONCURRENCY_LIMIT: usize = 256;
        stream::iter(pages)
            .map(|(domain, page)| {
                let values = (
                    domain,
                    page.source_url,
                    page.content.unwrap_or_default(),
                    page.status.as_i32(),
                    chrono::Utc::now().timestamp_millis(),
                );
                // We clone the statement reference for the async block
                let stmt = &self.insert_stmt;
                let session = &self.session;
                async move {
                    session.execute_unpaged(stmt, values).await
                }
            })
            .buffer_unordered(CONCURRENCY_LIMIT)
            .for_each(|res| async {
                if let Err(e) = res {
                    eprintln!("❌ DB Insert Error: {}", e);
                }
            }).await;
        Ok(())
    }

    pub async fn increment_domain_counts(&self, increments: HashMap<String, i64>) -> Result<()> {
        const CONCURRENCY_LIMIT: usize = 64;
        stream::iter(increments)
            .map(|(domain, count)| {
                // Safe because count is i64
                let query = format!("UPDATE Arachne.domain_stats SET page_count = page_count + {} WHERE domain = ?", count);
                let session = &self.session;
                async move {
                    session.query_unpaged(query, (domain,)).await
                }
            })
            .buffer_unordered(CONCURRENCY_LIMIT)
            .for_each(|res| async {
                if let Err(e) = res {
                    eprintln!("❌ DB Counter Error: {}", e);
                }
            }).await;
        Ok(())
    }

    pub async fn check_existing_urls(&self, url_pairs: Vec<(String, String)>) -> Result<HashSet<String>> {
        const CONCURRENCY_LIMIT: usize = 256;
        let results = stream::iter(url_pairs)
            .map(|(domain, url)| async move {
                let exec = self.session.execute_unpaged(&self.check_exist_stmt, (domain, url.clone())).await;
                match exec {
                    Ok(res) => {
                         match res.into_rows_result() {
                            Ok(rows) if rows.rows_num() > 0 => Some(url),
                            _ => None
                        }
                    },
                    Err(_) => None
                }
            })
            .buffer_unordered(CONCURRENCY_LIMIT)
            .filter_map(|res| async { res })
            .collect::<HashSet<String>>()
            .await;
        Ok(results)
    }
}

async fn setup_schema(session: &Session) -> Result<()> {
    println!("Initializing Schema...");

    // 1. Create Keyspace
    let keyspace_cql = "CREATE KEYSPACE IF NOT EXISTS Arachne WITH replication = {'class': 'SimpleStrategy', 'replication_factor': '1'}";
    session.query_unpaged(keyspace_cql, &[]).await?;

    // CRITICAL FIX: Wait for the keyspace to exist before creating tables
    session.await_schema_agreement().await?;

    // 2. Create Pages Table
    let pages_table = "
        CREATE TABLE IF NOT EXISTS Arachne.crawled_pages (
            domain TEXT,
            url TEXT,
            tag_sequence TEXT,
            http_status INT,
            crawled_at BIGINT,
            PRIMARY KEY ((domain), url)
        )";
    session.query_unpaged(pages_table, &[]).await?;

    // 3. Create Stats Table
    let stats_table = "CREATE TABLE IF NOT EXISTS Arachne.domain_stats (domain TEXT PRIMARY KEY, page_count COUNTER)";
    session.query_unpaged(stats_table, &[]).await?;

    // Wait for tables to exist before returning
    session.await_schema_agreement().await?;

    println!("Schema Initialized.");
    Ok(())
}