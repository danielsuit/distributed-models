//! Tests for the lenient parsers each agent uses to consume model output.
//! These exist so behaviour stays stable when we swap models or tweak
//! system prompts.

use distributed_models::agents::code_writer::{
    parse_code_writer_output, parse_operations_envelope_or_empty,
};
use distributed_models::agents::file_structure::parse_ranked_paths;
use distributed_models::agents::orchestrator::parse_plan;
use distributed_models::agents::review::parse_verdict;
use distributed_models::cli::parse_sse_data;
use distributed_models::messages::FileAction;

#[test]
fn code_writer_parses_strict_envelope() {
    let raw = r#"{
        "operations": [
            { "action": "create", "file": "src/main.rs", "content": "fn main() {}" }
        ],
        "summary": "Created entrypoint."
    }"#;
    let parsed = parse_code_writer_output(raw, "FALLBACK");
    assert_eq!(parsed.operations.len(), 1);
    assert_eq!(parsed.operations[0].action, FileAction::Create);
    assert_eq!(parsed.operations[0].file, "src/main.rs");
    assert_eq!(parsed.summary, "Created entrypoint.");
}

#[test]
fn integration_parser_returns_empty_on_gibberish() {
    let parsed = parse_operations_envelope_or_empty("not json at all");
    assert!(parsed.operations.is_empty());
    assert!(parsed.summary.is_empty());
}

#[test]
fn integration_parser_reads_envelope_without_fallback_file() {
    let raw = r#"{"operations":[{"action":"edit","file":"app.tsx","content":"export {}"}],"summary":"wired nav"}"#;
    let parsed = parse_operations_envelope_or_empty(raw);
    assert_eq!(parsed.operations.len(), 1);
    assert_eq!(parsed.summary, "wired nav");
}

#[test]
fn code_writer_strips_markdown_fences() {
    let raw = r#"```json
    {
        "operations": [
            { "action": "edit", "file": "lib.rs", "content": "// edited" }
        ],
        "summary": ""
    }
    ```"#;
    let parsed = parse_code_writer_output(raw, "FALLBACK");
    assert_eq!(parsed.operations.len(), 1);
    assert_eq!(parsed.operations[0].action, FileAction::Edit);
}

#[test]
fn code_writer_recovers_when_model_returns_array() {
    let raw = r#"[
        { "action": "create", "file": "a.txt", "content": "a" },
        { "action": "delete", "file": "b.txt" }
    ]"#;
    let parsed = parse_code_writer_output(raw, "FALLBACK");
    assert_eq!(parsed.operations.len(), 2);
    assert_eq!(parsed.operations[1].action, FileAction::Delete);
    assert!(parsed.operations[1].content.is_none());
}

#[test]
fn code_writer_recovers_when_model_returns_single_op() {
    let raw = r#"{ "action": "create", "file": "x", "content": "y" }"#;
    let parsed = parse_code_writer_output(raw, "FALLBACK");
    assert_eq!(parsed.operations.len(), 1);
    assert_eq!(parsed.operations[0].file, "x");
}

#[test]
fn code_writer_falls_back_to_create_for_garbage_input() {
    let parsed = parse_code_writer_output("not json at all", "src/fallback.rs");
    assert_eq!(parsed.operations.len(), 1);
    assert_eq!(parsed.operations[0].action, FileAction::Create);
    assert_eq!(parsed.operations[0].file, "src/fallback.rs");
    assert_eq!(
        parsed.operations[0].content.as_deref(),
        Some("not json at all")
    );
}

#[test]
fn code_writer_recovers_json_embedded_in_prose() {
    let raw = r#"Sure, here's the JSON:
{ "operations": [{ "action": "create", "file": "z.rs", "content": "zz" }], "summary": "" }
Hope that helps!"#;
    let parsed = parse_code_writer_output(raw, "FALLBACK");
    assert_eq!(parsed.operations.len(), 1);
    assert_eq!(parsed.operations[0].file, "z.rs");
}

#[test]
fn planner_parses_strict_json() {
    let raw = r#"{
        "plan": "Write hello world",
        "need_files": false,
        "file_query": "",
        "need_code": true,
        "target_file": "src/main.rs",
        "code_instruction": "Write a Rust hello world",
        "final_answer": ""
    }"#;
    let plan = parse_plan(raw).unwrap();
    assert!(plan.need_code);
    assert_eq!(plan.target_file, "src/main.rs");
}

#[test]
fn planner_extracts_json_from_prose() {
    let raw =
        "Here you go:\n{\"plan\":\"hi\",\"need_code\":false,\"final_answer\":\"hello\"}\nthanks";
    let plan = parse_plan(raw).unwrap();
    assert_eq!(plan.final_answer, "hello");
    assert!(!plan.need_code);
}

#[test]
fn planner_returns_none_for_garbage() {
    assert!(parse_plan("just some words").is_none());
}

#[test]
fn review_parses_verdict_json() {
    let raw = r#"{ "approved": false, "reason": "bad", "problems": ["missing import"] }"#;
    let verdict = parse_verdict(raw);
    assert!(!verdict.approved);
    assert_eq!(verdict.problems, vec!["missing import".to_string()]);
}

#[test]
fn review_defaults_to_approved_when_unparseable() {
    let verdict = parse_verdict("looks fine to me");
    assert!(verdict.approved);
    assert!(verdict.reason.contains("non-JSON"));
}

#[test]
fn review_extracts_json_from_fences() {
    let raw = "```json\n{\"approved\":true,\"reason\":\"\",\"problems\":[]}\n```";
    let verdict = parse_verdict(raw);
    assert!(verdict.approved);
}

#[test]
fn file_structure_keeps_only_known_paths() {
    let candidates = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
    let raw = "1. src/a.rs\n- src/c.rs (not in list)\nsrc/b.rs";
    let ranked = parse_ranked_paths(raw, &candidates);
    assert_eq!(ranked, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
}

#[test]
fn file_structure_falls_back_to_input_when_nothing_matches() {
    let candidates = vec!["src/a.rs".to_string()];
    let ranked = parse_ranked_paths("garbage\nmore garbage", &candidates);
    assert_eq!(ranked, candidates);
}

#[test]
fn sse_parser_extracts_data_from_event_block() {
    let block = "event: message\ndata: {\"hello\":\"world\"}";
    assert_eq!(
        parse_sse_data(block).as_deref(),
        Some("{\"hello\":\"world\"}")
    );
}

#[test]
fn sse_parser_concatenates_multiple_data_lines() {
    let block = "data: line1\ndata: line2";
    assert_eq!(parse_sse_data(block).as_deref(), Some("line1\nline2"));
}

#[test]
fn sse_parser_skips_comments() {
    let block = ": keepalive\ndata: payload";
    assert_eq!(parse_sse_data(block).as_deref(), Some("payload"));
}

#[test]
fn sse_parser_returns_none_for_empty_block() {
    assert!(parse_sse_data(": only a comment").is_none());
}
