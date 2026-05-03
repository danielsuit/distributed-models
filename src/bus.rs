use anyhow::{Context, Result};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use redis::Client;

use crate::config::queues;
use crate::messages::{ClientEvent, Message};

/// Thin wrapper around a Redis client. Every agent uses this for the three
/// patterns we care about: BLPOP for inbox polling, RPUSH for dispatching,
/// and PUBLISH for fanning events out to clients.
///
/// We keep two underlying handles:
/// * `write_conn` is a multiplexed connection used for non-blocking commands
///   (RPUSH, PUBLISH, GET, SET). Sharing the connection keeps overhead low
///   and lets the handlers stay `Clone`.
/// * `client` is held so we can hand out *dedicated* connections for blocking
///   commands like BLPOP. A blocking command on a multiplexed socket would
///   stall every other writer that shares it, which is fatal because the
///   HTTP handler's RPUSH would queue behind the agent's BLPOP.
///
/// A bus optionally carries a `prefix` that is prepended to every queue,
/// channel and key name. Production runs leave it empty; tests set it to a
/// per-run UUID so stray daemons or parallel tests can't collide on the
/// canonical queue names.
#[derive(Clone)]
pub struct Bus {
    client: Client,
    write_conn: ConnectionManager,
    prefix: String,
}

impl Bus {
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with_prefix(url, String::new()).await
    }

    pub async fn connect_with_prefix(url: &str, prefix: String) -> Result<Self> {
        let client = Client::open(url).with_context(|| format!("opening redis client {url}"))?;
        let write_conn = ConnectionManager::new(client.clone())
            .await
            .context("connecting redis")?;
        Ok(Self {
            client,
            write_conn,
            prefix,
        })
    }

    /// Active queue/channel namespace prefix. May be empty.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Apply the prefix to a canonical name.
    pub fn full_name(&self, base: &str) -> String {
        if self.prefix.is_empty() {
            base.to_string()
        } else {
            format!("{}{}", self.prefix, base)
        }
    }

    /// Push a message onto the destination agent's inbox queue.
    pub async fn dispatch(&self, message: &Message) -> Result<()> {
        let payload = serde_json::to_string(message)?;
        let queue = self.full_name(message.to.queue());
        let mut conn = self.write_conn.clone();
        let _: () = conn.rpush(&queue, payload).await?;
        Ok(())
    }

    /// Block (with a timeout) on the agent's inbox and return the next
    /// message. Returns `None` when the timeout expires; callers loop and call
    /// us again so we never starve other tokio tasks.
    ///
    /// IMPORTANT: this opens a *dedicated* Redis connection so the BLPOP
    /// can't stall RPUSH/PUBLISH commands the rest of the system issues on
    /// the multiplexed `write_conn`.
    pub async fn next_message(&self, queue: &str, timeout_secs: f64) -> Result<Option<Message>> {
        let queue = self.full_name(queue);
        let mut conn = self
            .client
            .get_multiplexed_async_connection()
            .await
            .with_context(|| format!("opening dedicated reader for {queue}"))?;
        let res: Option<(String, String)> = conn
            .blpop(&queue, timeout_secs)
            .await
            .with_context(|| format!("blpop on {queue}"))?;
        let Some((_, payload)) = res else {
            return Ok(None);
        };
        let message: Message = serde_json::from_str(&payload)
            .with_context(|| format!("decoding message from {queue}: {payload}"))?;
        Ok(Some(message))
    }

    /// Publish a client-bound event. The HTTP layer subscribes to the
    /// (possibly prefixed) events channel and forwards everything to active
    /// SSE connections.
    pub async fn publish_event(&self, event: &ClientEvent) -> Result<()> {
        let payload = serde_json::to_string(event)?;
        let channel = self.full_name(queues::EVENTS_CHANNEL);
        let mut conn = self.write_conn.clone();
        let _: () = conn.publish(&channel, payload).await?;
        Ok(())
    }

    pub async fn set_string(&self, key: &str, value: &str) -> Result<()> {
        let key = self.full_name(key);
        let mut conn = self.write_conn.clone();
        let _: () = conn.set(&key, value).await?;
        Ok(())
    }

    pub async fn get_string(&self, key: &str) -> Result<Option<String>> {
        let key = self.full_name(key);
        let mut conn = self.write_conn.clone();
        let value: Option<String> = conn.get(&key).await?;
        Ok(value)
    }
}
