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
            status TEXT NOT NULL,
            alias TEXT,
            ip TEXT,
            version TEXT
        );

        CREATE TABLE IF NOT EXISTS scripts (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            steps TEXT NOT NULL, -- JSON
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS execution_history (
            id TEXT PRIMARY KEY,
            script_id TEXT NOT NULL,
            client_id TEXT NOT NULL,
            status TEXT NOT NULL, -- 'running', 'completed', 'failed'
            started_at DATETIME DEFAULT CURRENT_TIMESTAMP,
            completed_at DATETIME,
            logs TEXT -- JSON array of log entries
        );
        CREATE TABLE IF NOT EXISTS client_groups (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS client_group_members (
            group_id TEXT NOT NULL,
            client_id TEXT NOT NULL,
            PRIMARY KEY (group_id, client_id),
            FOREIGN KEY(group_id) REFERENCES client_groups(id) ON DELETE CASCADE,
            FOREIGN KEY(client_id) REFERENCES clients(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS group_scripts (
            group_id TEXT NOT NULL,
            script_id TEXT NOT NULL,
            PRIMARY KEY (group_id, script_id),
            FOREIGN KEY(group_id) REFERENCES client_groups(id) ON DELETE CASCADE,
            FOREIGN KEY(script_id) REFERENCES scripts(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS client_updates (
            id TEXT PRIMARY KEY,
            version TEXT NOT NULL,
            filename TEXT NOT NULL,
            platform TEXT NOT NULL,
            uploaded_at DATETIME DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS web_users (
            id TEXT PRIMARY KEY,
            username TEXT UNIQUE NOT NULL,
            password_hash TEXT NOT NULL
        );
        "#,
    )
    .execute(&pool)
    .await?;

    // Migration: Add columns if they don't exist (ignore errors if they do)
    let _ = sqlx::query("ALTER TABLE clients ADD COLUMN alias TEXT").execute(&pool).await;
    let _ = sqlx::query("ALTER TABLE clients ADD COLUMN ip TEXT").execute(&pool).await;
    let _ = sqlx::query("ALTER TABLE clients ADD COLUMN version TEXT").execute(&pool).await;

    // Seed admin user if not exists
    // Use runtime query to avoid compile-time check failure on fresh db
    let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM web_users")
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
    
    if user_count == 0 {
        tracing::info!("Seeding default admin user...");
        let id = uuid::Uuid::new_v4().to_string();
        // SHA256("admin")
        let hash = "8c6976e5b5410415bde908bd4dee15dfb167a9c873fc4bb8a81f6f2ab448a918";
        let _ = sqlx::query("INSERT INTO web_users (id, username, password_hash) VALUES (?, ?, ?)")
            .bind(id)
            .bind("admin")
            .bind(hash)
            .execute(&pool).await;
    }

    // Seed example scripts if table is empty
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scripts")
        .fetch_one(&pool)
        .await
        .unwrap_or(0);

    if count == 0 {
        tracing::info!("Seeding example scripts...");
        
        // 1. Example: System Health Check
        let steps1 = serde_json::json!([
            {
                "type": "Shell",
                "payload": { "cmd": "uname", "args": ["-a"] }
            },
            {
                "type": "Shell",
                "payload": { "cmd": "df", "args": ["-h"] }
            },
            {
                "type": "Shell",
                "payload": { "cmd": "free", "args": ["-m"] }
            }
        ]).to_string();
        
        let id1 = uuid::Uuid::new_v4().to_string();
        let _ = sqlx::query!(
            "INSERT INTO scripts (id, name, steps) VALUES (?, ?, ?)",
            id1,
            "Example: System Health Check",
            steps1
        ).execute(&pool).await;

        // 2. Example: Fetch System Logs
        let steps2 = serde_json::json!([
            {
                "type": "Download",
                "payload": { "remote_path": "/var/log/syslog" }
            }
        ]).to_string();

        let id2 = uuid::Uuid::new_v4().to_string();
        let _ = sqlx::query!(
            "INSERT INTO scripts (id, name, steps) VALUES (?, ?, ?)",
            id2,
            "Example: Fetch System Logs (Linux)",
            steps2
        ).execute(&pool).await;

        // 3. Example: Deploy Config File
        // Note: This assumes 'example.conf' exists in server staging area. 
        // We'll just add the step as an example.
        let steps3 = serde_json::json!([
            {
                "type": "Upload",
                "payload": { 
                    "local_path": "example.conf", 
                    "remote_path": "/tmp/example.conf" 
                }
            }
        ]).to_string();

        let id3 = uuid::Uuid::new_v4().to_string();
        let _ = sqlx::query!(
            "INSERT INTO scripts (id, name, steps) VALUES (?, ?, ?)",
            id3,
            "Example: Deploy Config",
            steps3
        ).execute(&pool).await;
    }

    Ok(pool)
}
