// src/bin/db_extractor
use anyhow::{anyhow, Result};
use arrow_array::{
    ArrayRef, BooleanArray, Float32Array, Float64Array, Int32Array, Int64Array, RecordBatch,
    StringArray,
};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use futures::stream::{self, StreamExt};
use parquet::arrow::AsyncArrowWriter;
use parquet::basic::{Compression};
use parquet::file::properties::WriterProperties;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::value::CqlValue;
use std::fs::File;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Instant;
use scylla::value::Row;


const SCYLLA_URI: &str = "127.0.0.1:9042";
const KEYSPACE: &str = "arachne";
const TABLE: &str = "labeled_dataset";
const PARQUET_OUTPUT: &str = "labeled_dataset.parquet";

// Tuning
const PARALLELISM: usize = 256; // Number of parallel token ranges
const BATCH_SIZE: usize = 2000; // Rows per Parquet RowGroup


#[derive(Clone, Debug)]
struct ColumnMeta {
    name: String,
    cql_type: String,
    arrow_type: DataType,
}

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

    // 1. DYNAMICALLY DISCOVER SCHEMA
    println!("Init: fetching schema for {}.{}...", KEYSPACE, TABLE);
    let columns = fetch_table_schema(&session, KEYSPACE, TABLE).await?;

    if columns.is_empty() {
        return Err(anyhow!("Table {}.{} not found or has no columns", KEYSPACE, TABLE));
    }

    // Build Arrow Schema from discovered columns
    let arrow_fields: Vec<Field> = columns
        .iter()
        .map(|c| Field::new(&c.name, c.arrow_type.clone(), true))
        .collect();
    let schema = Arc::new(Schema::new(arrow_fields));

    println!("Init: Schema discovered with {} columns.", columns.len());
    for col in &columns {
        println!("  - {} ({}) -> {:?}", col.name, col.cql_type, col.arrow_type);
    }

    // 2. SETUP PIPELINE
    let (tx, mut rx) = mpsc::channel::<RecordBatch>(PARALLELISM * 2);
    let schema_clone = schema.clone();

    // 3. WRITER TASK
    let writer_handle = tokio::spawn(async move {
        println!("Writer: Creating file: {}", PARQUET_OUTPUT);
        let file = File::create(PARQUET_OUTPUT).expect("Failed to create file");

        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .set_max_row_group_size(BATCH_SIZE)
            .build();

        let mut writer =
            AsyncArrowWriter::try_new(tokio::fs::File::from(file), schema_clone, Some(props))
                .expect("Failed to create arrow writer");

        let mut total_rows = 0;

        while let Some(batch) = rx.recv().await {
            let batch_rows = batch.num_rows();
            if batch_rows == 0 { continue; }

            writer.write(&batch).await.expect("Failed to write batch");
            total_rows += batch_rows;

            if total_rows % (BATCH_SIZE * 10) == 0 {
                println!("Writer: >> Written {} rows...", total_rows);
            }
        }

        writer.close().await.expect("Failed to close writer");
        println!("Writer: FINISHED. Total rows written: {}", total_rows);
    });

    // 4. READER TASKS
    let ranges = generate_token_ranges(PARALLELISM);
    println!("Starting scan with {} parallel streams...", PARALLELISM);

    // We pass the column definitions to the workers so they know how to map data
    let columns = Arc::new(columns);

    let tasks = stream::iter(ranges).map(|(start, end)| {
        let session = session.clone();
        let tx = tx.clone();
        let schema = schema.clone();
        let columns = columns.clone();

        tokio::spawn(async move {
            if let Err(e) = process_token_range(session, start, end, tx, schema, columns).await {
                eprintln!("Worker Error: {}", e);
            }
        })
    });

    tasks.buffer_unordered(PARALLELISM).collect::<Vec<_>>().await;

    println!("All readers finished. Closing channel...");
    drop(tx);

    writer_handle.await?;
    println!("Job Complete. Time taken: {:.2?}", start_time.elapsed());
    Ok(())
}

// --- DYNAMIC SCHEMA DISCOVERY ---

async fn fetch_table_schema(session: &Session, keyspace: &str, table: &str) -> Result<Vec<ColumnMeta>> {
    // Query Scylla system tables to get column info
    let query = "SELECT column_name, kind, type FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?";

    // We expect rows of (name, kind, type) strings
    let mut rows_stream = session
        .query_iter(query, (keyspace, table))
        .await?
        .rows_stream::<(String, String, String)>()?;

    let mut cols = Vec::new();

    while let Some(row_res) = rows_stream.next().await {
        let (name, _kind, cql_type) = row_res?;

        let arrow_type = match cql_type.as_str() {
            "int" => DataType::Int32,
            "bigint" | "counter" | "time" => DataType::Int64,
            "timestamp" => DataType::Timestamp(TimeUnit::Millisecond, None),
            "float" => DataType::Float32,
            "double" => DataType::Float64,
            "boolean" => DataType::Boolean,
            // Fallback: Convert everything else (Text, UUID, Blob, Inet, Map, List) to String
            _ => DataType::Utf8,
        };

        cols.push(ColumnMeta {
            name,
            cql_type,
            arrow_type,
        });
    }

    // Scylla doesn't guarantee order from system_schema, but we want deterministic order
    cols.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(cols)
}

// --- DYNAMIC DATA PROCESSING ---

async fn process_token_range(
    session: Arc<Session>,
    start_token: i64,
    end_token: i64,
    tx: mpsc::Sender<RecordBatch>,
    schema: Arc<Schema>,
    columns: Arc<Vec<ColumnMeta>>,
) -> Result<()> {
    // Construct SELECT query dynamically based on discovered columns
    let col_names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    let select_clause = col_names.join(", ");

    let query = format!(
        "SELECT {} FROM {}.{} WHERE token(domain) >= ? AND token(domain) < ?",
        select_clause, KEYSPACE, TABLE
    );

    let mut prepared = session.prepare(query.as_str()).await?;
    prepared.set_page_size(BATCH_SIZE as i32);

    // We rely on Row (CqlValue) parsing now, not tuple parsing
    let mut rows_stream = session
        .execute_iter(prepared, (start_token, end_token))
        .await?
        .rows_stream::<scylla::value::Row>()?;

    // Buffers for each column. We need a vector of "Builders" essentially.
    let mut batch_buffer: Vec<Vec<Option<CqlValue>>> = vec![Vec::with_capacity(BATCH_SIZE); columns.len()];

    while let Some(row_res) = rows_stream.next().await {
        let row = row_res?;

        // Iterate columns in the row
        for (i, cql_val) in row.columns.into_iter().enumerate() {
            batch_buffer[i].push(cql_val);
        }

        if batch_buffer[0].len() >= BATCH_SIZE {
            send_dynamic_batch(&tx, &schema, &columns, &mut batch_buffer).await?;
        }
    }

    if !batch_buffer[0].is_empty() {
        send_dynamic_batch(&tx, &schema, &columns, &mut batch_buffer).await?;
    }

    Ok(())
}

async fn send_dynamic_batch(
    tx: &mpsc::Sender<RecordBatch>,
    schema: &Arc<Schema>,
    columns: &[ColumnMeta],
    buffer: &mut Vec<Vec<Option<CqlValue>>>,
) -> Result<()> {
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());

    for (i, col_meta) in columns.iter().enumerate() {
        let raw_vals = std::mem::take(&mut buffer[i]);

        let array: ArrayRef = match col_meta.arrow_type {
            DataType::Int32 => {
                let iter = raw_vals.into_iter().map(|v| v.and_then(|c| c.as_int()));
                Arc::new(Int32Array::from_iter(iter))
            },
            DataType::Int64 | DataType::Timestamp(_, _) => {
                let iter = raw_vals.into_iter().map(|v| {
                    match v {
                        Some(CqlValue::BigInt(n)) => Some(n),
                        Some(CqlValue::Counter(c)) => Some(c.0),
                        Some(CqlValue::Timestamp(t)) => Some(t.0),
                        Some(CqlValue::Time(t)) => Some(t.0),
                        _ => None,
                    }
                });
                Arc::new(Int64Array::from_iter(iter))
            },
            DataType::Float32 => {
                let iter = raw_vals.into_iter().map(|v| v.and_then(|c| c.as_float()));
                Arc::new(Float32Array::from_iter(iter))
            },
            DataType::Float64 => {
                let iter = raw_vals.into_iter().map(|v| v.and_then(|c| c.as_double()));
                Arc::new(Float64Array::from_iter(iter))
            },
            DataType::Boolean => {
                let iter = raw_vals.into_iter().map(|v| v.and_then(|c| c.as_boolean()));
                Arc::new(BooleanArray::from_iter(iter))
            },
            _ => {
                let iter = raw_vals.into_iter().map(|v| {
                    v.map(|c| format!("{}", c))
                });
                Arc::new(StringArray::from_iter(iter))
            }
        };
        arrays.push(array);
    }

    let batch = RecordBatch::try_new(schema.clone(), arrays)?;

    if let Err(_) = tx.send(batch).await {
    }

    Ok(())
}

fn generate_token_ranges(splits: usize) -> Vec<(i64, i64)> {
    let mut ranges = Vec::new();
    let min = i64::MIN;
    let max = i64::MAX;
    let total = (max as u128).wrapping_sub(min as u128);
    let step = total / splits as u128;
    let mut current = min as u128;

    for i in 0..splits {
        let next = if i == splits - 1 { max as u128 } else { current + step };
        ranges.push((current as i64, next as i64));
        current = next;
    }
    ranges
}