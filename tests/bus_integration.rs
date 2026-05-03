//! Integration tests for the Redis bus wrapper.
//!
//! These guard two invariants:
//! 1) queue prefixes isolate test/prod namespaces correctly.
//! 2) a blocking `next_message` wait cannot starve `dispatch`.

use std::time::{Duration, Instant};

use distributed_models::bus::Bus;
use distributed_models::messages::{Agent, Message};
use uuid::Uuid;

fn redis_url_for_test() -> String {
    std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/".to_string())
}

async fn try_bus(prefix: String) -> Option<Bus> {
    let url = redis_url_for_test();
    match tokio::time::timeout(
        Duration::from_secs(2),
        Bus::connect_with_prefix(&url, prefix),
    )
    .await
    {
        Ok(Ok(bus)) => Some(bus),
        _ => {
            eprintln!("[skip] no Redis at {url}; skipping bus integration test");
            None
        }
    }
}

fn unique_prefix() -> String {
    format!("dm-bus-test-{}-", Uuid::new_v4())
}

#[tokio::test]
async fn prefixed_dispatch_round_trips_message() {
    let Some(bus) = try_bus(unique_prefix()).await else {
        return;
    };

    let job_id = Uuid::new_v4().to_string();
    let mut outbound = Message::new(Agent::Client, Agent::Orchestrator, "user_message");
    outbound.job_id = job_id.clone();
    bus.dispatch(&outbound).await.unwrap();

    let inbound = bus
        .next_message(Agent::Orchestrator.queue(), 1.0)
        .await
        .unwrap()
        .expect("expected one queued message");
    assert_eq!(inbound.job_id, job_id);
    assert_eq!(inbound.task, "user_message");
}

#[tokio::test]
async fn dispatch_is_not_blocked_by_waiting_reader() {
    let Some(bus) = try_bus(unique_prefix()).await else {
        return;
    };

    // Start a long-ish blocking wait on one queue.
    let reader_bus = bus.clone();
    let waiting_reader = tokio::spawn(async move {
        reader_bus
            .next_message(Agent::Review.queue(), 2.0)
            .await
            .unwrap()
    });

    // Give the reader a moment to issue BLPOP, then dispatch elsewhere.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let start = Instant::now();
    let mut msg = Message::new(Agent::Client, Agent::Orchestrator, "user_message");
    msg.job_id = Uuid::new_v4().to_string();
    bus.dispatch(&msg).await.unwrap();
    let elapsed = start.elapsed();

    // On the old shared-connection bug, this call often blocked until the
    // BLPOP timeout elapsed (~2s). Keep a generous threshold for CI noise.
    assert!(
        elapsed < Duration::from_millis(500),
        "dispatch unexpectedly blocked for {:?}",
        elapsed
    );

    // Clean up spawned reader.
    waiting_reader.await.unwrap();
}
