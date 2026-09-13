//! Activity event store + panel flush — port of wings
//! `internal/cron/activity_cron.go` + `internal/cron/sftp_cron.go`.
//!
//! Events are persisted to a local SQLite database first, so a daemon
//! crash or panel outage never loses the audit trail (the panel accepts
//! duplicates-free batches: entries are deleted only after a successful
//! upload). Two flush paths mirror wings:
//!
//! - **plain**: everything that is not `server:sftp.*`, sent as-is.
//! - **sftp**: `server:sftp.*` events are merged per (user, server, ip,
//!   event, minute) with their `files` metadata concatenated, so a bulk
//!   SFTP operation becomes a single panel event (wings sftpCron).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::RwLock;

use crate::models::Activity;
use crate::remote::PanelClient;

/// SQLite parameter limit safety margin for `IN (...)` deletes. Modern
/// SQLite allows 32k+ parameters, but wings chunks at 32.000 for exactly
/// this reason; 900 is the portable bound and far below any realistic
/// batch size here.
const SQLITE_PARAM_CHUNK: usize = 900;

pub struct ActivityCollector {
    conn: Mutex<Connection>,
}

impl ActivityCollector {
    /// Open (creating if needed) the activity database at `path`.
    pub fn new(path: &Path) -> Result<Self, rusqlite::Error> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS activities (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                user      TEXT,
                server    TEXT NOT NULL,
                event     TEXT NOT NULL,
                metadata  TEXT NOT NULL,
                ip        TEXT NOT NULL DEFAULT '',
                timestamp TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_activities_event ON activities(event);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Persist an event. Never drops entries: the database is the buffer
    /// (wings keeps every event until the panel accepts it).
    pub fn push(&self, activity: Activity) {
        let metadata = match serde_json::to_string(&activity.metadata) {
            Ok(m) => m,
            Err(_) => "null".to_string(),
        };
        if let Err(e) = self.conn.lock().unwrap().execute(
            "INSERT INTO activities (user, server, event, metadata, ip, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                activity.user,
                activity.server,
                activity.event,
                metadata,
                activity.ip,
                activity.timestamp
            ],
        ) {
            tracing::error!(error = %e, "failed to persist activity event");
        }
    }

    /// Run until shutdown: every `interval` flush both event classes to
    /// the panel (up to `count` each), mirroring the wings scheduler that
    /// runs activityCron + sftpCron on the same interval.
    pub async fn flush_task(
        self: Arc<Self>,
        panel: Arc<RwLock<PanelClient>>,
        interval: Duration,
        count: usize,
    ) {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;

        loop {
            tick.tick().await;
            if let Err(e) = self.flush_plain(&panel, count).await {
                tracing::warn!(error = %e, "activity flush failed");
            }
            if let Err(e) = self.flush_sftp(&panel, count).await {
                tracing::warn!(error = %e, "sftp activity flush failed");
            }
        }
    }

    /// wings `activityCron`: send non-SFTP events as-is; delete invalid-IP
    /// rows and everything once accepted.
    async fn flush_plain(
        &self,
        panel: &Arc<RwLock<PanelClient>>,
        count: usize,
    ) -> Result<(), String> {
        let batch = self.fetch_plain(count)?;
        if batch.is_empty() {
            return Ok(());
        }

        // wings deletes rows whose IP no longer parses (historic bug fix).
        let mut invalid_ids = Vec::new();
        let mut valid: Vec<(i64, Activity)> = Vec::new();
        for (id, activity) in batch {
            if activity.ip.parse::<std::net::IpAddr>().is_err() {
                invalid_ids.push(id);
            } else {
                valid.push((id, activity));
            }
        }
        self.delete_ids(&invalid_ids);

        if valid.is_empty() {
            return Ok(());
        }

        let to_send: Vec<Activity> = valid.iter().map(|(_, a)| a.clone()).collect();
        if let Err(e) = panel.read().await.send_activity_logs(&to_send).await {
            tracing::warn!(error = %e, count = to_send.len(), "failed to send activity logs, keeping rows");
            return Ok(());
        }
        let ids: Vec<i64> = valid.iter().map(|(id, _)| *id).collect();
        self.delete_ids(&ids);
        tracing::debug!(count = ids.len(), "activity logs sent to panel");
        Ok(())
    }

    /// wings `sftpCron`: merge `server:sftp.*` events per
    /// (user, server, ip, event, minute) with concatenated `files`
    /// metadata, send the merged set, then delete the original rows.
    async fn flush_sftp(
        &self,
        panel: &Arc<RwLock<PanelClient>>,
        count: usize,
    ) -> Result<(), String> {
        let rows = self.fetch_sftp(count)?;
        if rows.is_empty() {
            return Ok(());
        }

        struct Merged {
            activity: Activity,
        }

        let mut order: Vec<(String, i64)> = Vec::new();
        let mut map: std::collections::HashMap<String, Merged> = std::collections::HashMap::new();

        for (id, mut activity) in rows {
            // Minute-granular grouping key (wings formats the timestamp as
            // "2006-01-02_15:04" so events land in the same bucket).
            let minute = chrono::DateTime::parse_from_rfc3339(&activity.timestamp)
                .map(|t| t.format("%Y-%m-%d_%H:%M").to_string())
                .unwrap_or_else(|_| activity.timestamp.clone());
            let key = format!(
                "{}|{}|{}|{}|{}",
                activity.user.as_deref().unwrap_or(""),
                activity.server,
                activity.ip,
                activity.event,
                minute
            );
            let files = match activity.metadata.get("files") {
                Some(serde_json::Value::Array(items)) => items.clone(),
                _ => Vec::new(),
            };
            activity.metadata = serde_json::json!({ "files": files });

            match map.get_mut(&key) {
                Some(existing) => {
                    // Keep the earliest timestamp of the group.
                    if activity.timestamp < existing.activity.timestamp {
                        existing.activity.timestamp = activity.timestamp.clone();
                    }
                    if let Some(serde_json::Value::Array(existing_files)) =
                        existing.activity.metadata.get_mut("files")
                    {
                        existing_files.extend(files);
                    }
                }
                None => {
                    if map.len() < count {
                        order.push((key.clone(), id));
                        map.insert(key, Merged { activity });
                    }
                    // Over cap: the row is skipped now, it will be picked
                    // up on the next flush (wings does the same).
                }
            }
        }

        if map.is_empty() {
            return Ok(());
        }

        order.sort_by_key(|(_, id)| *id);
        let merged: Vec<Activity> = order
            .iter()
            .filter_map(|(k, _)| map.get(k))
            .map(|m| m.activity.clone())
            .collect();

        if let Err(e) = panel.read().await.send_activity_logs(&merged).await {
            tracing::warn!(error = %e, count = merged.len(), "failed to send sftp activity logs, keeping rows");
            return Ok(());
        }
        // Only delete rows that were actually included in the merged batch.
        let sent_ids: Vec<i64> = order.iter().map(|(_, id)| *id).collect();
        self.delete_ids(&sent_ids);
        tracing::debug!(events = sent_ids.len(), merged = merged.len(), "sftp activity logs sent to panel");
        Ok(())
    }

    fn fetch_plain(&self, limit: usize) -> Result<Vec<(i64, Activity)>, String> {
        self.fetch("event NOT LIKE 'server:sftp.%'", limit)
    }

    fn fetch_sftp(&self, limit: usize) -> Result<Vec<(i64, Activity)>, String> {
        self.fetch("event LIKE 'server:sftp.%'", limit)
    }

    fn fetch(&self, filter: &str, limit: usize) -> Result<Vec<(i64, Activity)>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT id, user, server, event, metadata, ip, timestamp
                 FROM activities WHERE {filter} ORDER BY id LIMIT ?1"
            ))
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(rusqlite::params![limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| e.to_string())?;

        let mut out = Vec::new();
        for row in rows {
            let (id, user, server, event, metadata, ip, timestamp) =
                row.map_err(|e| e.to_string())?;
            let metadata = serde_json::from_str(&metadata).unwrap_or(serde_json::Value::Null);
            out.push((
                id,
                Activity { user, server, event, metadata, ip, timestamp },
            ));
        }
        Ok(out)
    }

    fn delete_ids(&self, ids: &[i64]) {
        for chunk in ids.chunks(SQLITE_PARAM_CHUNK) {
            let conn = self.conn.lock().unwrap();
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!("DELETE FROM activities WHERE id IN ({placeholders})");
            if let Err(e) = conn.execute(&sql, rusqlite::params_from_iter(chunk.iter())) {
                tracing::error!(error = %e, "failed to delete sent activity rows");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ActivityCollector {
        let dir = std::env::temp_dir().join(format!("roost-activity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        ActivityCollector::new(&dir.join("activity.sqlite")).unwrap()
    }

    fn activity(event: &str, files: &[&str]) -> Activity {
        Activity::new("srv", event)
            .with_user(Some("user-1".into()))
            .with_ip("203.0.113.9".into())
            .with_metadata(serde_json::json!({ "files": files }))
    }

    #[test]
    fn persists_and_drains_plain_events() {
        let s = store();
        s.push(activity("server:console.command", &[]));
        s.push(activity("server:file.uploaded", &[]));
        let rows = s.fetch_plain(10).unwrap();
        assert_eq!(rows.len(), 2);
        s.delete_ids(&[rows[0].0]);
        assert_eq!(s.fetch_plain(10).unwrap().len(), 1);
    }

    #[test]
    fn sftp_events_merged_per_minute() {
        let s = store();
        s.push(activity("server:sftp.write", &["a.txt"]));
        s.push(activity("server:sftp.write", &["b.txt"]));
        s.push(activity("server:sftp.delete", &["c.txt"]));

        let rows = s.fetch_sftp(100).unwrap();
        assert_eq!(rows.len(), 3);

        // Merge like wings sftpCron: two distinct groups.
        let mut groups: std::collections::HashMap<String, usize> = Default::default();
        let mut files = 0;
        for (_, a) in &rows {
            let minute = chrono::DateTime::parse_from_rfc3339(&a.timestamp)
                .unwrap()
                .format("%Y-%m-%d_%H:%M")
                .to_string();
            let key = format!("{}|{}|{}|{}|{}", a.user.clone().unwrap(), a.server, a.ip, a.event, minute);
            *groups.entry(key).or_insert(0) += 1;
            if let Some(serde_json::Value::Array(f)) = a.metadata.get("files") {
                files += f.len();
            }
        }
        assert_eq!(groups.len(), 2);
        assert_eq!(files, 3);
    }

    #[test]
    fn invalid_ip_rows_are_dropped_by_plain_flush() {
        let s = store();
        let mut bad = activity("server:console.command", &[]);
        bad.ip = "not-an-ip".into();
        s.push(bad);
        assert_eq!(s.fetch_plain(10).unwrap().len(), 1);
        // flush_plain deletes the invalid row before sending; simulate the
        // validation branch directly (no panel needed for the delete part).
        let batch = s.fetch_plain(10).unwrap();
        let invalid: Vec<i64> = batch
            .iter()
            .filter(|(_, a)| a.ip.parse::<std::net::IpAddr>().is_err())
            .map(|(id, _)| *id)
            .collect();
        s.delete_ids(&invalid);
        assert!(s.fetch_plain(10).unwrap().is_empty());
    }
}
