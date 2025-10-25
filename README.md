# Logos: The Search Core

An intelligent search engine core that forges order from chaos, transforming raw data into accessible knowledge with unparalleled performance.

---

## 1. Overview

Logos is a state-of-the-art search engine core designed to be the intelligent heart of a large-scale search system. It serves as the downstream system for the **Arachne web crawler**, taking the raw `crawl-results` and transforming them into a fully indexed, ranked, and queryable knowledge base.

Its name, **Logos**, reflects its core mission: to apply reason, logic, and structure to the vast, unstructured data of the web, making it intelligible and useful.

## 2. Core Philosophy & Design

Logos is engineered with a philosophy of precision, craftsmanship, and speed at every layer.

*   **Performance as a Feature:** From the Rust-based indexing pipeline to the scatter-gather query engine, every architectural decision is optimized for low latency and high throughput. We leverage best-in-class technologies to ensure that queries are served at the speed of thought.
*   **Asynchronous & Decoupled Architecture:** The system is designed as a distributed data pipeline. Indexing, ranking, and querying are independent processes that operate asynchronously, ensuring the system is resilient, scalable, and always available to serve requests.
*   **Intelligent & Layered Ranking:** Logos combines the offline, global authority calculations (like PageRank) with dynamic, query-time relevance scoring (BM25) to deliver results that are not just relevant, but also authoritative and trustworthy.

## 3. System Architecture

Logos is comprised of three distinct but interconnected subsystems, each with a name reflecting its philosophical purpose.

![[Untitled diagram-2025-10-16-141204.png]]

1.  **The Artificer (Indexing Pipeline):** A skilled craftsman that forges the raw data into a structured index.
    *   **Ingestion:** Consumes the `crawl-results` stream from Kafka.
    *   **Processing:** A fleet of **Indexer Workers** (written in Rust) parse content, extract text, tokenize, and build segments of an inverted index using the high-performance [**Tantivy**](https://github.com/quickwit-oss/tantivy) library.
    *   **Persistence:** Index segments are periodically merged and published to a central, durable **Distributed Search Index** (on an Object Store like S3 or MinIO). Document metadata is simultaneously written to The Chronicle.

2.  **The Dialectic (Ranking System):** An offline system that weighs arguments to determine truth and authority.
    *   **Process:** A distributed batch job reads the web's link graph from the metadata store.
    *   **Computation:** It iteratively calculates a static authority score (e.g., PageRank) for every document. For a pure-Rust, high-performance stack, this can be implemented with a framework like [**Timely/Differential Dataflow**](https://github.com/TimelyDataflow/timely-dataflow). Apache Spark is a classic alternative.
    *   **Output:** The calculated scores are written back into each document's entry in The Chronicle, ready for use at query time.

3.  **The Illuminator (Query Engine):** The user-facing system that shines a light on answers.
    *   **Architecture:** Implements a **Scatter-Gather** pattern for massively parallel query execution.
    *   **Query Broker:** A stateless service (built with a high-performance Rust web framework like [**Axum**](https://github.com/tokio-rs/axum) or [**Actix-web**](https://actix.rs/)) receives user queries.
    *   **Searcher Nodes:** A fleet of nodes, each holding a shard of the index in memory. The Broker broadcasts the query to all Searchers.
    *   **Aggregation:** Each Searcher returns its top results. The Broker merges these lists, fetches snippets and metadata from **The Chronicle (ScyllaDB)**, computes a final weighted score (BM25 + PageRank), and returns the final, illuminated results to the user. Communication between Broker and Searchers is optimized using [**gRPC**](https://grpc.io/) for minimal serialization overhead.

## 4. Technology Stack

*   **Core Language:** [**Rust**](https://www.rust-lang.org/)
*   **Core Search Library:** [**Tantivy**](https://github.com/quickwit-oss/tantivy) (Rust's Lucene)
*   **Data Stores:**
    *   **Metadata (The Chronicle):** [**ScyllaDB**](https://www.scylladb.com/)
    *   **Distributed Index:** [**AWS S3 / MinIO**](https://min.io/)
*   **Messaging/Streaming:** [**Apache Kafka**](https://kafka.apache.org/)
*   **Query Engine:**
    *   **API Framework:** [**Axum**](https://github.com/tokio-rs/axum) or [**Actix-web**](https://actix.rs/)
    *   **Internal RPC:** [**gRPC / Tonic**](https://github.com/hyperium/tonic)
*   **Offline Ranking:** [**Timely Dataflow**](https://github.com/TimelyDataflow/timely-dataflow) (Rust-native) or [**Apache Spark**](https://spark.apache.org/)
*   **Deployment:** [**Docker**](https://www.docker.com/) & [**Docker Compose**](https://docs.docker.com/compose/)

## 5. Getting Started

As Logos reuses the core infrastructure of Arachne, setup is streamlined.

1.  **Ensure Kafka & ScyllaDB are running** from the Arachne `docker-compose.yml`.

2.  **Clone the repository:**
    ```bash
    git clone https://github.com/Noel-Alex/Logos.git
    cd logos
    ```

3.  **Initialize the Logos components:**
    (A `docker-compose.logos.yml` file or similar would be used here to orchestrate the Indexer, Merger, Broker, and Searcher nodes).
    ```bash
    docker-compose up
    ```

