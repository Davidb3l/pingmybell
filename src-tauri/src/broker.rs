//! Approval broker (§6): pending PreToolUse requests parked on oneshot
//! channels. The ingest handler registers a request and awaits the receiver
//! with a timeout; the UI's `decide` command completes it. Whoever takes the
//! entry first (decision vs. timeout) wins — the loser sees None.
//!
//! (§6 sketches a DashMap; a Mutex<HashMap> is equivalent at this load and
//! avoids a dependency.)

use std::collections::HashMap;
use std::sync::Mutex;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::registry::AgentKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

impl Decision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::Ask => "ask",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Decision::Allow),
            "deny" => Some(Decision::Deny),
            "ask" => Some(Decision::Ask),
            _ => None,
        }
    }
}

/// Everything the overlay card and the voicing need about one request.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalInfo {
    pub id: String,
    pub session_id: String,
    /// The permission_request row this approval belongs to, so the decision
    /// lands on the right history entry even with concurrent approvals.
    #[serde(skip)]
    pub event_id: i64,
    pub agent: AgentKind,
    /// Project title (cwd basename).
    pub title: String,
    pub tool_name: String,
    /// Primary tool input, already truncated for display (e.g. the bash
    /// command line) — derived data, never logged (§9).
    pub tool_summary: String,
}

struct Pending {
    tx: oneshot::Sender<Decision>,
    info: ApprovalInfo,
}

#[derive(Default)]
pub struct Broker {
    pending: Mutex<HashMap<String, Pending>>,
}

impl Broker {
    /// Park a new approval; returns its id and the receiver the HTTP handler
    /// awaits.
    pub fn register(&self, mut info: ApprovalInfo) -> (ApprovalInfo, oneshot::Receiver<Decision>) {
        let mut raw = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut raw);
        let id: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        info.id = id.clone();

        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("broker mutex poisoned").insert(
            id,
            Pending {
                tx,
                info: info.clone(),
            },
        );
        (info, rx)
    }

    /// Complete a pending approval. Returns its info if it was still pending
    /// (i.e. this call won the race against the timeout).
    pub fn decide(&self, id: &str, decision: Decision) -> Option<ApprovalInfo> {
        let entry = self
            .pending
            .lock()
            .expect("broker mutex poisoned")
            .remove(id)?;
        // If the receiver was dropped (handler already timed out and gave up
        // between our remove and this send), treat as not-pending.
        match entry.tx.send(decision) {
            Ok(()) => Some(entry.info),
            Err(_) => None,
        }
    }

    /// Drop a pending approval after the handler's timeout. Returns info if
    /// it was still pending.
    pub fn expire(&self, id: &str) -> Option<ApprovalInfo> {
        self.pending
            .lock()
            .expect("broker mutex poisoned")
            .remove(id)
            .map(|p| p.info)
    }

    /// Whether the session still has other approvals parked (used to decide
    /// if a resolved approval should flip the session back to Working).
    pub fn has_pending_for_session(&self, session_id: &str) -> bool {
        self.pending
            .lock()
            .expect("broker mutex poisoned")
            .values()
            .any(|p| p.info.session_id == session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ApprovalInfo {
        ApprovalInfo {
            id: String::new(),
            session_id: "s1".into(),
            event_id: 1,
            agent: AgentKind::ClaudeCode,
            title: "api-server".into(),
            tool_name: "Bash".into(),
            tool_summary: "cargo test".into(),
        }
    }

    #[tokio::test]
    async fn decide_resolves_parked_receiver() {
        let broker = Broker::default();
        let (registered, rx) = broker.register(info());
        assert_eq!(registered.id.len(), 32);

        let resolved = broker.decide(&registered.id, Decision::Allow);
        assert!(resolved.is_some());
        assert_eq!(rx.await.unwrap(), Decision::Allow);
    }

    #[tokio::test]
    async fn expire_wins_over_late_decision() {
        let broker = Broker::default();
        let (registered, rx) = broker.register(info());

        assert!(broker.expire(&registered.id).is_some());
        assert!(
            broker.decide(&registered.id, Decision::Allow).is_none(),
            "late decision must not report success"
        );
        assert!(rx.await.is_err(), "receiver sees the channel closed");
    }

    #[tokio::test]
    async fn unknown_id_is_none() {
        let broker = Broker::default();
        assert!(broker.decide("nope", Decision::Deny).is_none());
        assert!(broker.expire("nope").is_none());
    }
}
