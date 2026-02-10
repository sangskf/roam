use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Pool, Sqlite};
use std::fs::File;
use std::path::Path;

pub async fn init_db(db_url: &str) -> anyhow::Result<Pool<Sqlite>> {
    // Check if db file exists, if not create it (for sqlite)
    // The db_url is usually "sqlite:filename.db"
    if let Some(path) = db_url.strip_prefix("sqlite:") {
        if !Path::new(path).exists() {
            File::create(path)?;
            tracing::info!("Created database file: {}", path);
        }
    }

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(db_url)
        .await?;

    // Create tables if not exist
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS clients (
            id TEXT PRIMARY KEY,
            hostname TEXT NOT NULL,
            os TEXT NOT NULL,
            last_seen DATETIME NOT NULL,
            status TEXT NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await?;

    Ok(pool)
}
