//! Cooperative cancellation tokens for chat jobs. The HTTP layer records a
//! cancel request; the orchestrator observes it between subtasks and while
//! waiting for agent replies or proposal decisions.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use parking_lot::Mutex;

/// Returned from [`run_job`](crate::agents::orchestrator) when the user stops
/// the run from the editor.
#[derive(Debug, Clone, Copy)]
pub struct JobCancelled;

impl fmt::Display for JobCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("job cancelled")
    }
}

impl std::error::Error for JobCancelled {}

/// Thread-safe set of job ids that should stop ASAP.
#[derive(Clone, Default)]
pub struct JobCancellation {
    ids: Arc<Mutex<HashSet<String>>>,
}

impl JobCancellation {
    pub fn request_cancel(&self, job_id: &str) {
        self.ids.lock().insert(job_id.to_string());
    }

    pub fn is_cancelled(&self, job_id: &str) -> bool {
        self.ids.lock().contains(job_id)
    }

    pub fn clear(&self, job_id: &str) {
        self.ids.lock().remove(job_id);
    }
}
