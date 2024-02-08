#![warn(
    missing_debug_implementations,
    missing_docs,
    rust_2018_idioms,
    unreachable_pub
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//! # apalis-sql
//! apalis offers Sqlite, Mysql and Postgres storages for its workers.
//!
//! ## Postgres Example
//!  ```rust,no_run
//! use apalis::prelude::*;
//! # use apalis_sql::postgres::PostgresStorage;
//! # use apalis_sql::postgres::PgPool;

//!  use email_service::Email;
//!
//!  #[tokio::main]
//!  async fn main() -> std::io::Result<()> {
//!      std::env::set_var("RUST_LOG", "debug,sqlx::query=error");
//!      let database_url = std::env::var("DATABASE_URL").expect("Must specify url to db");
//!      let pool = PgPool::connect(&database_url).await.unwrap();
//!      
//!      PostgresStorage::setup(&pool).await.unwrap();
//!      let pg: PostgresStorage<Email> = PostgresStorage::new(pool);
//!
//!      async fn send_email(job: Email, data: Data<usize>) -> Result<(), Error> {
//!          /// execute job
//!          Ok(())
//!      }
//!     // This can be even in another program/language
//!     // let query = "Select apalis.push_job('apalis::Email', json_build_object('subject', 'Test apalis', 'to', 'test1@example.com', 'text', 'Lorem Ipsum'));";
//!     // pg.execute(query).await.unwrap();
//!
//!      Monitor::<TokioExecutor>::new()
//!          .register_with_count(4, {
//!              WorkerBuilder::new(&format!("tasty-avocado"))
//!                  .data(0usize)
//!                  .source(pg)
//!                  .build_fn(send_email)
//!          })
//!          .run()
//!          .await
//!  }
//! ```

use std::time::Duration;

/// The context of the sql job
pub mod context;
/// Util for fetching rows
pub mod from_row;

/// Postgres storage for apalis. Uses `NOTIFY` and `SKIP LOCKED`
#[cfg(feature = "postgres")]
#[cfg_attr(docsrs, doc(cfg(feature = "postgres")))]
pub mod postgres;

/// Sqlite Storage for apalis.
/// Uses a transaction and min(rowid)
#[cfg(feature = "sqlite")]
#[cfg_attr(docsrs, doc(cfg(feature = "sqlite")))]
pub mod sqlite;

/// MySQL storage for apalis. Uses `SKIP LOCKED`
#[cfg(feature = "mysql")]
#[cfg_attr(docsrs, doc(cfg(feature = "mysql")))]
pub mod mysql;

/// Config for sql storages
#[derive(Debug, Clone)]
pub struct Config {
    keep_alive: Duration,
    buffer_size: usize,
    poll_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keep_alive: Duration::from_secs(30),
            buffer_size: 10,
            poll_interval: Duration::from_millis(50),
        }
    }
}
