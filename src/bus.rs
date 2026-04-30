use anyhow::{Context, Result};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;

use crate::config::queues;
use crate::messages::{ExtensionEvent, Message};

/// Wraps a Redis `ConnectionManager` so every agent has a small, ergonomic API
/// for the queue patterns we need: BLPOP for inbox polling, RPUSH for
/// dispatching, and PUBLISH for fanning events out to the extension.
#[derive(Clone)]
pub struct Bus {
    conn: ConnectionManager,
}

impl Bus {
    pub async fn connect(url: &str) -> Result<Self> {
        let client = redis::Client::open(url)
            .with_context(|| format!("opening redis client {url}"))?;
        let conn = ConnectionManager::new(client)
            .await
            .context("connecting redis")?;
        Ok(Self { conn })
    }

    /// Push a message onto the destination agent's inbox queue.
    pub async fn dispatch(&self, message: &Message) -> Result<()> {
        let payload = serde_json::to_string(message)?;
        let queue = message.to.queue();
        let mut conn = self.conn.clone();
        let _: () = conn.rpush(queue, payload).await?;
        Ok(())
    }

    /// Block (with a timeout) on the agent's inbox and return the next
    /// message. Returns `None` when the timeout expires; callers loop and call
    /// us again so we never starve other tokio tasks.
    pub async fn next_message(
        &self,
        queue: &str,
        timeout_secs: f64,
    ) -> Result<Option<Message>> {
        let mut conn = self.conn.clone();
        let res: Option<(String, String)> =
            conn.blpop(queue, timeout_secs).await.with_context(|| {
                format!("blpop on {queue}")
            })?;
        let Some((_, payload)) = res else {
            return Ok(None);
        };
        let message: Message = serde_json::from_str(&payload).with_context(|| {
            format!("decoding message from {queue}: {payload}")
        })?;
        Ok(Some(message))
    }

    /// Publish an extension-bound event. The websocket layer subscribes to
    /// `events:extension` and forwards everything to the active client.
    pub async fn publish_event(&self, event: &ExtensionEvent) -> Result<()> {
        let payload = serde_json::to_string(event)?;
        let mut conn = self.conn.clone();
        let _: () = conn.publish(queues::EVENTS_CHANNEL, payload).await?;
        Ok(())
    }

    /// Set a string key (used by the file-structure agent to cache the latest
    /// workspace tree).
    pub async fn set_string(&self, key: &str, value: &str) -> Result<()> {
        let mut conn = self.conn.clone();
        let _: () = conn.set(key, value).await?;
        Ok(())
    }

    pub async fn get_string(&self, key: &str) -> Result<Option<String>> {
        let mut conn = self.conn.clone();
        let value: Option<String> = conn.get(key).await?;
        Ok(value)
    }
}
