//! Slash-command registry. Mirrors Claude Code / Cursor behaviour: when the
//! user message begins with `/`, the HTTP handler looks the command up here
//! before dispatching to the orchestrator. Two outcomes are possible:
//!
//! - `Direct` — the backend answers the user immediately (e.g. `/help`).
//! - `Rewrite` — the user message is replaced with a richer prompt that
//!   the orchestrator then plans against. This keeps the rest of the
//!   pipeline unchanged: planner → file structure → code writer (with its
//!   tool-use loop, bash, etc.) → review.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashOutcome {
    /// Skip the orchestrator entirely and surface this text to the user
    /// as the assistant message for the job.
    Direct(String),
    /// Replace the user message with this rewritten prompt before
    /// dispatching to the orchestrator.
    Rewrite(String),
}

#[derive(Debug, Clone, Copy)]
pub struct SlashCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub summary: &'static str,
    handler: fn(&str) -> SlashOutcome,
}

impl SlashCommand {
    pub fn handle(&self, args: &str) -> SlashOutcome {
        (self.handler)(args)
    }
}

pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        aliases: &["?", "commands"],
        summary: "List available slash commands.",
        handler: handle_help,
    },
    SlashCommand {
        name: "clear",
        aliases: &["reset"],
        summary: "Reset the conversation. (The editor handles UI state; backend has nothing to clear.)",
        handler: handle_clear,
    },
    SlashCommand {
        name: "init",
        aliases: &["bootstrap", "scaffold-claude-md"],
        summary: "Read the project, draft a CLAUDE.md style guide for future sessions.",
        handler: handle_init,
    },
    SlashCommand {
        name: "review",
        aliases: &["code-review", "audit"],
        summary: "Review the current branch (git diff/status) and report issues.",
        handler: handle_review,
    },
    SlashCommand {
        name: "test",
        aliases: &["run-tests", "ci"],
        summary: "Run the project's test suite via bash and report results.",
        handler: handle_test,
    },
    SlashCommand {
        name: "commit",
        aliases: &["git-commit"],
        summary: "Stage changes, draft a commit message, and run `git commit`.",
        handler: handle_commit,
    },
    SlashCommand {
        name: "explain",
        aliases: &["describe"],
        summary: "Explain a file or symbol. Pass a path or symbol name as the argument.",
        handler: handle_explain,
    },
    SlashCommand {
        name: "fix",
        aliases: &["patch", "repair"],
        summary: "Locate and fix a described bug. Provide a short description of the issue.",
        handler: handle_fix,
    },
];

/// Look the input up. Returns `None` when the message does not begin with
/// `/` or no command matches — in which case the caller proceeds with the
/// raw user text. Unknown `/foo` inputs return `Some(Direct(error))` so
/// the user gets feedback instead of silent fallthrough.
pub fn resolve(input: &str) -> Option<SlashOutcome> {
    let trimmed = input.trim_start();
    let body = trimmed.strip_prefix('/')?;
    let (head, rest) = match body.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r),
        None => (body, ""),
    };
    let head_lower = head.to_lowercase();
    let head_lower = head_lower.trim_end_matches(':');

    let cmd = COMMANDS
        .iter()
        .find(|c| c.name == head_lower || c.aliases.contains(&head_lower));

    match cmd {
        Some(cmd) => Some(cmd.handle(rest.trim())),
        None => Some(SlashOutcome::Direct(format!(
            "Unknown command `/{head}`. Try `/help` to list commands."
        ))),
    }
}

fn handle_help(_args: &str) -> SlashOutcome {
    let mut out = String::from("**Slash commands**\n\n");
    for cmd in COMMANDS {
        let _ = write!(out, "- `/{}`", cmd.name);
        if !cmd.aliases.is_empty() {
            out.push_str(&format!(
                " (aliases: {})",
                cmd.aliases
                    .iter()
                    .map(|a| format!("`/{a}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        out.push_str(" — ");
        out.push_str(cmd.summary);
        out.push('\n');
    }
    out.push_str("\nAnything not starting with `/` is sent to the orchestrator as a normal request.");
    SlashOutcome::Direct(out)
}

fn handle_clear(_args: &str) -> SlashOutcome {
    SlashOutcome::Direct(
        "Conversation cleared on the client side. The backend is stateless across chats — your next request starts fresh.".to_string(),
    )
}

fn handle_init(args: &str) -> SlashOutcome {
    let extra = if args.is_empty() {
        String::new()
    } else {
        format!("\n\nUser hint: {args}")
    };
    SlashOutcome::Rewrite(format!(
        "Initialise this project for AI-assisted work. \
Read README.md (if present), top-level package files (Cargo.toml, package.json, pyproject.toml, etc.), \
and at least one representative source file. Then create or update `CLAUDE.md` at the workspace root \
with: (1) one-paragraph project summary, (2) build/run/test commands, (3) key directories, (4) coding \
conventions you observed, (5) anything a fresh agent should know to be productive. \
Use the read_file/list_dir/glob tools first; do not guess.{extra}"
    ))
}

fn handle_review(args: &str) -> SlashOutcome {
    let scope = if args.is_empty() {
        "the current uncommitted changes (working tree + index)".to_string()
    } else {
        format!("the changes implied by: {args}")
    };
    SlashOutcome::Rewrite(format!(
        "Review {scope}. Use `bash` to run `git status`, `git diff` (and `git diff --cached` for staged \
changes). Read each modified file with `read_file` to see context. Report: \
(1) bugs / logic errors, (2) security or correctness concerns, (3) style or clarity nits, \
(4) tests that should be added. Do NOT make code changes — review only. Finish with a concise summary."
    ))
}

fn handle_test(args: &str) -> SlashOutcome {
    let invocation = if args.is_empty() {
        "Detect the project's test command (cargo test / npm test / pytest / etc.) by inspecting \
package files, then run it via the bash tool".to_string()
    } else {
        format!("Run via bash: `{args}`")
    };
    SlashOutcome::Rewrite(format!(
        "{invocation}. If tests fail, read the relevant source files, identify the failure cause, \
and (if straightforward) propose a fix as an `edit`. If the failure is non-trivial, just summarise \
the failure for the user."
    ))
}

fn handle_commit(args: &str) -> SlashOutcome {
    let extra = if args.is_empty() {
        String::new()
    } else {
        format!(" The user's commit hint: {args}.")
    };
    SlashOutcome::Rewrite(format!(
        "Create a git commit for the current uncommitted changes. \
First run `git status` and `git diff` (via bash) so you understand what is being committed. \
Draft a concise commit message focused on WHY rather than WHAT. \
Stage the relevant files (avoid .env / credentials), then run `git commit -m '<message>'` via bash. \
Do not push. Show the resulting `git log -1 --oneline`.{extra}"
    ))
}

fn handle_explain(args: &str) -> SlashOutcome {
    if args.is_empty() {
        return SlashOutcome::Direct(
            "Usage: `/explain <path-or-symbol>` — e.g. `/explain src/agents/orchestrator.rs` or `/explain ToolSession`."
                .to_string(),
        );
    }
    SlashOutcome::Rewrite(format!(
        "Explain `{args}` to the user. If it looks like a path, read that file (and any closely \
related files) with read_file. If it looks like a symbol, use grep to find the definition then read \
its file. Cover: (1) responsibility, (2) key functions/types, (3) how it integrates with the rest \
of the codebase. Keep it focused — under 250 words."
    ))
}

fn handle_fix(args: &str) -> SlashOutcome {
    if args.is_empty() {
        return SlashOutcome::Direct(
            "Usage: `/fix <bug description>` — e.g. `/fix /events drops events when there are no subscribers`."
                .to_string(),
        );
    }
    SlashOutcome::Rewrite(format!(
        "Locate and fix this bug: {args}. \
Use grep / glob / read_file to find the relevant code. Confirm the bug exists by reading the actual \
implementation; do not assume. Then propose a minimal `edit` and, if practical, an `edit` adding a \
regression test. Each edit will be approved by the user before applying."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_command_returns_direct_error() {
        match resolve("/notarealthing") {
            Some(SlashOutcome::Direct(text)) => assert!(text.contains("Unknown")),
            other => panic!("expected Direct, got {other:?}"),
        }
    }

    #[test]
    fn non_slash_input_returns_none() {
        assert!(resolve("just a regular message").is_none());
        assert!(resolve("").is_none());
    }

    #[test]
    fn help_lists_every_command() {
        match resolve("/help") {
            Some(SlashOutcome::Direct(text)) => {
                for cmd in COMMANDS {
                    assert!(
                        text.contains(&format!("/{}", cmd.name)),
                        "/help missing /{} (text: {text})",
                        cmd.name
                    );
                }
            }
            other => panic!("expected help Direct, got {other:?}"),
        }
    }

    #[test]
    fn alias_resolves_to_canonical_handler() {
        match resolve("/?") {
            Some(SlashOutcome::Direct(_)) => {}
            other => panic!("expected /? alias to map to /help, got {other:?}"),
        }
    }

    #[test]
    fn init_returns_rewrite_with_user_hint() {
        match resolve("/init focus on the agents folder") {
            Some(SlashOutcome::Rewrite(prompt)) => {
                assert!(prompt.contains("CLAUDE.md"));
                assert!(prompt.contains("agents folder"));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn review_with_no_args_targets_uncommitted_changes() {
        match resolve("/review") {
            Some(SlashOutcome::Rewrite(prompt)) => {
                assert!(prompt.contains("git diff"));
                assert!(prompt.contains("uncommitted"));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[test]
    fn explain_without_target_returns_usage_hint() {
        match resolve("/explain") {
            Some(SlashOutcome::Direct(text)) => assert!(text.contains("Usage")),
            other => panic!("expected Direct usage hint, got {other:?}"),
        }
    }

    #[test]
    fn clear_short_circuits_to_direct() {
        match resolve("/clear") {
            Some(SlashOutcome::Direct(_)) => {}
            other => panic!("expected Direct, got {other:?}"),
        }
    }
}
