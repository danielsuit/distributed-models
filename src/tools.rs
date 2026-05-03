//! Tool catalog used by the Code Writer's tool-use loop.
//!
//! Local Ollama models cannot reliably perform native tool calls, so we ask
//! them to emit one JSON object per turn — `{thought, tool, args}` — and
//! parse it leniently here. Read-only tools (`read_file`, `list_dir`, `grep`,
//! `glob`) execute against the workspace immediately and feed their output
//! back into the next prompt. Mutating tools (`edit`, `create`, `delete`)
//! update an in-memory virtual filesystem; when the loop terminates via
//! `finish`, the staged state is converted into a `CodeWriterResult` of
//! whole-file `FileOperation`s — preserving the existing accept/reject UI
//! contract that the editor and CLI already implement.
//!
//! `edit` accepts a search/replace pair (Claude-Code style). The search
//! string must match the current file VERBATIM and be unique unless
//! `replace_all` is set. The materialised whole-file content is what we hand
//! to the orchestrator, so callers see the same diff-style payload they
//! already render.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use tokio::fs;

use crate::messages::{CodeWriterResult, FileOperation};

const MAX_READ_BYTES: u64 = 256 * 1024;
const MAX_GREP_BYTES: u64 = 1_000_000;
const MAX_GREP_HITS: usize = 80;
const MAX_GREP_FILES: usize = 4_000;
const MAX_DIR_ENTRIES: usize = 200;
const MAX_GLOB_RESULTS: usize = 200;

/// Directories the workspace walker skips by default. Build artefacts and VCS
/// metadata almost never inform code edits and inflate prompts.
const WALK_SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    "vendor",
    "__pycache__",
    ".venv",
    "venv",
    ".gradle",
    ".idea",
    ".vscode",
    "out",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCall {
    ReadFile {
        path: String,
        start_line: Option<usize>,
        end_line: Option<usize>,
    },
    ListDir {
        path: String,
    },
    Grep {
        pattern: String,
        path: Option<String>,
    },
    Glob {
        pattern: String,
    },
    Edit {
        path: String,
        search: String,
        replace: String,
        replace_all: bool,
    },
    Create {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Finish {
        summary: String,
    },
    /// Run a shell command in the workspace root. Gated behind the same
    /// accept/reject UI as file proposals via `CommandProposal`.
    Bash {
        command: String,
        timeout_secs: Option<u64>,
    },
    /// Semantic search across the workspace using Ollama embeddings.
    /// Returns the top-K most similar code chunks.
    SemanticSearch {
        query: String,
        top_k: Option<usize>,
    },
}

impl ToolCall {
    /// Path the call mutates (or `None` for read-only tools).
    pub fn mutating_path(&self) -> Option<&str> {
        match self {
            ToolCall::Edit { path, .. }
            | ToolCall::Create { path, .. }
            | ToolCall::Delete { path } => Some(path),
            _ => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            ToolCall::ReadFile {
                path,
                start_line,
                end_line,
            } => match (start_line, end_line) {
                (Some(s), Some(e)) => format!("read_file({path}:{s}-{e})"),
                (Some(s), None) => format!("read_file({path}:{s}-)"),
                _ => format!("read_file({path})"),
            },
            ToolCall::ListDir { path } => format!(
                "list_dir({})",
                if path.is_empty() { "." } else { path.as_str() }
            ),
            ToolCall::Grep { pattern, path } => match path {
                Some(p) => format!("grep(pattern={pattern:?}, path={p:?})"),
                None => format!("grep(pattern={pattern:?})"),
            },
            ToolCall::Glob { pattern } => format!("glob({pattern:?})"),
            ToolCall::Edit { path, .. } => format!("edit({path})"),
            ToolCall::Create { path, .. } => format!("create({path})"),
            ToolCall::Delete { path } => format!("delete({path})"),
            ToolCall::Finish { .. } => "finish".to_string(),
            ToolCall::Bash { command, .. } => {
                let preview: String = command.chars().take(60).collect();
                format!("bash({preview})")
            }
            ToolCall::SemanticSearch { query, top_k } => match top_k {
                Some(k) => format!("semantic_search({query:?}, top_k={k})"),
                None => format!("semantic_search({query:?})"),
            },
        }
    }
}

/// Outcome of parsing one model turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolAction {
    /// A regular tool call.
    Call { thought: String, call: ToolCall },
    /// The model returned the legacy `{operations, summary}` envelope. Treat
    /// it as an immediate finish carrying those operations verbatim — keeps
    /// older models / fixtures (and the integration test) working.
    LegacyOperations { result: CodeWriterResult },
    /// The model returned text we could not turn into a tool call. The string
    /// is fed back to the model so it can correct itself on the next turn.
    ParseError(String),
}

pub fn parse_tool_action(raw: &str) -> ToolAction {
    let trimmed = strip_code_fence(raw.trim());
    let body = match extract_first_json_object(&trimmed) {
        Some(b) => b.to_string(),
        None => {
            return ToolAction::ParseError(
                "Could not find a JSON object in your response. Reply with one JSON object only, e.g. {\"tool\":\"read_file\",\"args\":{\"path\":\"src/main.rs\"}}.".to_string(),
            );
        }
    };
    let value: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(err) => {
            let err_s = err.to_string();
            let json_shape_hint =
                body.contains("\" in ") || body.contains(" in .\"") || err_s.contains("expected `:`");
            let suffix = if json_shape_hint {
                " Common mistake: do NOT mimic `grep(\"needle\" in folder)` prose. grep args are ONLY `pattern` and optional `path` string fields, e.g. {\"tool\":\"grep\",\"args\":{\"pattern\":\"stylesheet\",\"path\":\"index.html\"}}."
            } else {
                ""
            };
            return ToolAction::ParseError(format!(
                "Invalid JSON ({err}).{suffix} Return one JSON object — no markdown fences or extra prose surrounding it."
            ));
        }
    };

    if value.get("operations").and_then(|v| v.as_array()).is_some() {
        if let Ok(result) = serde_json::from_value::<CodeWriterResult>(value.clone()) {
            return ToolAction::LegacyOperations { result };
        }
    }

    let thought = value
        .get("thought")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let tool_name = ["tool", "name", "action", "function"]
        .iter()
        .find_map(|k| value.get(*k).and_then(|v| v.as_str()))
        .unwrap_or("")
        .trim()
        .to_lowercase();

    if tool_name.is_empty() {
        return ToolAction::ParseError(
            "Missing `tool` field. Use one of: read_file, list_dir, grep, glob, edit, create, delete, bash, semantic_search, finish.".to_string(),
        );
    }

    let args_obj = value.get("args").cloned();
    let str_get = |keys: &[&str]| -> Option<String> {
        if let Some(a) = &args_obj {
            if let Some(s) = str_arg(a, keys) {
                return Some(s);
            }
        }
        str_arg(&value, keys)
    };
    let usize_get = |keys: &[&str]| -> Option<usize> {
        if let Some(a) = &args_obj {
            if let Some(n) = usize_arg(a, keys) {
                return Some(n);
            }
        }
        usize_arg(&value, keys)
    };
    let bool_get = |keys: &[&str]| -> Option<bool> {
        if let Some(a) = &args_obj {
            if let Some(b) = bool_arg(a, keys) {
                return Some(b);
            }
        }
        bool_arg(&value, keys)
    };

    match tool_name.as_str() {
        "read_file" | "read" | "open" | "view" | "cat" | "show" => {
            let path = match str_get(&["path", "file", "filename"]) {
                Some(p) => p,
                None => return missing("read_file", "path"),
            };
            ToolAction::Call {
                thought,
                call: ToolCall::ReadFile {
                    path,
                    start_line: usize_get(&["start_line", "start", "from", "line_start", "line"]),
                    end_line: usize_get(&["end_line", "end", "to", "line_end"]),
                },
            }
        }
        "list_dir" | "ls" | "list" | "dir" | "tree" | "list_directory" => {
            let path = str_get(&["path", "directory", "dir"]).unwrap_or_default();
            ToolAction::Call {
                thought,
                call: ToolCall::ListDir { path },
            }
        }
        "grep" | "search" | "search_text" | "rg" | "find_text" => {
            let pattern = match str_get(&["pattern", "query", "needle", "text", "regex"]) {
                Some(p) => p,
                None => return missing("grep", "pattern"),
            };
            ToolAction::Call {
                thought,
                call: ToolCall::Grep {
                    pattern,
                    // Deliberately avoid treating `in` as a path key — models imitate
                    // `grep("x" in file)` prose and confuse JSON + routing.
                    path: str_get(&["path", "scope", "directory", "file"]),
                },
            }
        }
        "glob" | "find" | "find_files" | "files" => {
            let pattern = match str_get(&["pattern", "query", "glob"]) {
                Some(p) => p,
                None => return missing("glob", "pattern"),
            };
            ToolAction::Call {
                thought,
                call: ToolCall::Glob { pattern },
            }
        }
        "edit" | "patch" | "search_replace" | "replace" | "modify" => {
            let path = match str_get(&["path", "file", "filename"]) {
                Some(p) => p,
                None => return missing("edit", "path"),
            };
            let search = match str_get(&["search", "old_str", "old", "find", "from"]) {
                Some(s) => s,
                None => return missing("edit", "search"),
            };
            if search.trim().is_empty() {
                return ToolAction::ParseError(
                    "Tool `edit` requires a NON-EMPTY `search`. read_file first, then copy contiguous lines verbatim from disk into args.search.".to_string(),
                );
            }
            let replace = str_get(&["replace", "new_str", "new", "to", "with"]).unwrap_or_default();
            let replace_all = bool_get(&["replace_all", "all", "global"]).unwrap_or(false);
            ToolAction::Call {
                thought,
                call: ToolCall::Edit {
                    path,
                    search,
                    replace,
                    replace_all,
                },
            }
        }
        "create" | "create_file" | "write" | "write_file" | "new_file" | "add_file" => {
            let path = match str_get(&["path", "file", "filename"]) {
                Some(p) => p,
                None => return missing("create", "path"),
            };
            let content = str_get(&["content", "body", "text", "data"]).unwrap_or_default();
            ToolAction::Call {
                thought,
                call: ToolCall::Create { path, content },
            }
        }
        "delete" | "delete_file" | "remove" | "rm" | "unlink" => {
            let path = match str_get(&["path", "file", "filename"]) {
                Some(p) => p,
                None => return missing("delete", "path"),
            };
            ToolAction::Call {
                thought,
                call: ToolCall::Delete { path },
            }
        }
        "finish" | "done" | "stop" | "complete" | "end" | "submit" => {
            let summary = str_get(&["summary", "message", "result", "answer"]).unwrap_or_default();
            ToolAction::Call {
                thought,
                call: ToolCall::Finish { summary },
            }
        }
        "bash" | "shell" | "run" | "exec" | "execute" | "run_command" => {
            let command = match str_get(&["command", "cmd", "script"]) {
                Some(c) => c,
                None => return missing("bash", "command"),
            };
            let timeout_secs = ["timeout", "timeout_secs", "timeout_seconds"]
                .iter()
                .find_map(|k| {
                    args_obj
                        .as_ref()
                        .and_then(|a| a.get(*k))
                        .or_else(|| value.get(*k))
                        .and_then(|v| v.as_u64())
                });
            ToolAction::Call {
                thought,
                call: ToolCall::Bash {
                    command,
                    timeout_secs,
                },
            }
        }
        "semantic_search" | "search_semantic" | "embed_search" | "vector_search" => {
            let query = match str_get(&["query", "pattern", "text"]) {
                Some(q) => q,
                None => return missing("semantic_search", "query"),
            };
            let top_k = ["top_k", "k", "limit"]
                .iter()
                .find_map(|k| {
                    args_obj
                        .as_ref()
                        .and_then(|a| a.get(*k))
                        .or_else(|| value.get(*k))
                        .and_then(|v| v.as_u64())
                })
                .map(|n| n as usize);
            ToolAction::Call {
                thought,
                call: ToolCall::SemanticSearch { query, top_k },
            }
        }
        other => ToolAction::ParseError(format!(
            "Unknown tool `{other}`. Use one of: read_file, list_dir, grep, glob, edit, create, delete, bash, semantic_search, finish."
        )),
    }
}

fn missing(tool: &str, arg: &str) -> ToolAction {
    ToolAction::ParseError(format!("Tool `{tool}` requires an `{arg}` argument."))
}

fn str_arg(value: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = value.get(*k).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

fn usize_arg(value: &Value, keys: &[&str]) -> Option<usize> {
    for k in keys {
        if let Some(n) = value.get(*k).and_then(|v| v.as_u64()) {
            return Some(n as usize);
        }
    }
    None
}

fn bool_arg(value: &Value, keys: &[&str]) -> Option<bool> {
    for k in keys {
        if let Some(b) = value.get(*k).and_then(|v| v.as_bool()) {
            return Some(b);
        }
    }
    None
}

fn strip_code_fence(raw: &str) -> String {
    let t = raw.trim();
    if let Some(rest) = t.strip_prefix("```json") {
        return rest.trim_end_matches("```").trim().to_string();
    }
    if let Some(rest) = t.strip_prefix("```") {
        return rest.trim_end_matches("```").trim().to_string();
    }
    t.to_string()
}

/// Find the first balanced `{...}` JSON object inside `raw`. Tracks string
/// boundaries so braces inside string literals don't confuse the scanner.
fn extract_first_json_object(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let start = raw.find('{')?;
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if escaped {
            escaped = false;
            continue;
        }
        if in_str {
            if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    raw.rfind('}').map(|end| &raw[start..=end])
}

// ===== Path safety =====

/// Normalise a model-supplied path against the workspace root. Returns the
/// canonical relative key (forward slashes, no leading `./`) plus the
/// absolute path if a root is configured.
///
/// Local models routinely emit absolute paths even when the prompt asks for
/// workspace-relative ones. We accept absolute paths that fall under the
/// workspace root by stripping the prefix; absolute paths outside the
/// workspace are rejected with a message that names the workspace so the
/// model can self-correct on its next turn. `..` traversal is always
/// rejected.
pub fn safe_relative_path(root: Option<&Path>, rel: &str) -> Result<(String, Option<PathBuf>)> {
    let normalized = rel.trim().replace('\\', "/");
    if normalized.is_empty() {
        bail!("path is empty");
    }

    // Absolute paths: strip the workspace prefix when applicable.
    if normalized.starts_with('/') || is_windows_absolute(&normalized) {
        match root {
            Some(workspace) => {
                let ws = workspace.to_string_lossy().replace('\\', "/");
                let ws = ws.trim_end_matches('/');
                if normalized == ws {
                    // The workspace root itself — return an empty key that
                    // callers like `list_dir` interpret as "the workspace".
                    return Ok((String::new(), Some(workspace.to_path_buf())));
                }
                let prefix = format!("{ws}/");
                if let Some(rest) = normalized.strip_prefix(&prefix) {
                    return safe_relative_path(root, rest);
                }
                bail!(
                    "absolute path `{rel}` is outside the workspace root `{}`. Pass a path relative to the workspace (e.g. `src/main.rs`).",
                    workspace.display()
                );
            }
            None => bail!(
                "absolute path `{rel}` cannot be resolved — no workspace root is set. Open a folder in the editor or pass --workspace."
            ),
        }
    }

    let stripped = normalized.trim_start_matches("./");
    if stripped.is_empty() {
        bail!("path is empty");
    }
    let parts: Vec<&str> = stripped
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.contains(&"..") {
        bail!("path traversal not allowed: {rel}");
    }
    let key = parts.join("/");
    if key.is_empty() {
        bail!("path is empty");
    }
    let abs = root.map(|r| r.join(&key));
    Ok((key, abs))
}

fn is_windows_absolute(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Like `safe_relative_path`, but errors when the path resolves to the
/// workspace root itself. Use this for read_file / edit / create / delete;
/// list_dir can still accept the workspace root.
pub fn safe_relative_file_path(
    root: Option<&Path>,
    rel: &str,
) -> Result<(String, Option<PathBuf>)> {
    let (key, abs) = safe_relative_path(root, rel)?;
    if key.is_empty() {
        bail!(
            "`{rel}` is the workspace root itself — pass a file path inside the workspace (e.g. `src/main.rs`)"
        );
    }
    Ok((key, abs))
}

// ===== Session / virtual filesystem =====

#[derive(Debug, Clone, PartialEq, Eq)]
enum StagedAction {
    Untracked,
    Edit,
    Create,
    Delete,
}

#[derive(Debug, Clone)]
struct FileSlot {
    /// Current content of this path within the session. `None` means the
    /// path is staged for deletion.
    content: Option<String>,
    /// Original content as observed on disk the first time this slot was
    /// touched, captured so a rejected proposal can revert the slot to
    /// its pre-mutation state without re-reading from disk.
    original_content: Option<String>,
    /// Did the file exist on disk when we first observed it? Determines
    /// whether the final operation should be `Create` or `Edit`.
    initial_existed: bool,
    /// What the model has done to this path so far.
    action: StagedAction,
}

/// In-memory overlay over the workspace filesystem. Tools record their
/// reads/writes here so subsequent calls within the same loop see a
/// consistent view of pending changes.
pub struct ToolSession {
    workspace_root: Option<PathBuf>,
    files: BTreeMap<String, FileSlot>,
}

impl ToolSession {
    pub fn new(workspace_root: Option<PathBuf>) -> Self {
        Self {
            workspace_root,
            files: BTreeMap::new(),
        }
    }

    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    pub fn has_staged(&self) -> bool {
        self.files
            .values()
            .any(|f| !matches!(f.action, StagedAction::Untracked))
    }

    /// Build the wire-shape `FileOperation` for whatever is currently
    /// staged at `key`, or `None` if the path is not staged.
    pub fn staged_op_for(&self, key: &str) -> Option<FileOperation> {
        let slot = self.files.get(key)?;
        match slot.action {
            StagedAction::Untracked => None,
            StagedAction::Delete => Some(FileOperation::delete(key.to_string())),
            StagedAction::Create => slot
                .content
                .as_ref()
                .map(|c| FileOperation::create(key.to_string(), c.clone())),
            StagedAction::Edit => slot
                .content
                .as_ref()
                .map(|c| FileOperation::edit(key.to_string(), c.clone())),
        }
    }

    /// Roll a path back to the state it was in before the most recent
    /// mutation. Used after a rejected proposal so the rest of the loop
    /// continues against an accurate view of the workspace.
    pub fn revert(&mut self, key: &str) {
        let restore = self
            .files
            .get(key)
            .map(|slot| (slot.initial_existed, slot.original_content.clone()));
        match restore {
            Some((true, original)) => {
                if let Some(slot) = self.files.get_mut(key) {
                    slot.content = original;
                    slot.action = StagedAction::Untracked;
                }
            }
            Some((false, _)) => {
                self.files.remove(key);
            }
            None => {}
        }
    }

    /// Convert the staged state into a wire-shaped `CodeWriterResult`.
    pub fn final_operations(&self, summary: String) -> CodeWriterResult {
        let mut ops = Vec::new();
        for (path, slot) in &self.files {
            match slot.action {
                StagedAction::Untracked => {}
                StagedAction::Delete => ops.push(FileOperation::delete(path.clone())),
                StagedAction::Create => {
                    if let Some(c) = &slot.content {
                        ops.push(FileOperation::create(path.clone(), c.clone()));
                    }
                }
                StagedAction::Edit => {
                    if let Some(c) = &slot.content {
                        ops.push(FileOperation::edit(path.clone(), c.clone()));
                    }
                }
            }
        }
        CodeWriterResult {
            operations: ops,
            summary,
            rejected_paths: Vec::new(),
            already_decided: false,
        }
    }
}

// ===== Tool execution =====

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    /// Free-form text fed back to the model on the next turn.
    pub feedback: String,
    /// True when the call mutated the session's virtual filesystem.
    pub mutated: bool,
}

pub async fn execute(session: &mut ToolSession, call: &ToolCall) -> ToolOutcome {
    match call {
        ToolCall::ReadFile {
            path,
            start_line,
            end_line,
        } => execute_read_file(session, path, *start_line, *end_line).await,
        ToolCall::ListDir { path } => execute_list_dir(session, path).await,
        ToolCall::Grep { pattern, path } => execute_grep(session, pattern, path.as_deref()).await,
        ToolCall::Glob { pattern } => execute_glob(session, pattern).await,
        ToolCall::Edit {
            path,
            search,
            replace,
            replace_all,
        } => execute_edit(session, path, search, replace, *replace_all).await,
        ToolCall::Create { path, content } => execute_create(session, path, content),
        ToolCall::Delete { path } => execute_delete(session, path),
        ToolCall::Finish { .. } => ToolOutcome {
            feedback: "(finish recorded)".to_string(),
            mutated: false,
        },
        ToolCall::Bash { .. } | ToolCall::SemanticSearch { .. } => ToolOutcome {
            feedback:
                "internal: this tool is dispatched by the agent loop, not the in-process executor"
                    .into(),
            mutated: false,
        },
    }
}

async fn read_through(session: &mut ToolSession, path: &str) -> Result<String> {
    let (key, abs) = safe_relative_file_path(session.workspace_root.as_deref(), path)?;
    if let Some(slot) = session.files.get(&key) {
        return slot
            .content
            .clone()
            .ok_or_else(|| anyhow!("`{key}` is staged for deletion"));
    }
    let abs = abs.ok_or_else(|| anyhow!("no workspace root configured"))?;
    let metadata = fs::metadata(&abs)
        .await
        .map_err(|err| anyhow!("file not found: {key} ({err})"))?;
    if !metadata.is_file() {
        bail!("`{key}` is not a regular file");
    }
    if metadata.len() > MAX_READ_BYTES {
        bail!(
            "`{key}` is {} bytes — exceeds the {MAX_READ_BYTES} byte read cap",
            metadata.len()
        );
    }
    let content = fs::read_to_string(&abs)
        .await
        .map_err(|err| anyhow!("read failed: {err}"))?;
    session.files.insert(
        key.clone(),
        FileSlot {
            content: Some(content.clone()),
            original_content: Some(content.clone()),
            initial_existed: true,
            action: StagedAction::Untracked,
        },
    );
    Ok(content)
}

async fn execute_read_file(
    session: &mut ToolSession,
    path: &str,
    start: Option<usize>,
    end: Option<usize>,
) -> ToolOutcome {
    let content = match read_through(session, path).await {
        Ok(c) => c,
        Err(err) => {
            return ToolOutcome {
                feedback: format!("error reading {path}: {err}"),
                mutated: false,
            };
        }
    };
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total = lines.len();
    let s = start.map(|n| n.saturating_sub(1)).unwrap_or(0);
    let e = end.map(|n| n.min(total)).unwrap_or(total);
    if s >= total {
        return ToolOutcome {
            feedback: format!("(empty range; {path} has {total} line(s))"),
            mutated: false,
        };
    }
    let last = e.max(s + 1).min(total);
    let mut numbered = String::new();
    numbered.push_str(&format!(
        "--- {path} (lines {}-{} of {total}) ---\n",
        s + 1,
        last
    ));
    for (i, line) in lines[s..last].iter().enumerate() {
        let stripped = line.strip_suffix('\n').unwrap_or(line);
        numbered.push_str(&format!("{:>5}  {}\n", s + i + 1, stripped));
    }
    ToolOutcome {
        feedback: numbered,
        mutated: false,
    }
}

async fn execute_list_dir(session: &mut ToolSession, path: &str) -> ToolOutcome {
    let root = match session.workspace_root.as_deref() {
        Some(r) => r,
        None => {
            return ToolOutcome {
                feedback: "error: no workspace root configured for list_dir".into(),
                mutated: false,
            }
        }
    };
    let target = if path.trim().is_empty() {
        root.to_path_buf()
    } else {
        match safe_relative_path(Some(root), path) {
            Ok((_, Some(abs))) => abs,
            Ok((_, None)) => root.to_path_buf(),
            Err(err) => {
                return ToolOutcome {
                    feedback: format!("error: {err}"),
                    mutated: false,
                };
            }
        }
    };
    let mut reader = match fs::read_dir(&target).await {
        Ok(r) => r,
        Err(err) => {
            return ToolOutcome {
                feedback: format!("error reading directory {path}: {err}"),
                mutated: false,
            };
        }
    };
    let mut entries: Vec<(String, bool)> = Vec::new();
    let mut hidden_count = 0usize;
    let mut skipped_count = 0usize;
    while let Ok(Some(entry)) = reader.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            hidden_count += 1;
            continue;
        }
        if WALK_SKIP.iter().any(|s| name == *s) {
            skipped_count += 1;
            continue;
        }
        let is_dir = entry.file_type().await.map(|f| f.is_dir()).unwrap_or(false);
        entries.push((name, is_dir));
        if entries.len() >= MAX_DIR_ENTRIES {
            break;
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let display = if path.trim().is_empty() { "." } else { path };
    let absolute = target.display();
    let mut out = format!(
        "--- {display} → {absolute} ({} entry/entries) ---\n",
        entries.len()
    );
    if entries.is_empty() {
        let hidden_note = if hidden_count + skipped_count > 0 {
            format!(
                " ({hidden_count} hidden / {skipped_count} skipped — build artefacts and dotfiles are filtered)"
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "(directory is empty{hidden_note}.)\n\n\
This is a fresh / empty workspace. If the user asked you to BUILD something here, \
do NOT keep listing — go straight to `create{{path, content}}` calls (e.g. `index.html`, \
`styles.css`, `script.js` for a web project; `src/main.rs` + `Cargo.toml` for Rust; etc.). \
After you create files, future read_file/list_dir calls will see them.\n"
        ));
    } else {
        for (name, is_dir) in entries {
            out.push_str(&format!("{name}{}\n", if is_dir { "/" } else { "" }));
        }
    }
    ToolOutcome {
        feedback: out,
        mutated: false,
    }
}

fn grep_substring_syntax_note(pattern: &str) -> Option<String> {
    if pattern.chars().any(|c| {
        matches!(
            c,
            '^' | '$' | '[' | ']' | '(' | ')' | '*' | '+' | '?' | '|' | '\\' | '{' | '}'
        )
    }) {
        Some(
            "NOTE: `grep` is case-insensitive SUBSTRING matching only (not regex). \
Metacharacters are searched literally; use short needles (`class=\"`, `<a `, `.btn`).\n\
"
            .into(),
        )
    } else {
        None
    }
}

async fn execute_grep(
    session: &mut ToolSession,
    pattern: &str,
    scope: Option<&str>,
) -> ToolOutcome {
    let root = match session.workspace_root.as_deref() {
        Some(r) => r,
        None => {
            return ToolOutcome {
                feedback: "error: no workspace root configured for grep".into(),
                mutated: false,
            }
        }
    };
    let needle = pattern.to_lowercase();
    let meta_note = grep_substring_syntax_note(pattern);
    if needle.is_empty() {
        return ToolOutcome {
            feedback: "error: pattern is empty".into(),
            mutated: false,
        };
    }

    let candidate_files: Vec<String> = match scope {
        Some(s) if !s.trim().is_empty() => vec![s.trim().to_string()],
        _ => walk_workspace(root, MAX_GREP_FILES).await,
    };

    let mut hits = Vec::new();
    'outer: for rel in candidate_files {
        let abs = root.join(&rel);
        let Ok(meta) = fs::metadata(&abs).await else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_GREP_BYTES {
            continue;
        }
        let Ok(content) = fs::read_to_string(&abs).await else {
            continue;
        };
        for (i, line) in content.lines().enumerate() {
            if line.to_lowercase().contains(&needle) {
                let snippet: String = line.trim().chars().take(200).collect();
                hits.push(format!("{}:{}: {}", rel, i + 1, snippet));
                if hits.len() >= MAX_GREP_HITS {
                    break 'outer;
                }
            }
        }
    }
    let body = if hits.is_empty() {
        format!("(no matches for `{pattern}`)")
    } else {
        hits.join("\n")
    };
    let note = meta_note.unwrap_or_default();
    ToolOutcome {
        feedback: format!(
            "--- grep `{pattern}` ({} hit(s)) ---\n{note}{body}",
            hits.len()
        ),
        mutated: false,
    }
}

async fn execute_glob(session: &mut ToolSession, pattern: &str) -> ToolOutcome {
    let root = match session.workspace_root.as_deref() {
        Some(r) => r,
        None => {
            return ToolOutcome {
                feedback: "error: no workspace root configured for glob".into(),
                mutated: false,
            }
        }
    };
    // Local models often pass several patterns as a comma- or
    // semicolon-separated string (e.g. `**/*.html, **/*.css`). Accept that
    // and treat the result as a union.
    let patterns: Vec<String> = pattern
        .split([',', ';', '|'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let patterns = if patterns.is_empty() {
        vec![pattern.trim().to_string()]
    } else {
        patterns
    };
    let entries = walk_workspace(root, MAX_GREP_FILES).await;
    let matches: Vec<String> = entries
        .into_iter()
        .filter(|p| patterns.iter().any(|pat| glob_match(pat, p)))
        .take(MAX_GLOB_RESULTS)
        .collect();
    let body = if matches.is_empty() {
        format!("(no paths match `{pattern}`)")
    } else {
        matches.join("\n")
    };
    ToolOutcome {
        feedback: format!(
            "--- glob `{pattern}` ({} match(es)) ---\n{body}",
            matches.len()
        ),
        mutated: false,
    }
}

async fn execute_edit(
    session: &mut ToolSession,
    path: &str,
    search: &str,
    replace: &str,
    replace_all: bool,
) -> ToolOutcome {
    let original = match read_through(session, path).await {
        Ok(c) => c,
        Err(err) => {
            return ToolOutcome {
                feedback: format!("error opening `{path}` for edit: {err}"),
                mutated: false,
            };
        }
    };
    let updated = match apply_search_replace(&original, search, replace, replace_all) {
        Ok(u) => u,
        Err(err) => {
            return ToolOutcome {
                feedback: format!("edit failed in `{path}`: {err}"),
                mutated: false,
            };
        }
    };
    let (key, _) = match safe_relative_file_path(session.workspace_root.as_deref(), path) {
        Ok(p) => p,
        Err(err) => {
            return ToolOutcome {
                feedback: format!("error: {err}"),
                mutated: false,
            };
        }
    };
    let prior_slot = session.files.get(&key).cloned();
    let initial_existed = prior_slot.as_ref().map(|s| s.initial_existed).unwrap_or(true);
    let original_content = prior_slot
        .as_ref()
        .and_then(|s| s.original_content.clone())
        .or_else(|| Some(original.clone()));
    let prior_action = prior_slot
        .map(|s| s.action)
        .unwrap_or(StagedAction::Untracked);
    let action = match (initial_existed, prior_action) {
        (false, _) => StagedAction::Create,
        (true, StagedAction::Create) => StagedAction::Create,
        _ => StagedAction::Edit,
    };
    let before_lines = original.lines().count();
    let after_lines = updated.lines().count();
    let bytes_delta = updated.len() as i64 - original.len() as i64;
    session.files.insert(
        key.clone(),
        FileSlot {
            content: Some(updated),
            original_content,
            initial_existed,
            action,
        },
    );
    ToolOutcome {
        feedback: format!(
            "edit staged for `{key}` ({} → {} lines, {:+} bytes)",
            before_lines, after_lines, bytes_delta
        ),
        mutated: true,
    }
}

fn execute_create(session: &mut ToolSession, path: &str, content: &str) -> ToolOutcome {
    let (key, _) = match safe_relative_file_path(session.workspace_root.as_deref(), path) {
        Ok(p) => p,
        Err(err) => {
            return ToolOutcome {
                feedback: format!("error: {err}"),
                mutated: false,
            };
        }
    };
    let prior = session.files.get(&key).cloned();
    let initial_existed = prior.as_ref().map(|s| s.initial_existed).unwrap_or(false);
    
    if initial_existed {
        return ToolOutcome {
            feedback: format!("error: `{key}` already exists. You MUST use `read_file` to read its contents and then use `edit` to modify it. Do not overwrite files with `create`."),
            mutated: false,
        };
    }
    
    let original_content = prior.as_ref().and_then(|s| s.original_content.clone());
    let action = StagedAction::Create;
    let bytes = content.len();
    session.files.insert(
        key.clone(),
        FileSlot {
            content: Some(content.to_string()),
            original_content,
            initial_existed,
            action,
        },
    );
    ToolOutcome {
        feedback: format!(
            "{} staged for `{key}` ({bytes} byte(s))",
            if initial_existed { "edit" } else { "create" }
        ),
        mutated: true,
    }
}

fn execute_delete(session: &mut ToolSession, path: &str) -> ToolOutcome {
    let (key, _) = match safe_relative_file_path(session.workspace_root.as_deref(), path) {
        Ok(p) => p,
        Err(err) => {
            return ToolOutcome {
                feedback: format!("error: {err}"),
                mutated: false,
            };
        }
    };
    let prior = session.files.get(&key).cloned();
    let initial_existed = prior.as_ref().map(|s| s.initial_existed).unwrap_or(true);
    let original_content = prior.as_ref().and_then(|s| s.original_content.clone());
    session.files.insert(
        key.clone(),
        FileSlot {
            content: None,
            original_content,
            initial_existed,
            action: StagedAction::Delete,
        },
    );
    ToolOutcome {
        feedback: format!("delete staged for `{key}`"),
        mutated: true,
    }
}

/// Apply a search/replace pair the way Claude Code's Edit tool does:
/// the search string must match VERBATIM and (unless `replace_all`) appear
/// exactly once in the file. This is a much friendlier representation for
/// local models than full unified diffs.
pub fn apply_search_replace(
    content: &str,
    search: &str,
    replace: &str,
    replace_all: bool,
) -> Result<String> {
    if search.is_empty() {
        bail!("search string is empty");
    }
    let count = content.matches(search).count();
    if count == 0 {
        bail!("search string not found (be exact, including whitespace and indentation)");
    }
    if !replace_all && count > 1 {
        bail!(
            "search string is not unique ({count} matches) — provide more context or set replace_all=true"
        );
    }
    if replace_all {
        Ok(content.replace(search, replace))
    } else {
        Ok(content.replacen(search, replace, 1))
    }
}

// ===== Glob matching =====

pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p_segs: Vec<&str> = pattern.split('/').collect();
    let s_segs: Vec<&str> = path.split('/').collect();
    glob_segments(&p_segs, &s_segs)
}

fn glob_segments(p: &[&str], s: &[&str]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(&"**"), _) => {
            if glob_segments(&p[1..], s) {
                return true;
            }
            if !s.is_empty() && glob_segments(p, &s[1..]) {
                return true;
            }
            false
        }
        (Some(_), None) => false,
        (Some(pp), Some(ss)) => {
            if glob_segment_match(pp, ss) {
                glob_segments(&p[1..], &s[1..])
            } else {
                false
            }
        }
    }
}

fn glob_segment_match(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let nam: Vec<char> = name.chars().collect();
    fn rec(p: &[char], s: &[char]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(&'*'), _) => {
                if rec(&p[1..], s) {
                    return true;
                }
                if !s.is_empty() {
                    return rec(p, &s[1..]);
                }
                false
            }
            (None, _) => false,
            (Some(_), None) => false,
            (Some(&'?'), Some(_)) => rec(&p[1..], &s[1..]),
            (Some(pp), Some(ss)) => pp == ss && rec(&p[1..], &s[1..]),
        }
    }
    rec(&pat, &nam)
}

// ===== Workspace walking =====

async fn walk_workspace(root: &Path, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= max {
            break;
        }
        let mut reader = match fs::read_dir(&dir).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = reader.next_entry().await {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if WALK_SKIP.iter().any(|s| name == *s) {
                continue;
            }
            let Ok(ft) = entry.file_type().await else {
                continue;
            };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    let rel_s = rel.to_string_lossy().replace('\\', "/");
                    if !rel_s.is_empty() {
                        out.push(rel_s);
                        if out.len() >= max {
                            break;
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_read_file_with_args_block() {
        let raw = r#"{"thought":"open main","tool":"read_file","args":{"path":"src/main.rs"}}"#;
        match parse_tool_action(raw) {
            ToolAction::Call {
                thought,
                call: ToolCall::ReadFile { path, .. },
            } => {
                assert_eq!(thought, "open main");
                assert_eq!(path, "src/main.rs");
            }
            other => panic!("expected ReadFile, got {other:?}"),
        }
    }

    #[test]
    fn parse_read_file_with_flat_path() {
        let raw = r#"{"tool":"read","path":"src/lib.rs","start_line":10,"end_line":20}"#;
        match parse_tool_action(raw) {
            ToolAction::Call {
                call:
                    ToolCall::ReadFile {
                        path,
                        start_line,
                        end_line,
                    },
                ..
            } => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(start_line, Some(10));
                assert_eq!(end_line, Some(20));
            }
            other => panic!("expected ReadFile, got {other:?}"),
        }
    }

    #[test]
    fn parse_handles_code_fence() {
        let raw = "```json\n{\"tool\":\"finish\",\"args\":{\"summary\":\"all done\"}}\n```";
        match parse_tool_action(raw) {
            ToolAction::Call {
                call: ToolCall::Finish { summary },
                ..
            } => assert_eq!(summary, "all done"),
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[test]
    fn parse_legacy_operations_envelope_falls_through_as_finish() {
        let raw = r#"{"operations":[{"action":"create","file":"a.rs","content":"fn x(){}"}],"summary":"x"}"#;
        match parse_tool_action(raw) {
            ToolAction::LegacyOperations { result } => {
                assert_eq!(result.operations.len(), 1);
                assert_eq!(result.operations[0].file, "a.rs");
                assert_eq!(result.summary, "x");
            }
            other => panic!("expected LegacyOperations, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_tool_returns_helpful_error() {
        match parse_tool_action(r#"{"tool":"explode","args":{}}"#) {
            ToolAction::ParseError(msg) => {
                assert!(msg.contains("Unknown tool"), "msg was: {msg}");
                assert!(msg.contains("read_file"));
            }
            other => panic!("expected ParseError, got {other:?}"),
        }
    }

    #[test]
    fn parse_extracts_first_balanced_object_from_prose() {
        let raw = "Sure! Here you go:\n{\"tool\":\"list_dir\",\"path\":\"src\"}\nThanks";
        match parse_tool_action(raw) {
            ToolAction::Call {
                call: ToolCall::ListDir { path },
                ..
            } => assert_eq!(path, "src"),
            other => panic!("expected ListDir, got {other:?}"),
        }
    }

    #[test]
    fn parse_edit_rejects_empty_search() {
        match parse_tool_action(
            r#"{"tool":"edit","args":{"path":"a.css","search":"","replace":"x"}}"#,
        ) {
            ToolAction::ParseError(msg) => {
                assert!(msg.contains("NON-EMPTY"), "msg={msg:?}");
                assert!(msg.contains("search"), "msg={msg:?}");
            }
            other => panic!("expected ParseError, got {other:?}"),
        }
    }

    #[test]
    fn parse_missing_required_arg_is_error() {
        match parse_tool_action(r#"{"tool":"edit","args":{"path":"a.rs"}}"#) {
            ToolAction::ParseError(msg) => assert!(msg.contains("`search`"), "msg was: {msg}"),
            other => panic!("expected ParseError, got {other:?}"),
        }
    }

    #[test]
    fn apply_replaces_unique_match() {
        assert_eq!(
            apply_search_replace("hello world\n", "hello", "hi", false).unwrap(),
            "hi world\n"
        );
    }

    #[test]
    fn apply_rejects_non_unique_search_without_replace_all() {
        let err = apply_search_replace("ab\nab\n", "ab", "x", false).unwrap_err();
        assert!(err.to_string().contains("not unique"));
    }

    #[test]
    fn apply_replaces_all_when_requested() {
        assert_eq!(
            apply_search_replace("ab\nab\n", "ab", "x", true).unwrap(),
            "x\nx\n"
        );
    }

    #[test]
    fn apply_errors_when_search_missing() {
        assert!(apply_search_replace("foo", "nope", "x", false).is_err());
    }

    #[test]
    fn safe_relative_rejects_traversal() {
        assert!(safe_relative_path(Some(Path::new("/tmp")), "../etc/passwd").is_err());
        assert!(safe_relative_path(Some(Path::new("/tmp")), "a/../b").is_err());
    }

    #[test]
    fn safe_relative_normalizes() {
        let (key, _) = safe_relative_path(Some(Path::new("/tmp")), "./src//main.rs").unwrap();
        assert_eq!(key, "src/main.rs");
    }

    #[test]
    fn safe_relative_accepts_absolute_path_under_workspace() {
        let (key, abs) = safe_relative_path(
            Some(Path::new("/Users/me/project")),
            "/Users/me/project/src/main.rs",
        )
        .unwrap();
        assert_eq!(key, "src/main.rs");
        assert_eq!(abs, Some(PathBuf::from("/Users/me/project/src/main.rs")));
    }

    #[test]
    fn safe_relative_rejects_absolute_outside_workspace() {
        let err = safe_relative_path(
            Some(Path::new("/Users/me/project")),
            "/Users/me/Desktop/other.txt",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("outside the workspace root"), "msg was: {err}");
        assert!(err.contains("/Users/me/project"));
    }

    #[test]
    fn safe_relative_rejects_absolute_when_no_workspace() {
        let err = safe_relative_path(None, "/etc/passwd")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no workspace root"), "msg was: {err}");
    }

    #[test]
    fn safe_relative_returns_empty_for_workspace_root_path() {
        let (key, abs) = safe_relative_path(
            Some(Path::new("/Users/me/project")),
            "/Users/me/project",
        )
        .unwrap();
        assert!(key.is_empty(), "expected empty key for workspace-root path");
        assert_eq!(abs, Some(PathBuf::from("/Users/me/project")));
    }

    #[test]
    fn safe_relative_file_rejects_workspace_root_path() {
        let err = safe_relative_file_path(
            Some(Path::new("/Users/me/project")),
            "/Users/me/project",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("workspace root itself"),
            "msg was: {err}"
        );
    }

    #[test]
    fn safe_relative_handles_trailing_slash_on_workspace() {
        let (key, _) = safe_relative_path(
            Some(Path::new("/Users/me/project/")),
            "/Users/me/project/src/lib.rs",
        )
        .unwrap();
        assert_eq!(key, "src/lib.rs");
    }

    #[test]
    fn glob_basic_patterns() {
        assert!(glob_match("*.rs", "foo.rs"));
        assert!(!glob_match("*.rs", "foo.ts"));
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/sub/lib.rs"));
        assert!(glob_match("src/**", "src/a/b.rs"));
        assert!(glob_match("src/**/*.rs", "src/a/b.rs"));
        assert!(glob_match("**/*.html", "a/b/c.html"));
    }

    #[tokio::test]
    async fn list_dir_on_empty_workspace_includes_create_hint() {
        let dir = std::env::temp_dir().join(format!("dm-empty-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let mut session = ToolSession::new(Some(dir.clone()));
        let outcome = execute(
            &mut session,
            &ToolCall::ListDir {
                path: String::new(),
            },
        )
        .await;
        assert!(
            outcome.feedback.contains("(directory is empty"),
            "expected empty marker, got: {}",
            outcome.feedback
        );
        assert!(
            outcome.feedback.contains("create{path"),
            "expected create-call hint, got: {}",
            outcome.feedback
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_dir_resolves_absolute_path_inside_workspace() {
        let dir = std::env::temp_dir().join(format!("dm-listabs-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("greet.txt"), "hi").await.unwrap();
        let mut session = ToolSession::new(Some(dir.clone()));
        let abs = dir.to_string_lossy().to_string();
        let outcome = execute(&mut session, &ToolCall::ListDir { path: abs }).await;
        assert!(
            outcome.feedback.contains("greet.txt"),
            "expected file in listing, got: {}",
            outcome.feedback
        );
        assert!(
            !outcome.feedback.contains("(directory is empty"),
            "non-empty dir should not show the empty hint"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn glob_accepts_comma_separated_patterns() {
        let dir = std::env::temp_dir().join(format!("dm-glob-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("index.html"), "<html/>")
            .await
            .unwrap();
        tokio::fs::write(dir.join("styles.css"), "body{}")
            .await
            .unwrap();
        tokio::fs::write(dir.join("script.js"), "console.log()")
            .await
            .unwrap();
        let mut session = ToolSession::new(Some(dir.clone()));
        let outcome = execute(
            &mut session,
            &ToolCall::Glob {
                pattern: "**/*.html, **/*.css".into(),
            },
        )
        .await;
        assert!(
            outcome.feedback.contains("index.html"),
            "expected html match, got: {}",
            outcome.feedback
        );
        assert!(
            outcome.feedback.contains("styles.css"),
            "expected css match, got: {}",
            outcome.feedback
        );
        assert!(
            !outcome.feedback.contains("script.js"),
            "should not have matched js: {}",
            outcome.feedback
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_emits_create_for_brand_new_paths() {
        let mut session = ToolSession::new(None);
        let outcome = execute_create(&mut session, "src/new.rs", "fn x() {}");
        assert!(outcome.mutated);
        let result = session.final_operations("ok".into());
        assert_eq!(result.operations.len(), 1);
        assert_eq!(
            result.operations[0].action,
            crate::messages::FileAction::Create
        );
        assert_eq!(result.operations[0].file, "src/new.rs");
    }

    #[test]
    fn session_emits_delete_for_paths_marked_deleted() {
        let mut session = ToolSession::new(None);
        let _ = execute_delete(&mut session, "old.rs");
        let result = session.final_operations(String::new());
        assert_eq!(result.operations.len(), 1);
        assert_eq!(
            result.operations[0].action,
            crate::messages::FileAction::Delete
        );
        assert!(result.operations[0].content.is_none());
    }

    #[tokio::test]
    async fn revert_restores_existing_file_to_original_content() {
        let dir = tempdir_with_file("greet.txt", "hello world\n").await;
        let mut session = ToolSession::new(Some(dir.clone()));
        let _ = execute(
            &mut session,
            &ToolCall::Edit {
                path: "greet.txt".into(),
                search: "world".into(),
                replace: "rust".into(),
                replace_all: false,
            },
        )
        .await;
        // Mid-edit, the staged content reflects the change.
        assert!(session
            .staged_op_for("greet.txt")
            .is_some_and(|op| op.content.as_deref() == Some("hello rust\n")));
        // Reverting wipes the staged action and brings content back.
        session.revert("greet.txt");
        assert!(session.staged_op_for("greet.txt").is_none());
        // Subsequent edits operate on the original content again.
        let outcome = execute(
            &mut session,
            &ToolCall::Edit {
                path: "greet.txt".into(),
                search: "hello".into(),
                replace: "hi".into(),
                replace_all: false,
            },
        )
        .await;
        assert!(outcome.mutated, "second edit should succeed against original");
        assert_eq!(
            session.staged_op_for("greet.txt").and_then(|op| op.content),
            Some("hi world\n".to_string()),
            "second edit should patch the original content, not the rejected one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revert_drops_brand_new_paths_entirely() {
        let mut session = ToolSession::new(None);
        let _ = execute_create(&mut session, "fresh.rs", "fn x() {}");
        assert!(session.staged_op_for("fresh.rs").is_some());
        session.revert("fresh.rs");
        assert!(session.staged_op_for("fresh.rs").is_none());
        assert!(!session.has_staged());
    }

    #[test]
    fn parse_bash_tool() {
        let raw = r#"{"tool":"bash","args":{"command":"cargo test","timeout":120}}"#;
        match parse_tool_action(raw) {
            ToolAction::Call {
                call:
                    ToolCall::Bash {
                        command,
                        timeout_secs,
                    },
                ..
            } => {
                assert_eq!(command, "cargo test");
                assert_eq!(timeout_secs, Some(120));
            }
            other => panic!("expected Bash, got {other:?}"),
        }
    }

    #[test]
    fn parse_semantic_search_tool() {
        let raw = r#"{"tool":"semantic_search","args":{"query":"agent loop","top_k":5}}"#;
        match parse_tool_action(raw) {
            ToolAction::Call {
                call: ToolCall::SemanticSearch { query, top_k },
                ..
            } => {
                assert_eq!(query, "agent loop");
                assert_eq!(top_k, Some(5));
            }
            other => panic!("expected SemanticSearch, got {other:?}"),
        }
    }

    #[test]
    fn session_collapses_multiple_creates_to_last_write() {
        let mut session = ToolSession::new(None);
        let _ = execute_create(&mut session, "x.rs", "v1");
        let _ = execute_create(&mut session, "x.rs", "v2");
        let result = session.final_operations(String::new());
        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.operations[0].content.as_deref(), Some("v2"));
    }

    #[tokio::test]
    async fn read_then_edit_roundtrips_through_session() {
        let dir = tempdir_with_file("hello.txt", "hello world\n").await;
        let mut session = ToolSession::new(Some(dir.clone()));

        let read_outcome = execute(
            &mut session,
            &ToolCall::ReadFile {
                path: "hello.txt".into(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        assert!(read_outcome.feedback.contains("hello world"));

        let edit = ToolCall::Edit {
            path: "hello.txt".into(),
            search: "world".into(),
            replace: "rust".into(),
            replace_all: false,
        };
        let edit_outcome = execute(&mut session, &edit).await;
        assert!(edit_outcome.mutated, "edit should mutate session");
        assert!(
            edit_outcome.feedback.contains("staged"),
            "feedback was: {}",
            edit_outcome.feedback
        );

        let result = session.final_operations("renamed world to rust".into());
        assert_eq!(result.operations.len(), 1);
        assert_eq!(
            result.operations[0].action,
            crate::messages::FileAction::Edit
        );
        assert_eq!(
            result.operations[0].content.as_deref(),
            Some("hello rust\n")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn tempdir_with_file(name: &str, body: &str) -> PathBuf {
        let id = uuid::Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("dm-tools-test-{id}"));
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create tempdir");
        let path = dir.join(name);
        tokio::fs::write(&path, body).await.expect("write fixture");
        dir
    }
}
