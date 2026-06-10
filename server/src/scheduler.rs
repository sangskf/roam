use std::sync::Arc;
use tokio::time;
use tracing::{info, error, warn};
use uuid::Uuid;
use sqlx::Row;
use chrono::{Timelike, Datelike};

use crate::state::{AppState, ScriptGroup};

const CHECK_INTERVAL_SEC: u64 = 60;

pub fn start(state: Arc<AppState>) {
    let state_clone = state.clone();
    tokio::spawn(async move {
        time::sleep(time::Duration::from_secs(10)).await;
        loop {
            if let Err(e) = check_and_run(&state_clone).await {
                error!("Scheduler check failed: {}", e);
            }
            time::sleep(time::Duration::from_secs(CHECK_INTERVAL_SEC)).await;
        }
    });
    info!("Scheduler started (check interval: {}s)", CHECK_INTERVAL_SEC);
}

async fn check_and_run(state: &Arc<AppState>) -> anyhow::Result<()> {
    let now = chrono::Utc::now();

    let rows = sqlx::query(
        "SELECT id, name, cron_expression, task_type, group_id, script_ids, client_ids, steps \
         FROM scheduled_tasks \
         WHERE enabled = 1 AND (next_run_at IS NULL OR next_run_at <= ?)"
    )
    .bind(now)
    .fetch_all(&state.db)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    let server_host = format!("{}:{}", state.config.host, state.config.port);

    for row in rows {
        let task_id: String = row.get("id");
        let name: String = row.get("name");
        let cron_expr: String = row.get("cron_expression");
        let task_type: String = row.get("task_type");
        let group_id: Option<String> = row.get("group_id");
        let script_ids: String = row.get("script_ids");
        let client_ids_json: String = row.get("client_ids");
        let steps_json: String = row.get("steps");

        info!("Scheduler triggering task: {} ({})", name, task_id);

        let next_run = match CronExpr::parse(&cron_expr) {
            Ok(expr) => expr.next_occurrence(chrono::Utc::now()),
            Err(e) => {
                warn!("Invalid cron expression '{}' for task {}: {}", cron_expr, task_id, e);
                let _ = sqlx::query("UPDATE scheduled_tasks SET enabled = 0 WHERE id = ?")
                    .bind(&task_id)
                    .execute(&state.db).await;
                continue;
            }
        };

        let status = match task_type.as_str() {
            "group" => execute_group_task(state, &task_id, &name, &group_id, &script_ids, &server_host, &task_id).await,
            "custom" => execute_custom_task(state, &task_id, &name, &client_ids_json, &steps_json, &server_host, &task_id).await,
            _ => {
                warn!("Unknown task_type '{}' for scheduled task {}", task_type, task_id);
                "failed".to_string()
            }
        };

        let _ = sqlx::query(
            "UPDATE scheduled_tasks SET last_run_at = ?, next_run_at = ?, last_status = ? WHERE id = ?"
        )
        .bind(chrono::Utc::now())
        .bind(next_run)
        .bind(&status)
        .bind(&task_id)
        .execute(&state.db).await;

        info!("Scheduled task {} finished with status: {}", name, status);
    }

    Ok(())
}

async fn execute_group_task(
    state: &Arc<AppState>,
    task_id: &str,
    _name: &str,
    group_id: &Option<String>,
    script_ids_json: &str,
    server_host: &str,
    scheduled_task_id: &str,
) -> String {
    let gid = match group_id {
        Some(g) => g.clone(),
        None => {
            warn!("Scheduled task {} has no group_id", task_id);
            return "failed".to_string();
        }
    };

    let members = sqlx::query("SELECT client_id FROM client_group_members WHERE group_id = ?")
        .bind(&gid)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

    if members.is_empty() {
        warn!("Scheduled task {}: group has no members", task_id);
        return "failed".to_string();
    }

    let script_id_list: Vec<String> = serde_json::from_str(script_ids_json).unwrap_or_default();

    let mut scripts = Vec::new();
    for sid in &script_id_list {
        if let Ok(Some(row)) = sqlx::query("SELECT id, name, steps FROM scripts WHERE id = ?")
            .bind(sid)
            .fetch_optional(&state.db).await
        {
            let id_str: String = row.get("id");
            let name: String = row.get("name");
            let steps_str: String = row.get("steps");
            let steps: Vec<crate::state::ScriptStep> = serde_json::from_str(&steps_str).unwrap_or_default();
            scripts.push(ScriptGroup {
                id: Uuid::parse_str(&id_str).unwrap_or_default(),
                name,
                steps,
            });
        }
    }

    if scripts.is_empty() {
        warn!("Scheduled task {}: no valid scripts found", task_id);
        return "failed".to_string();
    }

    let scripts_arc = Arc::new(scripts);

    for member in &members {
        let cid_str: String = member.get("client_id");
        let client_id = Uuid::parse_str(&cid_str).unwrap_or_default();
        if !state.clients.contains_key(&client_id) {
            continue;
        }

        let state_clone = state.clone();
        let scripts_clone = scripts_arc.clone();
        let host = server_host.to_string();
        let st_id = scheduled_task_id.to_string();

        tokio::spawn(async move {
            for script in scripts_clone.iter() {
                let history_id = Uuid::new_v4();
                let h_id = history_id.to_string();
                let s_id = script.id.to_string();
                let now = chrono::Utc::now();
                let _ = sqlx::query(
                    "INSERT INTO execution_history (id, script_id, client_id, status, started_at, scheduled_task_id) VALUES (?, ?, ?, ?, ?, ?)",
                )
                    .bind(&h_id)
                    .bind(&s_id)
                    .bind(&cid_str)
                    .bind("running")
                    .bind(now)
                    .bind(&st_id)
                    .execute(&state_clone.db).await;

                crate::handlers::run_script_task(
                    state_clone.clone(),
                    client_id,
                    script.clone(),
                    history_id,
                    host.clone(),
                ).await;
            }
        });
    }

    "completed".to_string()
}

async fn execute_custom_task(
    state: &Arc<AppState>,
    task_id: &str,
    _name: &str,
    client_ids_json: &str,
    steps_json: &str,
    server_host: &str,
    scheduled_task_id: &str,
) -> String {
    let client_ids: Vec<String> = serde_json::from_str(client_ids_json).unwrap_or_default();
    let steps: Vec<crate::state::ScriptStep> = serde_json::from_str(steps_json).unwrap_or_default();

    if client_ids.is_empty() || steps.is_empty() {
        warn!("Scheduled task {}: no clients or steps defined", task_id);
        return "failed".to_string();
    }

    let script = ScriptGroup {
        id: Uuid::new_v4(),
        name: format!("scheduled-{}", task_id),
        steps,
    };
    let script_arc = Arc::new(script);

    for cid_str in &client_ids {
        let client_id = Uuid::parse_str(cid_str).unwrap_or_default();
        if !state.clients.contains_key(&client_id) {
            continue;
        }

        let state_clone = state.clone();
        let script_clone = script_arc.clone();
        let host = server_host.to_string();
        let cid = client_id;
        let cid_str_for_history = cid_str.clone();
        let st_id = scheduled_task_id.to_string();

        tokio::spawn(async move {
            let history_id = Uuid::new_v4();
            let h_id = history_id.to_string();
            let s_id = script_clone.id.to_string();
            let now = chrono::Utc::now();
            let _ = sqlx::query(
                "INSERT INTO execution_history (id, script_id, client_id, status, started_at, scheduled_task_id) VALUES (?, ?, ?, ?, ?, ?)",
            )
                .bind(&h_id)
                .bind(&s_id)
                .bind(&cid_str_for_history)
                .bind("running")
                .bind(now)
                .bind(&st_id)
                .execute(&state_clone.db).await;

            crate::handlers::run_script_task(
                state_clone.clone(),
                cid,
                (*script_clone).clone(),
                history_id,
                host.clone(),
            ).await;
        });
    }

    "completed".to_string()
}

// ---------------------------------------------------------------------------
// Minimal cron parser (5-field: minute hour day-of-month month day-of-week)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CronExpr {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days: Vec<u32>,
    months: Vec<u32>,
    weekdays: Vec<u32>,
}

impl CronExpr {
    pub fn parse(expr: &str) -> Result<Self, String> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!("Expected 5 fields, got {}", parts.len()));
        }

        Ok(CronExpr {
            minutes: parse_field(parts[0], 0, 59)?,
            hours: parse_field(parts[1], 0, 23)?,
            days: parse_field(parts[2], 1, 31)?,
            months: parse_field(parts[3], 1, 12)?,
            weekdays: parse_field(parts[4], 0, 6)?,
        })
    }

    fn matches(&self, dt: &chrono::DateTime<chrono::Utc>) -> bool {
        self.minutes.contains(&(dt.minute() as u32))
            && self.hours.contains(&(dt.hour() as u32))
            && self.days.contains(&(dt.day() as u32))
            && self.months.contains(&(dt.month() as u32))
            && self.weekdays.contains(&(dt.weekday().num_days_from_sunday()))
    }

    pub fn next_occurrence(&self, from: chrono::DateTime<chrono::Utc>) -> Option<chrono::DateTime<chrono::Utc>> {
        // Start from next minute
        let mut candidate = from + chrono::Duration::minutes(1);
        // Search up to 2 years ahead to avoid infinite loop
        let deadline = from + chrono::Duration::days(730);

        while candidate <= deadline {
            if self.matches(&candidate) {
                return Some(candidate);
            }
            candidate = candidate + chrono::Duration::minutes(1);
        }
        None
    }
}

fn parse_field(field: &str, min: u32, max: u32) -> Result<Vec<u32>, String> {
    let mut values = Vec::new();

    for segment in field.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            return Err("Empty field segment".to_string());
        }

        // Step syntax: range/step or */step
        let (range_part, step) = if let Some(pos) = segment.find('/') {
            let step: u32 = segment[pos + 1..].parse().map_err(|_| format!("Invalid step: {}", &segment[pos+1..]))?;
            if step == 0 {
                return Err("Step cannot be 0".to_string());
            }
            (&segment[..pos], Some(step))
        } else {
            (segment, None)
        };

        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some(pos) = range_part.find('-') {
            let s: u32 = range_part[..pos].parse().map_err(|_| format!("Invalid range start: {}", &range_part[..pos]))?;
            let e: u32 = range_part[pos + 1..].parse().map_err(|_| format!("Invalid range end: {}", &range_part[pos+1..]))?;
            (s, e)
        } else {
            let v: u32 = range_part.parse().map_err(|_| format!("Invalid number: {}", range_part))?;
            values.push(v);
            continue;
        };

        if start > end || start < min || end > max {
            return Err(format!("Range {}..{} out of bounds for field {}-{}", start, end, min, max));
        }

        match step {
            Some(s) => {
                let mut v = start;
                while v <= end {
                    values.push(v);
                    v = v.checked_add(s).ok_or("Step overflow")?;
                }
            }
            None => {
                for v in start..=end {
                    values.push(v);
                }
            }
        }
    }

    values.sort();
    values.dedup();
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cron_every_minute() {
        let expr = CronExpr::parse("* * * * *").unwrap();
        let dt = chrono::Utc::now();
        assert!(expr.matches(&dt));
    }

    #[test]
    fn test_cron_exact_minute() {
        let expr = CronExpr::parse("30 * * * *").unwrap();
        let dt = chrono::DateTime::from_timestamp(0, 0).unwrap(); // 1970-01-01 00:00:00
        assert!(!expr.matches(&dt));
        let dt2 = chrono::DateTime::from_timestamp(30 * 60, 0).unwrap(); // 00:30
        assert!(expr.matches(&dt2));
    }

    #[test]
    fn test_cron_next_occurrence() {
        let expr = CronExpr::parse("0 */2 * * *").unwrap(); // every 2 hours
        let from = chrono::DateTime::from_timestamp(0, 0).unwrap(); // 00:00
        let next = expr.next_occurrence(from).unwrap();
        assert_eq!(next.hour(), 2);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn test_cron_complex() {
        let expr = CronExpr::parse("*/15 9-17 * * 1-5").unwrap(); // every 15 min, 9-5, weekdays
        assert_eq!(expr.minutes.len(), 4); // 0, 15, 30, 45
        assert_eq!(expr.hours.len(), 9); // 9, 10, ..., 17
        assert_eq!(expr.weekdays.len(), 5); // 1, 2, 3, 4, 5
    }

    #[test]
    fn test_cron_list() {
        let expr = CronExpr::parse("0,30 * * * *").unwrap();
        assert!(expr.minutes.contains(&0));
        assert!(expr.minutes.contains(&30));
        assert_eq!(expr.minutes.len(), 2);
    }
}
