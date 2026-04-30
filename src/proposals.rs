use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::oneshot;

/// In-memory registry of pending file write proposals. The orchestrator parks
/// a oneshot here when it pushes a proposal to the extension; the websocket
/// layer resolves it once the user clicks Accept or Reject.
#[derive(Clone, Default)]
pub struct ProposalStore {
    inner: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
}

impl ProposalStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a proposal id and return the receiver the caller should await.
    pub fn register(&self, proposal_id: String) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().insert(proposal_id, tx);
        rx
    }

    /// Resolve a proposal. Returns `true` if a waiter was notified.
    pub fn resolve(&self, proposal_id: &str, accepted: bool) -> bool {
        if let Some(tx) = self.inner.lock().remove(proposal_id) {
            let _ = tx.send(accepted);
            true
        } else {
            false
        }
    }
}
