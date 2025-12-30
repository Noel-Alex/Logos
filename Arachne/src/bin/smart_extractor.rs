use anyhow::Result;
use arrow_array::{Int32Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use clap::{Parser, Subcommand};
use dashmap::DashMap;
use futures::stream::{self, StreamExt};
use parquet::arrow::AsyncArrowWriter;
use parquet::basic::{Compression, Encoding};
use parquet::file::properties::WriterProperties;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use std::env;
use std::fs::File;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Instant;

// --- CONFIGURATION ---
const KEYSPACE: &str = "Arachne";
const TABLE_DATA: &str = "crawled_pages";
const TABLE_LABELS: &str = "domain_labels";
const PARQUET_OUTPUT: &str = "training_dataset.parquet";

const PARALLELISM: usize = 64;
const BATCH_SIZE: usize = 2000;

#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Check,
    Export,
}

// Global Map to store the labels in memory: Domain -> IsPhishing
type LabelMap = Arc<DashMap<String, bool>>;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args = Cli::parse();
    let start_time = Instant::now();

    let uri = env::var("SCYLLA_URI").unwrap_or_else(|_| "127.0.0.1:9042".to_string());
    println!("Init: Connecting to ScyllaDB at {}...", uri);

    let session = SessionBuilder::new()
        .known_node(uri)
        .compression(None)
        .build()
        .await?;
    let session = Arc::new(session);

    // STEP 1: LOAD LABELS INTO MEMORY
    // We do this first because we need to join the data.
    println!("Step 1: Loading Domain Labels into Memory...");
    let label_map = load_labels(session.clone()).await?;
    println!("Loaded {} labeled domains.", label_map.len());

    let valid_count = Arc::new(AtomicUsize::new(0));
    let skipped_count = Arc::new(AtomicUsize::new(0));

    match args.command {
        Commands::Check => {
            println!("--- MODE: CHECK ---");
            run_scan(session, label_map, valid_count.clone(), skipped_count.clone(), None).await?;
            println!("--------------------------------");
            println!("MATCHED DATASET SIZE: {}", valid_count.load(Ordering::Relaxed));
            println!("SKIPPED (No Label/Bad HTML): {}", skipped_count.load(Ordering::Relaxed));
        }
        Commands::Export => {
            println!("--- MODE: EXPORT ---");

            // Schema: URL (String), Skeleton (String), Label (Int 0 or 1)
            let schema = Arc::new(Schema::new(vec![
                Field::new("url", DataType::Utf8, false),
                Field::new("html_skeleton", DataType::Utf8, false),
                Field::new("is_phishing", DataType::Int32, false),
            ]));

            let (tx, mut rx) = mpsc::channel::<RecordBatch>(PARALLELISM * 2);
            let writer_schema = schema.clone();

            // Writer Task
            let writer_handle = tokio::spawn(async move {
                let file = File::create(PARQUET_OUTPUT).expect("Failed to create file");
                let props = WriterProperties::builder()
                    .set_compression(Compression::SNAPPY)
                    .set_max_row_group_size(BATCH_SIZE)
                    .build();

                let mut writer = AsyncArrowWriter::try_new(tokio::fs::File::from(file), writer_schema, Some(props))
                    .expect("Failed to create arrow writer");

                let mut total = 0;
                while let Some(batch) = rx.recv().await {
                    total += batch.num_rows();
                    writer.write(&batch).await.expect("Write failed");
                }
                writer.close().await.expect("Close failed");
                total
            });

            run_scan(session, label_map, valid_count.clone(), skipped_count.clone(), Some((tx, schema))).await?;

            let written = writer_handle.await?;
            println!("--------------------------------");
            println!("Export Complete.");
            println!("Total Rows Written: {}", written);
        }
    }

    println!("Total Time: {:.2?}", start_time.elapsed());
    Ok(())
}

// --- HELPER: LOAD LABELS ---
async fn load_labels(session: Arc<Session>) -> Result<LabelMap> {
    let map = Arc::new(DashMap::new());
    let query = format!("SELECT domain, is_phishing FROM {}.{}", KEYSPACE, TABLE_LABELS);

    // We assume domain_labels fits in RAM (even 1M rows is only ~50MB)
    let mut rows_stream = session.query_iter(query, &[])
        .await?
        .rows_stream::<(String, bool)>()?;

    while let Some(row) = rows_stream.next().await {
        if let Ok((domain, is_phishing)) = row {
            map.insert(domain, is_phishing);
        }
    }
    Ok(map)
}

// --- CORE: PARALLEL SCAN ---
async fn run_scan(
    session: Arc<Session>,
    label_map: LabelMap,
    valid_counter: Arc<AtomicUsize>,
    skipped_counter: Arc<AtomicUsize>,
    export_target: Option<(mpsc::Sender<RecordBatch>, Arc<Schema>)>,
) -> Result<()> {
    let ranges = generate_token_ranges(PARALLELISM);
    let export_target = export_target.map(|(tx, sc)| (tx, sc));

    let tasks = stream::iter(ranges).map(|(start, end)| {
        let session = session.clone();
        let map = label_map.clone(); // Cheap Arc clone
        let v_cnt = valid_counter.clone();
        let s_cnt = skipped_counter.clone();
        let chan = export_target.as_ref().map(|(tx, sc)| (tx.clone(), sc.clone()));

        tokio::spawn(async move {
            process_range(session, start, end, map, v_cnt, s_cnt, chan).await
        })
    });

    tasks.buffer_unordered(PARALLELISM).collect::<Vec<_>>().await;
    Ok(())
}

async fn process_range(
    session: Arc<Session>,
    start: i64,
    end: i64,
    label_map: LabelMap,
    v_cnt: Arc<AtomicUsize>,
    s_cnt: Arc<AtomicUsize>,
    chan: Option<(mpsc::Sender<RecordBatch>, Arc<Schema>)>,
) {
    // Select data from Crawled Pages
    // Note: partition key is 'domain', so token(domain) works perfectly.
    let query = format!(
        "SELECT domain, url, tag_sequence, http_status FROM {}.{} WHERE token(domain) >= ? AND token(domain) < ?",
        KEYSPACE, TABLE_DATA
    );

    let mut prepared = match session.prepare(query.as_str()).await {
        Ok(p) => p,
        Err(_) => return,
    };
    prepared.set_page_size(BATCH_SIZE as i32);

    let mut stream = match session.execute_iter(prepared, (start, end)).await {
        Ok(res) => match res.rows_stream::<(String, String, Option<String>, Option<i32>)>() {
            Ok(s) => s,
            Err(_) => return,
        },
        Err(_) => return,
    };

    // Buffers
    let mut b_urls = Vec::with_capacity(BATCH_SIZE);
    let mut b_skeletons = Vec::with_capacity(BATCH_SIZE);
    let mut b_labels = Vec::with_capacity(BATCH_SIZE);

    while let Some(Ok((domain, url, tag_opt, status_opt))) = stream.next().await {

        // 1. Check HTTP Status (Must be 200 OK)
        if status_opt != Some(200) {
            s_cnt.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // 2. Check HTML Skeleton (Must exist, not be empty, not be placeholder)
        let skeleton = match tag_opt {
            Some(s) if !s.is_empty() && s != "~" => s,
            _ => {
                s_cnt.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        // 3. JOIN: Check if we have a label for this domain
        // We use the DashMap look-up here.
        if let Some(entry) = label_map.get(&domain) {
            let is_phishing = *entry.value();

            v_cnt.fetch_add(1, Ordering::Relaxed);

            if let Some((ref tx, ref schema)) = chan {
                b_urls.push(url);
                b_skeletons.push(skeleton);
                // Convert Bool to Int (0 or 1) for the dataset
                b_labels.push(if is_phishing { 1 } else { 0 });

                if b_urls.len() >= BATCH_SIZE {
                    send_batch(tx, schema, &mut b_urls, &mut b_skeletons, &mut b_labels).await;
                }
            }
        } else {
            // We have the HTML, but no label for this domain. Skip.
            s_cnt.fetch_add(1, Ordering::Relaxed);
        }
    }

    // Flush remainder
    if let Some((ref tx, ref schema)) = chan {
        if !b_urls.is_empty() {
            send_batch(tx, schema, &mut b_urls, &mut b_skeletons, &mut b_labels).await;
        }
    }
}

async fn send_batch(
    tx: &mpsc::Sender<RecordBatch>,
    schema: &Arc<Schema>,
    urls: &mut Vec<String>,
    skeletons: &mut Vec<String>,
    labels: &mut Vec<i32>,
) {
    let arr_url = StringArray::from(std::mem::take(urls));
    let arr_skel = StringArray::from(std::mem::take(skeletons));
    let arr_lbl = Int32Array::from(std::mem::take(labels));

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(arr_url), Arc::new(arr_skel), Arc::new(arr_lbl)],
    ).unwrap();

    let _ = tx.send(batch).await;
}

fn generate_token_ranges(splits: usize) -> Vec<(i64, i64)> {
    let mut ranges = Vec::new();
    let min = i64::MIN;
    let max = i64::MAX;
    let step = (max as u128).wrapping_sub(min as u128) / splits as u128;
    let mut current = min as u128;

    for i in 0..splits {
        let next = if i == splits - 1 { max as u128 } else { current + step };
        ranges.push((current as i64, next as i64));
        current = next;
    }
    ranges
}