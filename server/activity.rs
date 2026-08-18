use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crate::models::Activity;
use crate::remote::PanelClient;
use tokio::sync::RwLock;

/// In-memory buffer for activity events that are flushed to the panel on an
/// interval. Mirrors wings' SQLite-backed activity store + activity cron.
/// The buffer is capped: if the panel is unreachable for a long time the
/// oldest events are dropped instead of growing without bound.
pub struct ActivityCollector {
    buffer: Mutex<VecDeque<Activity>>,
    /// Soft cap: when the buffer exceeds this, the oldest entries are
    /// dropped (wings keeps the last 30 days in SQLite; an unbounded
    /// in-memory queue is the equivalent memory exhaustion vector).
    max_buffer: usize,
}

impl ActivityCollector {
    pub fn new() -> Self {
        Self {
            buffer: Mutex::new(VecDeque::new()),
            max_buffer: 10_000,
        }
    }

    pub fn push(&self, activity: Activity) {
        let mut buf = self.buffer.lock().unwrap();
        if buf.len() >= self.max_buffer {
            buf.pop_front();
        }
        buf.push_back(activity);
    }

    /// Take up to `limit` entries from the front of the queue.
    fn drain(&self, limit: usize) -> Vec<Activity> {
        let mut buf = self.buffer.lock().unwrap();
        let take = buf.len().min(limit);
        buf.drain(..take).collect()
    }

    fn requeue(&self, batch: Vec<Activity>) {
        let mut buf = self.buffer.lock().unwrap();
        for a in batch.into_iter().rev() {
            buf.push_front(a);
        }
    }

    /// Run until shutdown: every `interval` send up to `count` buffered
    /// events to the panel. Failed batches are re-queued for the next tick,
    /// mirroring wings (entries are only removed once accepted).
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
            let batch = self.drain(count);
            if batch.is_empty() {
                continue;
            }
            if let Err(e) = panel.read().await.send_activity_logs(&batch).await {
                tracing::warn!(error = %e, count = batch.len(), "failed to send activity logs, requeueing");
                self.requeue(batch);
            } else {
                tracing::debug!(count = batch.len(), "activity logs sent to panel");
            }
        }
    }
}