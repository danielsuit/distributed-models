//! Integration agent.
//!
//! Runs after the code writer. Proposes **additional** file operations so new
//! assets and pages are actually reachable (CSS/HTML wiring, routers, nav).
//!
//! Prompts MUST include verbatim draft contents so the model can reason about
//! real code; outputs are sanitized so naive “edit” stubs cannot wipe HTML/CSS.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::json;

use crate::agents::{code_writer, AgentRuntime};
use crate::messages::{Agent, ClientEvent, CodeWriterResult, FileAction};

const POLL_TIMEOUT_SECS: f64 = 5.0;

/// Enough for most pages; truncation is marked so avoid edits to unseen regions.
const MAX_PROMPT_CHARS_PER_FILE: usize = 36_000;

const SYSTEM_PROMPT: &str = "You are an integration coherence agent.\n\
You MUST read every full file draft below together and reason whether they fit the user's request:\n\
- Cross-file sense: stylesheet linked or imported somewhere that runs, router entries match filenames, navigation leads to new pages.\n\
- Markup↔stylesheet: CSS class/id selectors should match `class=`/`id=` on HTML elements in the drafts for pages using that stylesheet; propose minimal edits if obvious orphan rules or bare tags that need hooks.\n\
- Fix wiring gaps ONLY when you can apply a **minimal, reversible** patch.\n\n\
Output ONE JSON object (no markdown, no prose outside JSON):\n\
{\"operations\":[{\"action\":\"create\"|\"edit\"|\"delete\",\"file\":\"<path>\",\"content\":\"<ONLY for create/edit>\"}],\"summary\":\"…\"}\n\n\
CRITICAL anti-data-loss rules (violations corrupt the user's project):\n\
1. For **edit** or **create** replacing an existing draft: `content` is the ENTIRE FINAL FILE after your change—not a snippet, fragment, `{}`, ellipsis, todo, placeholder, nor an empty/minimal skeleton.\n\
2. Start from the SOURCE TEXT shown for that path in sections **DRAFT FILE CONTENTS**. Copy ALL of it, then inject your small change (e.g. ONE `<link>`, `@import`, route line, `<a href>`).\n\
3. **Never** return an HTML/CSS file that clears or omits substantive content already in the draft. **Never** “simplify” the file to blanks.\n\
4. If you cannot faithfully preserve the draft text alongside your wiring change, respond with {\"operations\":[],\"summary\":\"\"}.\n\
5. Output **EXTRA** ops only—not a full remake of unrelated files.\n\
6. Use forward slashes; whole-file content for create/edit.\n";

pub async fn run(rt: AgentRuntime) {
    tracing::info!("integration agent online");
    loop {
        match step(&rt).await {
            Ok(_) => {}
            Err(err) => {
                tracing::error!("integration agent error: {err:?}");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn step(rt: &AgentRuntime) -> Result<()> {
    let Some(message) = rt
        .bus
        .next_message(Agent::Integration.queue(), POLL_TIMEOUT_SECS)
        .await?
    else {
        return Ok(());
    };

    rt.bus
        .publish_event(&ClientEvent::AgentStatus {
            job_id: message.job_id.clone(),
            agent: Agent::Integration,
            status: "checking cohesion and wiring".into(),
        })
        .await?;

    let task = message
        .context
        .get("instruction")
        .and_then(|v| v.as_str())
        .unwrap_or(&message.task)
        .to_string();
    let user_message = message
        .context
        .get("user_message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let workspace_root = message
        .context
        .get("workspace_root")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let related_files = message
        .context
        .get("related_files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect::<Vec<_>>();

    let draft: CodeWriterResult = serde_json::from_value(
        message
            .context
            .get("code_writer_result")
            .cloned()
            .unwrap_or_default(),
    )
    .unwrap_or_default();

    let draft_bodies = format_draft_contents_for_prompt(&draft);
    let outline: Vec<String> = draft
        .operations
        .iter()
        .map(|op| {
            let action_str = match op.action {
                FileAction::Create => "create",
                FileAction::Edit => "edit",
                FileAction::Delete => "delete",
            };
            format!(
                "- {action_str} {} ({} chars in draft)",
                op.file,
                op.content.as_ref().map(|c| c.chars().count()).unwrap_or(0)
            )
        })
        .collect();

    let prompt = format!(
		"Workspace root: {workspace_root}\n\
Relevant paths (hints from indexer): {related_files:?}\n\n\
USER REQUEST:\n{user_message}\n\n\
PLANNER FOCUS:\n{task}\n\n\
DRAFT OVERVIEW ({count} op(s)):\n{outline}\n\n\
--- DRAFT FILE CONTENTS (source of truth; integration edits MUST preserve these verbatim plus your addition) ---\n\
{draft_bodies}\n\
--- END DRAFT FILE CONTENTS ---\n\n\
Return ONLY the JSON envelope: extra wiring operations needed, OR empty operations.",
		outline = outline.join("\n"),
		count = draft.operations.len(),
	);

    rt.emit_prompt_estimate(
        message.job_id.as_str(),
        Agent::Integration,
        Some(SYSTEM_PROMPT),
        &prompt,
    )
    .await;
    let raw = match rt
        .ollama
        .generate(
            &rt.model_for(Agent::Integration).await,
            Some(SYSTEM_PROMPT),
            &prompt,
            rt.config.ollama_num_ctx,
        )
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!("integration Ollama call failed: {err}");
            let response = message
                .reply(message.from, "integration_result")
                .with_result(json!(CodeWriterResult::default()));
            rt.bus.dispatch(&response).await?;
            return Ok(());
        }
    };

    let parsed = code_writer::parse_operations_envelope_or_empty(&raw);
    let sanitized = sanitize_integration_against_draft(&draft, parsed);

    let response = message
        .reply(message.from, "integration_result")
        .with_result(json!(sanitized));
    rt.bus.dispatch(&response).await?;
    Ok(())
}

fn normalize_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

fn format_draft_contents_for_prompt(draft: &CodeWriterResult) -> String {
    let mut out = String::new();
    for op in &draft.operations {
        if matches!(op.action, FileAction::Delete) {
            out.push_str(&format!(
                "### DELETE `{}` ###\n\n",
                normalize_path(&op.file)
            ));
            continue;
        }
        let Some(content) = &op.content else {
            out.push_str(&format!(
                "### `{}` (no embedded content in draft) ###\n\n",
                normalize_path(&op.file)
            ));
            continue;
        };
        let p = normalize_path(&op.file);
        let truncated = content.chars().count() > MAX_PROMPT_CHARS_PER_FILE;
        let excerpt: String = if truncated {
            content.chars().take(MAX_PROMPT_CHARS_PER_FILE).collect()
        } else {
            content.clone()
        };
        out.push_str(&format!("### FILE `{p}` ###\n"));
        if truncated {
            out.push_str(&format!(
                "[TRUNCATED: showing first {} chars of {}]\n\n",
                excerpt.len(),
                content.len()
            ));
        }
        out.push_str(&excerpt);
        out.push_str("\n\n");
    }
    out
}

/// Last write wins per path among draft ops (mirrors sequential apply semantics).
pub(crate) fn draft_content_baselines(draft: &CodeWriterResult) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for op in &draft.operations {
        let key = normalize_path(&op.file);
        if matches!(op.action, FileAction::Delete) {
            m.remove(&key);
            continue;
        }
        if let Some(content) = &op.content {
            m.insert(key, content.clone());
        }
    }
    m
}

/// Remove integration edits that slash size vs the draft baseline for the same
/// path (common failure mode when the model guesses without seeing full text).
pub(crate) fn sanitize_integration_against_draft(
    draft: &CodeWriterResult,
    extras: CodeWriterResult,
) -> CodeWriterResult {
    let baseline = draft_content_baselines(draft);

    let threshold = move |baseline_len: usize| -> usize {
        if baseline_len < 260 {
            return 120;
        }
        // Roughly: new body should keep at least ~45% of prior length for non-trivial files.
        ((baseline_len as f64) * 0.45).floor() as usize
    };

    let mut kept = Vec::new();
    for mut op in extras.operations {
        let key = normalize_path(&op.file);
        if matches!(op.action, FileAction::Delete) {
            kept.push(op);
            continue;
        }
        let proposed = match op.content.take() {
            Some(c) => c,
            None => {
                tracing::warn!(
                    "dropping integration op with no content ({:?} `{}`)",
                    op.action,
                    op.file
                );
                continue;
            }
        };

        if let Some(prev) = baseline.get(&key) {
            let b_len = prev.len();
            let p_len = proposed.len();
            let min_keep = threshold(b_len);
            let prev_non_ws = prev.chars().filter(|c| !c.is_whitespace()).count();
            let prop_non_ws = proposed.chars().filter(|c| !c.is_whitespace()).count();

            let wipe_suspected = (b_len > 200 && p_len < min_keep)
                || (prev_non_ws > 400 && prop_non_ws * 10 < prev_non_ws.max(400))
                || (prev_non_ws > 80 && proposed.trim().is_empty());

            if wipe_suspected {
                tracing::warn!(
                    "dropping destructive-looking integration {:?} `{}` ({} -> {} chars)",
                    op.action,
                    op.file,
                    b_len,
                    p_len,
                );
                continue;
            }
            op.content = Some(proposed);
            kept.push(op);
        } else {
            // Editing a path not touched in this draft: keep (no baseline); prompt had less context.
            op.content = Some(proposed);
            kept.push(op);
        }
    }

    CodeWriterResult {
        summary: if kept.is_empty() {
            String::new()
        } else {
            extras.summary
        },
        operations: kept,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::FileOperation;

    #[test]
    fn sanitization_drops_tiny_integration_edit_when_draft_was_large_html() {
        let draft = CodeWriterResult {
            operations: vec![FileOperation::create(
                "src/index.html",
                "<html><head></head><body><h1>Hello</h1><p>x".repeat(40) + "</p></body></html>",
            )],
            summary: String::new(),
            ..Default::default()
        };
        let bad = CodeWriterResult {
            operations: vec![FileOperation::edit(
                "src/index.html",
                "<html><head></head><body></body></html>",
            )],
            summary: "link".into(),
            ..Default::default()
        };
        let out = sanitize_integration_against_draft(&draft, bad);
        assert!(
            out.operations.is_empty(),
            "wipe-like replacement should be dropped"
        );
    }

    #[test]
    fn sanitization_keeps_integration_edit_that_preserves_most_content() {
        let body = "<html><head></head><body>\nCONTENT\n"
            .to_string()
            .repeat(30)
            + "</body></html>";
        let draft = CodeWriterResult {
            operations: vec![FileOperation::create("index.html", body.clone())],
            summary: String::new(),
            ..Default::default()
        };
        let head_end = body.find("</head>").unwrap();
        let mut patched = body;
        patched.insert_str(head_end, r#"<link rel="stylesheet" href="x.css"/>"#);
        let good = CodeWriterResult {
            operations: vec![FileOperation::edit("index.html", patched)],
            summary: "linked css".into(),
            ..Default::default()
        };
        let out = sanitize_integration_against_draft(&draft, good);
        assert_eq!(out.operations.len(), 1);
        assert!(out.operations[0]
            .content
            .as_ref()
            .unwrap()
            .contains("CONTENT"));
    }
}
