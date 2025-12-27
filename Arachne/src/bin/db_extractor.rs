use anyhow::Result;
use arrow_array::{Int32Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use futures::stream::{self, StreamExt};
use parquet::arrow::AsyncArrowWriter;
use parquet::basic::{Compression, Encoding};
use parquet::file::properties::WriterProperties;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use std::fs::File;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Instant;

// --- CONFIGURATION ---
const SCYLLA_URI: &str = "127.0.0.1:9042";
const KEYSPACE: &str = "Arachne";
const TABLE: &str = "crawled_pages";
const PARQUET_OUTPUT: &str = "crawled_data.parquet";

// STABILITY SETTINGS
// 1 Thread = No fighting for resources.
// It is safer to run 1 stable stream than 16 crashing ones.
const PARALLELISM: usize = 128;

const BATCH_SIZE: usize = 500;

#[tokio::main]
async fn main() -> Result<()> {
    let start_time = Instant::now();
    println!("Init: Connecting to ScyllaDB at {}...", SCYLLA_URI);

    let session = SessionBuilder::new()
        .known_node(SCYLLA_URI)
        .compression(None)
        .build()
        .await?;
    let session = Arc::new(session);

    println!("Init: Connected. Preparing schema...");

    let schema = Arc::new(Schema::new(vec![
        Field::new("source_url", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, true),
        Field::new("http_status_code", DataType::Int32, true),
    ]));

    // Capacity 1 means: If Writer has 1 batch, Reader MUST STOP.
    let (tx, mut rx) = mpsc::channel::<RecordBatch>(PARALLELISM * 2);

    let schema_clone = schema.clone();

    // --- WRITER TASK ---
    let writer_handle = tokio::spawn(async move {
        println!("Writer: Creating file...");
        let file = File::create(PARQUET_OUTPUT).expect("Failed to create file");

        // --- CRITICAL FIX: ROW GROUP SIZE ---
        // By default, Parquet waits for 1024*1024 rows before writing.
        // We force it to write every BATCH_SIZE (50 rows) to keep RAM usage low.
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .set_max_row_group_size(BATCH_SIZE) // <--- THIS SAVES YOUR RAM
            .build();

        let mut writer =
            AsyncArrowWriter::try_new(tokio::fs::File::from(file), schema_clone, Some(props))
                .expect("Failed to create arrow writer");

        let mut total_rows = 0;

        while let Some(batch) = rx.recv().await {
            let batch_rows = batch.num_rows();
            if batch_rows == 0 {
                continue;
            }

            // Write to internal buffer
            writer.write(&batch).await.expect("Failed to write batch");

            // Note: The 'set_max_row_group_size' above ensures this actually
            // flushes to disk frequently.

            total_rows += batch_rows;
            println!(
                "Writer: >> Flushed {} rows to disk (Total: {})",
                batch_rows, total_rows
            );
        }

        // Force final flush
        writer.close().await.expect("Failed to close writer");
        println!("Writer: FINISHED. Total rows written: {}", total_rows);
    });

    // --- READER SETUP ---
    let ranges = generate_token_ranges(PARALLELISM);
    println!("Starting scan with {} parallel streams...", PARALLELISM);

    let tasks = stream::iter(ranges).map(|(start, end)| {
        let session = session.clone();
        let tx = tx.clone();
        let schema = schema.clone();

        tokio::spawn(async move { process_token_range(session, start, end, tx, schema).await })
    });

    tasks
        .buffer_unordered(PARALLELISM)
        .collect::<Vec<_>>()
        .await;

    println!("All readers finished. Closing channel...");
    drop(tx); // Signal writer to close file

    writer_handle.await?;
    println!("Job Complete. Time taken: {:.2?}", start_time.elapsed());
    Ok(())
}

async fn process_token_range(
    session: Arc<Session>,
    start_token: i64,
    end_token: i64,
    tx: mpsc::Sender<RecordBatch>,
    schema: Arc<Schema>,
) {
    let query = format!(
        "SELECT source_url, content, http_status_code FROM {}.{} WHERE token(source_url) >= ? AND token(source_url) < ?",
        KEYSPACE, TABLE
    );

    let mut prepared = match session.prepare(query.as_str()).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Prepare error: {:?}", e);
            return;
        }
    };

    // CRITICAL FIX: PAGING
    // Limit network fetch to 50 rows at a time.
    prepared.set_page_size(BATCH_SIZE as i32);

    let mut rows_stream = session
        .execute_iter(prepared, (start_token, end_token))
        .await
        .expect("Query execution failed")
        .rows_stream::<(String, Option<String>, Option<i32>)>()
        .expect("Type casting failed");

    let mut urls = Vec::with_capacity(BATCH_SIZE);
    let mut contents = Vec::with_capacity(BATCH_SIZE);
    let mut codes = Vec::with_capacity(BATCH_SIZE);

    while let Some(row_result) = rows_stream.next().await {
        match row_result {
            Ok((url, content, code)) => {
                urls.push(url);
                contents.push(content);
                codes.push(code);

                if urls.len() >= BATCH_SIZE {
                    send_batch(&tx, &schema, &mut urls, &mut contents, &mut codes).await;
                }
            }
            Err(e) => eprintln!("Read error: {:?}", e),
        }
    }

    if !urls.is_empty() {
        send_batch(&tx, &schema, &mut urls, &mut contents, &mut codes).await;
    }
}

async fn send_batch(
    tx: &mpsc::Sender<RecordBatch>,
    schema: &Arc<Schema>,
    urls: &mut Vec<String>,
    contents: &mut Vec<Option<String>>,
    codes: &mut Vec<Option<i32>>,
) {
    // Zero-copy move
    let url_array = StringArray::from(std::mem::take(urls));
    let content_array = StringArray::from(std::mem::take(contents));
    let code_array = Int32Array::from(std::mem::take(codes));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(url_array),
            Arc::new(content_array),
            Arc::new(code_array),
        ],
    )
    .unwrap();

    // Blocking Send: Wait here if writer is busy
    if let Err(_) = tx.send(batch).await {
        // Channel closed
    }
}

fn generate_token_ranges(splits: usize) -> Vec<(i64, i64)> {
    let mut ranges = Vec::new();
    let min = i64::MIN;
    let max = i64::MAX;

    let total_space = (max as u128).wrapping_sub(min as u128);
    let step = total_space / splits as u128;

    let mut current = min as u128;

    for i in 0..splits {
        let next = if i == splits - 1 {
            max as u128
        } else {
            current + step
        };
        ranges.push((current as i64, next as i64));
        current = next;
    }
    ranges
}
