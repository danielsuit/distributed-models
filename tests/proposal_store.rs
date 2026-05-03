//! Pure unit tests for the in-memory proposal registry.

use std::time::Duration;

use distributed_models::proposals::ProposalStore;
use tokio::time::timeout;

#[tokio::test]
async fn register_and_resolve_with_accept() {
    let store = ProposalStore::new();
    let receiver = store.register("p1".to_string());

    assert!(store.resolve("p1", true), "expected a waiter to exist");

    let result = timeout(Duration::from_millis(100), receiver)
        .await
        .expect("receiver should resolve quickly")
        .expect("oneshot must succeed");
    assert!(result, "accepted proposals should resolve to true");
}

#[tokio::test]
async fn register_and_resolve_with_reject() {
    let store = ProposalStore::new();
    let receiver = store.register("p2".to_string());
    assert!(store.resolve("p2", false));
    assert!(!receiver.await.unwrap());
}

#[tokio::test]
async fn resolve_unknown_proposal_returns_false() {
    let store = ProposalStore::new();
    assert!(!store.resolve("does-not-exist", true));
}

#[tokio::test]
async fn proposal_can_only_be_resolved_once() {
    let store = ProposalStore::new();
    let _rx = store.register("p3".to_string());
    assert!(store.resolve("p3", true));
    assert!(
        !store.resolve("p3", true),
        "second resolution should report no waiter"
    );
}

#[tokio::test]
async fn store_handles_concurrent_proposals() {
    let store = ProposalStore::new();
    let mut handles = Vec::new();
    for i in 0..16 {
        let id = format!("c{i}");
        let rx = store.register(id.clone());
        let store_clone = store.clone();
        handles.push(tokio::spawn(async move {
            let accepted = i % 2 == 0;
            store_clone.resolve(&id, accepted);
            (i, rx.await.unwrap())
        }));
    }
    for handle in handles {
        let (i, accepted) = handle.await.unwrap();
        assert_eq!(accepted, i % 2 == 0);
    }
}
