//! Wire-format tests. These pin the JSON shapes that agents and the editor
//! rely on. If you change a message type, update the snapshots below
//! deliberately.

use distributed_models::messages::{
    Agent, ChatRequest, ChatResponse, ClientEvent, CodeWriterResult, DiagnosticEntry,
    DiagnosticsRequest, FileAction, FileChange, FileChangeRequest, FileEntry, FileOperation,
    FileSnapshotRequest, Message, ProposalDecisionRequest, ReviewVerdict,
};
use serde_json::json;

#[test]
fn file_operation_serializes_with_action_field() {
    let op = FileOperation::create("src/main.rs", "fn main() {}");
    let serialized = serde_json::to_value(&op).unwrap();
    assert_eq!(
        serialized,
        json!({
            "action": "create",
            "file": "src/main.rs",
            "content": "fn main() {}",
        })
    );

    let delete = FileOperation::delete("old.rs");
    let serialized = serde_json::to_value(&delete).unwrap();
    assert_eq!(
        serialized,
        json!({
            "action": "delete",
            "file": "old.rs",
        }),
        "delete operations must omit the content field"
    );
}

#[test]
fn file_operation_roundtrips_for_each_action() {
    for action in ["create", "edit", "delete"] {
        let body = if action == "delete" {
            json!({ "action": action, "file": "x.txt" })
        } else {
            json!({
                "action": action,
                "file": "x.txt",
                "content": "abc",
            })
        };
        let parsed: FileOperation = serde_json::from_value(body.clone()).unwrap();
        let reserialized = serde_json::to_value(&parsed).unwrap();
        assert_eq!(reserialized, body, "round-trip for action `{action}`");
    }
}

#[test]
fn agent_enum_uses_lowercase_in_json() {
    let agents = [
        Agent::Orchestrator,
        Agent::FileStructure,
        Agent::CodeWriter,
        Agent::ErrorAgent,
        Agent::Review,
        Agent::Integration,
        Agent::Client,
    ];
    let serialized: Vec<String> = agents
        .iter()
        .map(|a| serde_json::to_string(a).unwrap())
        .collect();
    assert_eq!(
        serialized,
        vec![
            "\"orchestrator\"",
            "\"filestructure\"",
            "\"codewriter\"",
            "\"erroragent\"",
            "\"review\"",
            "\"integration\"",
            "\"client\"",
        ]
    );
}

#[test]
fn agent_queue_names_match_spec() {
    assert_eq!(Agent::Orchestrator.queue(), "agent:orchestrator");
    assert_eq!(Agent::FileStructure.queue(), "agent:filestructure");
    assert_eq!(Agent::CodeWriter.queue(), "agent:codewriter");
    assert_eq!(Agent::ErrorAgent.queue(), "agent:error");
    assert_eq!(Agent::Review.queue(), "agent:review");
    assert_eq!(Agent::Integration.queue(), "agent:integration");
}

#[test]
fn message_carries_required_fields() {
    let msg = Message::new(Agent::Client, Agent::Orchestrator, "user_message")
        .with_job("job-123")
        .with_context(json!({ "user_message": "hi" }));
    let serialized = serde_json::to_value(&msg).unwrap();

    for field in [
        "id",
        "job_id",
        "from",
        "to",
        "task",
        "context",
        "result",
        "timestamp",
    ] {
        assert!(
            serialized.get(field).is_some(),
            "Message JSON must include `{field}`",
        );
    }
    assert_eq!(serialized["job_id"], "job-123");
    assert_eq!(serialized["from"], "client");
    assert_eq!(serialized["to"], "orchestrator");
    assert_eq!(serialized["context"]["user_message"], "hi");
}

#[test]
fn message_reply_preserves_job_and_context() {
    let original = Message::new(Agent::Orchestrator, Agent::CodeWriter, "write_code")
        .with_job("j1")
        .with_context(json!({ "instruction": "write hello" }));
    let reply = original.reply(Agent::Orchestrator, "code_writer_result");
    assert_eq!(reply.job_id, "j1");
    assert_eq!(reply.from, Agent::CodeWriter);
    assert_eq!(reply.to, Agent::Orchestrator);
    assert_eq!(reply.context, original.context);
    assert_eq!(reply.task, "code_writer_result");
}

#[test]
fn client_events_use_snake_case_type_tag() {
    let event = ClientEvent::AgentStatus {
        job_id: "j".into(),
        agent: Agent::Orchestrator,
        status: "planning".into(),
    };
    let serialized = serde_json::to_value(&event).unwrap();
    assert_eq!(serialized["type"], "agent_status");

    let event = ClientEvent::FileProposal {
        job_id: "j".into(),
        proposal_id: "p".into(),
        operation: FileOperation::create("x", "y"),
        review_notes: None,
    };
    let serialized = serde_json::to_value(&event).unwrap();
    assert_eq!(serialized["type"], "file_proposal");
    assert_eq!(serialized["operation"]["action"], "create");
}

#[test]
fn prompt_estimate_event_serializes() {
    let event = ClientEvent::PromptEstimate {
        job_id: "j".into(),
        agent: Agent::CodeWriter,
        approximate_tokens: 12_340,
    };
    let serialized = serde_json::to_value(&event).unwrap();
    assert_eq!(serialized["type"], "prompt_estimate");
    assert_eq!(serialized["job_id"], "j");
    assert_eq!(serialized["agent"], "codewriter");
    assert_eq!(serialized["approximate_tokens"], 12_340);
}

#[test]
fn rest_request_payloads_round_trip() {
    let chat = ChatRequest {
        text: "hi".into(),
        workspace_root: Some("/ws".into()),
        history: Vec::new(),
    };
    let chat_json = serde_json::to_value(&chat).unwrap();
    assert_eq!(chat_json["text"], "hi");
    assert_eq!(chat_json["workspace_root"], "/ws");
    assert_eq!(chat_json["history"], serde_json::json!([]));

    let chat_response: ChatResponse = serde_json::from_value(json!({ "job_id": "abc" })).unwrap();
    assert_eq!(chat_response.job_id, "abc");

    let snapshot = FileSnapshotRequest {
        workspace_root: "/ws".into(),
        files: vec![FileEntry {
            path: "a.rs".into(),
            size: 12,
            is_dir: false,
            symbols: None,
        }],
    };
    let _ = serde_json::to_string(&snapshot).unwrap();

    let change = FileChangeRequest {
        workspace_root: "/ws".into(),
        change: FileChange::Created {
            path: "src/new.rs".into(),
        },
    };
    let serialized = serde_json::to_value(&change).unwrap();
    assert_eq!(serialized["change"]["kind"], "created");
    assert_eq!(serialized["change"]["path"], "src/new.rs");

    let diagnostics = DiagnosticsRequest {
        workspace_root: "/ws".into(),
        diagnostics: vec![DiagnosticEntry {
            file: "a.rs".into(),
            line: 1,
            column: 2,
            severity: "error".into(),
            message: "boom".into(),
            source: Some("rustc".into()),
        }],
    };
    let serialized = serde_json::to_value(&diagnostics).unwrap();
    assert_eq!(serialized["diagnostics"][0]["severity"], "error");

    let decision: ProposalDecisionRequest =
        serde_json::from_value(json!({ "accepted": true })).unwrap();
    assert!(decision.accepted);
}

#[test]
fn code_writer_result_default_is_empty() {
    let result = CodeWriterResult::default();
    assert!(result.operations.is_empty());
    assert!(result.summary.is_empty());
}

#[test]
fn review_verdict_round_trips() {
    let verdict = ReviewVerdict {
        approved: true,
        reason: "looks good".into(),
        problems: vec!["stub".into()],
    };
    let serialized = serde_json::to_string(&verdict).unwrap();
    let parsed: ReviewVerdict = serde_json::from_str(&serialized).unwrap();
    assert_eq!(parsed, verdict);
}

#[test]
fn file_action_lowercase_serialization() {
    assert_eq!(
        serde_json::to_string(&FileAction::Create).unwrap(),
        "\"create\""
    );
    assert_eq!(
        serde_json::to_string(&FileAction::Edit).unwrap(),
        "\"edit\""
    );
    assert_eq!(
        serde_json::to_string(&FileAction::Delete).unwrap(),
        "\"delete\""
    );
}
