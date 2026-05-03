# Distributed Models

An open-source code editor (a fork of [VS Code OSS](https://github.com/microsoft/vscode))
with a built-in multi-agent AI system that uses local
[Ollama](https://ollama.com) models. The agents run in a Rust backend; the
editor itself materialises every file write the agents propose, so any
local model — tool-calling or not — can edit your workspace by returning
plain JSON.

## Repository layout

```
.
├── Cargo.toml
├── docker-compose.yml          # Redis container
├── install.sh                  # One-shot setup (backend, plus optional editor fork)
├── distributed-models.yaml     # Primary backend config (host/port/models)
├── .env                        # Optional env override fallback
├── README.md
├── src/                        # Rust backend (5 agents + REST/SSE server + CLI)
├── tests/                      # Rust test suite (51 tests)
└── editor/
    ├── src/vs/workbench/contrib/distributedModels/...   # Overlay sources
    └── vscode-oss/                                      # Full VS Code checkout (created by install.sh --with-editor)
```

## How it works

The backend exposes a small REST API on `http://127.0.0.1:3000`. Both the
editor and the bundled CLI use the same endpoints; nothing is editor-specific.

```
                         ┌─────────────────────────────┐
                         │  Distributed Models editor  │
                         │  (VS Code OSS fork)         │
                         └──────────────┬──────────────┘
                                        │ REST  +  Server-Sent Events
                                        ▼
                         ┌─────────────────────────────┐
                         │   axum HTTP server :3000    │
                         └──────────────┬──────────────┘
                                        │ Redis lists + pub/sub
                                        ▼
   ┌────────────┬───────────────┬───────────────┬───────────┬────────────┐
   │Orchestrator│ FileStructure │   CodeWriter  │  Review   │ ErrorAgent │
   │   (7B)     │     (3B)      │   (14B/32B)   │   (7B)    │    (7B)    │
   └─────┬──────┴───────┬───────┴───────┬───────┴─────┬─────┴──────┬─────┘
         └──────────────┴──── Ollama HTTP API ────────┴────────────┘
```

Every agent is a long-running `tokio` task. They poll their inbox queue
(`agent:orchestrator`, `agent:filestructure`, `agent:codewriter`, `agent:review`,
`agent:error`) with `BLPOP` and call Ollama via HTTP. Status, log, file
proposal, assistant message and error events flow back through a Redis
`events:client` pub/sub channel that the HTTP server fans out to connected
SSE clients.

### Agent Architecture (Detailed)

There are six agents (plus an `Integration` agent kept for tests). Each is a
long-running `tokio` task spawned in `agents/mod.rs::spawn_all`.

#### Orchestrator (`orchestrator.rs`, 7B model)

Owns the lifecycle of every chat job. When a `user_message` or `auto_fix`
arrives it spawns a per-job task via an in-process registry
(`JobRegistry = Arc<Mutex<HashMap<String, mpsc::Sender<Message>>>>`). The
job task runs a linear pipeline:

1. **Plan** — sends the user message (plus up to 16 turns of conversation
   history) to the planner LLM with a system prompt that outputs strict JSON
   (`PlannerOutput`). The plan decides `need_files`, `need_code`,
   `file_query`, `code_instruction`, and `final_answer`.
2. **Index sweep** — if `need_files || need_code`, dispatches a `query`
   message to `FileStructure` and waits for `query_result`.
3. **Code generation** — if `need_code`, dispatches `write_code` to
   `CodeWriter` with the instruction, related files, target file, and
   workspace root. Waits for `code_writer_result`.
4. **Review** — sends the writer's result to `Review` for validation.
5. **Proposal loop** — each `FileOperation` is proposed to the user via
   SSE `FileProposal` events; the orchestrator blocks (up to 30 min per
   proposal) until the user accepts or rejects.
6. **Summary** — a final LLM call composes a friendly markdown summary of
   what was accepted/rejected.

A `user_expects_frontend_file_pass` heuristic forces `need_files=true` +
`need_code=true` when the request mentions CSS/styling/polish, preventing
the planner from hallucinating a `final_answer` without reading files.

#### FileStructure (`file_structure.rs`, 3B model)

Maintains a `WorkspaceState` (a `BTreeMap<String, FileEntry>`) persisted to
Redis under `state:filestructure`. Handles three tasks:

- **`snapshot`** — replaces the entire file map (sent by the editor's
  `fileWatcher.ts` on workspace open).
- **`created`/`changed`/`deleted`** — incremental updates from the editor's
  file-system watcher.
- **`query`** — filters the map by a keyword substring on paths. If the
  candidate set exceeds 8 files and the query is non-empty, the agent calls
  the LLM to rank paths by relevance (`ask_model_to_rank`), returning at
  most 12 ranked paths. Falls back to a broad file list when the keyword
  matches nothing.

**Current limitation:** The file map is flat — it stores paths, sizes, and
`is_dir` but has no knowledge of symbols, imports, or call relationships.

#### CodeWriter (`code_writer.rs`, 14B/32B model)

The most complex agent. Runs a **tool-use loop** (up to 25 iterations)
where the model emits one JSON tool call per turn:

- **Read-only tools:** `read_file`, `list_dir`, `grep`, `glob`,
  `semantic_search`, `bash`
- **Mutating tools:** `edit` (search/replace pair), `create`, `delete`
- **Terminal:** `finish`

Edits use Claude-Code-style search/replace pairs — the `search` string must
appear verbatim in the file. The agent maintains a `ToolSession` (an
in-memory virtual filesystem overlay in `tools.rs`) so subsequent reads
reflect pending changes. Each mutation is proposed to the user mid-loop;
rejections revert the session slot and the model gets feedback to try a
different approach.

Anti-loop guards:
- Consecutive duplicate read-only calls (same normalised path signature) →
  nudge after 2, bail after 4.
- Pure read-only streak cap of 12 calls with no mutations.
- Max 3 consecutive parse errors before giving up.
- Transcript is trimmed to 24K chars (oldest entries dropped first).

**Legacy fallback:** If a model returns the old `{operations, summary}`
envelope instead of tool calls, it's consumed directly.

#### Review (`review.rs`, 7B model)

Validates the CodeWriter's output before the user sees it. The prompt
includes the user's request, the planner's plan, the code instruction, the
indexer path hints, and all proposed file operations (truncated at 48K chars
per file). Returns `{approved, reason, problems}`. The review catches:

- Accidental file wipes (a later operation replacing a substantive file
  with something much shorter / empty / placeholder-only).
- Hollow CSS changes (only comments/TODO stubs with no real declarations).
- Missing stylesheet `<link>` tags in HTML that claims to be styled.
- Broken syntax, missing imports, off-task content.

If parsing fails, defaults to `approved: true` so the pipeline isn't
blocked by model errors.

#### ErrorAgent (`error_agent.rs`, 7B model)

Watches diagnostics streamed from the editor via `POST /diagnostics`.
Groups diagnostics by file, persists them to Redis under
`state:diagnostics`, and filters for severity `"error"`. When errors are
found, it calls the LLM to produce a short plain-English fix plan, then
forwards an `auto_fix` message into the orchestrator inbox so the normal
pipeline (index → write → review → propose) handles the repair.

**Current limitation:** The error agent has no access to the actual file
contents — it sees only the diagnostic messages (file, line, column,
severity, message). It cannot read the offending code before planning.

#### Integration (`integration.rs`, 7B model)

Runs after the code writer to propose **additional** wiring operations
(CSS `<link>` tags, import statements, router entries, nav links). Currently
bypassed in the default pipeline — the CodeWriter's tool loop now handles
wiring inline. Kept for tests and as an opt-in second pass. Has built-in
sanitization (`sanitize_integration_against_draft`) that drops integration
edits that slash file size vs. the draft baseline (catches accidental wipes).

#### Semantic Index (`index.rs`)

Not an agent, but a critical subsystem. Walks the workspace, splits each
text file into overlapping 80-line chunks (20-line overlap), computes Ollama
embeddings via `/api/embeddings`, and persists to `.dm-index/index.json`.
The CodeWriter's `semantic_search` tool queries this index by cosine
similarity. Caps: 256KB per file, 60 chunks per file, 8K total entries.
Rebuilds lazily on first search; incremental updates via mtime tracking.

#### Bus (`bus.rs`) + Proposals (`proposals.rs`)

Redis-backed message bus. Each agent has a dedicated `BLPOP` connection so
an idle agent never stalls the write path. The `ProposalStore` is an
in-memory `DashMap<String, oneshot::Sender<bool>>` — the HTTP handler
resolves proposals by sending `true`/`false` through the channel.

### The "no tool calls" trick

Local models often can't perform tool calls reliably. So the Code Writer
agent never asks the model to *do* anything — it only asks for JSON:

```json
{
    "operations": [
        { "action": "create", "file": "src/main.rs", "content": "fn main() {}" },
        { "action": "edit",   "file": "src/lib.rs",  "content": "pub fn run() {}" },
        { "action": "delete", "file": "src/old.rs" }
    ],
    "summary": "Bootstrapped the binary and removed the old shim."
}
```

The editor (or the CLI) parses the JSON, shows each operation as an
accept/reject prompt, then performs the actual file write through VS Code's
`IFileService`. This means literally any local model that can produce JSON
can edit your workspace.

## Prerequisites

- Rust 1.74+ ([rustup](https://rustup.rs))
- Docker (for the bundled Redis), or a local `redis-server`
- [Ollama](https://ollama.com/download)
- For the editor fork: `git`, `npm`, Node 22.x, Python 3 (for product.json
  merging). Building the editor takes 30–90 minutes on the first run and
  uses ~10–15 GB of disk.

## Quick start (backend only)

```bash
git clone <this-repo>
cd distributed-models

./install.sh                 # installs Redis/Ollama, pulls models, builds Rust

./target/release/distributed-models serve      # starts the daemon on :3000
```

In another terminal:

```bash
./target/release/distributed-models health
./target/release/distributed-models chat "Write a hello-world axum server"
```

The CLI streams every agent status update to stdout and prompts you to
accept or reject each proposed operation. Pass `--auto-accept` to take
everything automatically, or `--workspace /path/to/dir` to attach a
workspace root.

## Quick start (with editor fork)

```bash
./install.sh --with-editor                     # also clones vscode + applies overlay
./target/release/distributed-models serve

# In two more terminals (inside the editor clone):
cd editor/vscode-oss
npm run watch
./scripts/code.sh
```

The Distributed Models sidebar will appear in the activity bar. Type a
request, watch the agents work, accept/reject each file operation inline.

> The first editor build is large and slow. Use `./install.sh` to set up
> the backend first; you can run and develop against the daemon entirely
> from the CLI before tackling the editor build.

## Configuration

Primary config now lives in `distributed-models.yaml`:

```yaml
host: 127.0.0.1
port: 3000
redis_url: redis://127.0.0.1:6379/
ollama_endpoint: http://127.0.0.1:11434
models:
  orchestrator: qwen2.5-coder:7b
  file_structure: llama3.2:3b
  code_writer: qwen2.5-coder:14b
  error_agent: qwen2.5-coder:7b
  review: qwen2.5-coder:7b
```

The backend loads config in this order:
1. `distributed-models.yaml` (or custom path in `DM_CONFIG`)
2. env vars / `.env` fallback

The editor reads `distributedModels.backendUrl` from VS Code settings and
defaults to `http://127.0.0.1:3000`.

Inside the Distributed Models sidebar, the **Local models** section lets users
view and update all agent model names at runtime. Saving writes the new model
assignments to `distributed-models.yaml` and new chats use them immediately.

## REST + SSE API

The backend listens on `DM_HOST:DM_PORT` (default `127.0.0.1:3000`).

### Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/` | Liveness blurb. |
| `GET` | `/health` | JSON with config and model assignments. |
| `GET` | `/config` | Current runtime config + model assignments. |
| `POST` | `/config/models` | Update model assignments at runtime and persist to YAML. |
| `POST` | `/chat` | Start a chat job. Returns `{ "job_id": "..." }`. |
| `GET` | `/events` | Server-Sent Events stream of every event. Optional `?job_id=` filter. |
| `POST` | `/file-snapshot` | Replace the workspace file map. |
| `POST` | `/file-change` | Append a single file change. |
| `POST` | `/diagnostics` | Replace the diagnostics list. |
| `POST` | `/proposal/:id` | Resolve a pending file proposal. |

### Request payloads

```jsonc
// POST /chat
{ "text": "Refactor foo.rs", "workspace_root": "/path" }

// POST /file-snapshot
{ "workspace_root": "/path", "files": [{ "path": "src/foo.rs", "size": 12, "is_dir": false }] }

// POST /file-change
{ "workspace_root": "/path", "change": { "kind": "changed", "path": "src/foo.rs" } }

// POST /diagnostics
{ "workspace_root": "/path", "diagnostics": [
    { "file": "src/foo.rs", "line": 10, "column": 5, "severity": "error", "message": "..." }
] }

// POST /proposal/:id
{ "accepted": true }

// POST /config/models
{ "models": {
    "orchestrator": "qwen2.5-coder:7b",
    "file_structure": "llama3.2:3b",
    "code_writer": "qwen2.5-coder:14b",
    "error_agent": "qwen2.5-coder:7b",
    "review": "qwen2.5-coder:7b"
} }
```

### Server → client events (SSE `data:` payloads)

```jsonc
{ "type": "agent_status", "job_id": "...", "agent": "orchestrator", "status": "planning" }
{ "type": "log", "job_id": "...", "agent": "filestructure", "message": "Indexed 42 entries" }
{ "type": "file_proposal", "job_id": "...", "proposal_id": "<uuid>",
  "operation": { "action": "create", "file": "src/foo.rs", "content": "..." },
  "review_notes": "..." }
{ "type": "assistant_message", "job_id": "...", "text": "Done. Accepted: src/foo.rs" }
{ "type": "error", "job_id": "...", "message": "..." }
{ "type": "job_complete", "job_id": "..." }
```

Every operation in a proposal must be accepted or rejected before the
orchestrator sends `assistant_message` and `job_complete`.

## Testing

The Rust crate ships with 51 tests across seven files:

- `tests/protocol.rs` — wire format pinning for `Message`, `ClientEvent`,
  `FileOperation`, REST request payloads.
- `tests/parsers.rs` — the lenient parsers each agent uses to consume
  model output (`parse_code_writer_output`, `parse_plan`, `parse_verdict`,
  `parse_ranked_paths`, `parse_sse_data`).
- `tests/proposal_store.rs` — the in-memory proposal registry under
  concurrent access.
- `tests/ollama_client.rs` — boots a tiny axum mock for Ollama's
  `/api/generate` endpoint and verifies the wire body, response decoding
  and error paths.
- `tests/http_server.rs` — boots the real router (`build_router`) on an
  ephemeral port, exercises every REST endpoint and the SSE event stream.
  Skips automatically when no Redis is reachable.
- `tests/agent_flow.rs` — end-to-end pipeline test. Spins up all five
  agents against a mocked Ollama and a real Redis, drives a full chat
  through `POST /chat`, accepts a file proposal via `POST /proposal/:id`,
  and verifies the orchestrator emits `assistant_message` + `job_complete`
  on the SSE stream. Each test runs in its own queue namespace so a
  daemon already running on the canonical queues won't interfere.
- `tests/bus_integration.rs` — Redis bus invariants: prefixed queue
  roundtrip behavior and proof that a waiting `BLPOP` reader does not
  block dispatching writes on another queue.

Run the suite with:

```bash
make test               # all tests
make test-unit          # pure-logic only (no Redis, no network)
make test-integration   # http_server + ollama_client + agent_flow
make check              # cargo fmt --check && cargo clippy -D warnings
```

No new crate downloads are required — the suite uses only what the
backend already pulls in. CI (`.github/workflows/ci.yml`) runs the same
gates on every push, with a Redis service container so the
integration tests participate.

## Project layout (Rust)

```
src/
├── main.rs                 # CLI: serve | chat | health
├── lib.rs                  # Re-exports for the integration tests
├── cli.rs                  # REST + SSE chat client
├── server.rs               # axum HTTP server + SSE endpoint (22KB)
├── bus.rs                  # Redis helpers (BLPOP / RPUSH / PUBLISH)
├── messages.rs             # Wire types shared by every agent (12KB)
├── ollama.rs               # Thin async wrapper around Ollama (generate + embed)
├── tools.rs                # Tool catalog + virtual filesystem (57KB, largest file)
├── index.rs                # Semantic codebase index (embeddings + cosine search)
├── bash.rs                 # Sandboxed shell command execution
├── slash.rs                # Slash-command parser
├── proposals.rs            # Pending file-proposal registry (DashMap)
├── job_cancel.rs           # Per-job cancellation tokens
├── workspace_path.rs       # Path normalisation helpers
├── config.rs               # YAML + env-driven configuration
└── agents/
    ├── mod.rs              # AgentRuntime + spawn_all
    ├── orchestrator.rs     # Plans + drives each chat job (26KB)
    ├── code_writer.rs      # Tool-use loop + legacy parser (40KB)
    ├── file_structure.rs   # Workspace map + LLM-based path ranking
    ├── review.rs           # Validates code-writer output
    ├── error_agent.rs      # Reacts to diagnostics
    └── integration.rs      # Cross-file wiring (currently opt-in)
```

## Editor overlay (TypeScript)

The `editor/` directory stores both:
- the overlay sources committed in this repo (`editor/src/...`)
- a local VS Code OSS checkout at `editor/vscode-oss/` (created by `install.sh --with-editor`)

The TypeScript files
are placed at the workbench paths the spec expects:

```
src/vs/workbench/contrib/distributedModels/
├── browser/
│   ├── distributedModels.contribution.ts
│   ├── sidebar.ts
│   ├── fileOperations.ts
│   ├── agentClient.ts
│   ├── fileWatcher.ts
│   └── diagnosticsWatcher.ts
└── common/
    ├── distributedModels.ts
    └── types.ts
```

`product.overrides.json` adjusts product.json so the resulting binary
boots as "Distributed Models" with telemetry disabled.

See `editor/README.md` for the full applyability story.

## Current Architecture Gaps & Planned Improvements

All improvements are designed to remain **100% local** — no external API
calls, no cloud services. Everything runs through Ollama on the local
machine.

### 1. Graph-Based Code Indexing (replaces flat file map)

**Problem:** The `FileStructure` agent stores a flat `BTreeMap<path, FileEntry>`
with no knowledge of symbols, imports, or call chains. When the orchestrator
passes `related_files` to the CodeWriter, it's just a list of paths — the
model has to `read_file` each one to understand relationships. This wastes
tool-loop iterations and context tokens.

**Improvement:** Build a function-level dependency graph using `tree-sitter`
(already Rust-native) during the `snapshot` phase. Each node = a function,
type, or variable; each edge = a call, import, or type reference. The
`query` handler would then return not just paths but the relevant call
chain and type definitions for the code the user is asking about.

This is the approach validated by *ABCoder / UniAST* (arXiv 2604.18413):
parsing the codebase into a relational index lets the agent traverse
`Dependency` edges outward (what does this function call?) and `Reference`
edges inward (what calls this function?) — directly providing the context
that keyword search and similarity retrieval miss.

**Concrete change:** Add a `tree-sitter` parse pass in `file_structure.rs`
that produces a `HashMap<Symbol, Vec<Symbol>>` adjacency list alongside the
existing file map. The CodeWriter's prompt would receive "function X calls
Y, Z; is called by W" instead of just "files: [a.rs, b.rs, c.rs]".

### 2. Error Agent → Context-Aware Fix Pipeline

**Problem:** The `ErrorAgent` currently receives only the diagnostic
messages (file, line, severity, message) and generates a fix plan without
ever reading the actual source code. It then forwards a raw `auto_fix` to
the orchestrator, which re-runs the entire pipeline from scratch. The error
agent has zero context about what the code actually looks like.

**Improvement (from RepoTransAgent, arXiv 2508.17720):** Before generating
a fix plan, the error agent should:

1. **Read the offending file regions** — use `tools::execute_read_file` to
   fetch the lines around each error location.
2. **Resolve missing symbols** — if the error is "symbol not found", query
   the graph index for the symbol definition's location and read that too.
3. **Compose a targeted instruction** — include the actual code + the
   resolved symbol context in the `auto_fix` message, not just the error
   string.

This mirrors how the RepoTransAgent's Refine Agent works: after a test
failure it doesn't just retry — it re-invokes context-gathering tools
specifically for the error, then feeds that enriched context into the next
generation attempt.

**Concrete change:** Before `build_plan`, add a loop that calls
`read_through` for each diagnostic's file+line region and appends the code
snippet to the prompt. The plan LLM then sees both the error message AND
the actual code, producing far more targeted instructions.

### 3. Structured Dynamic Prompting for All Agents

**Problem:** Agent prompts are currently ad-hoc strings. The orchestrator's
planner system prompt is a single paragraph. The CodeWriter's system prompt
is a 90-line wall of text mixing goals, tool docs, rules, and exceptions.
This makes it hard for smaller models to extract the relevant instruction
for their current state.

**Improvement (from RepoTransAgent):** Standardize every agent prompt into
explicit sections:

| Section | Type | Description |
|---------|------|-------------|
| **Goals** | Static | Agent's role and objective |
| **Tools** | Static | Available tool names, params, descriptions |
| **Guidelines** | Static | Rules and anti-patterns |
| **Example** | Static | One concrete worked example of the task |
| **Input** | Dynamic | The current user request / instruction |
| **Gathered Context** | Dynamic | Files read, index results, prior tool outputs |
| **Output Format** | Static | Required JSON schema |
| **Last Command** | Dynamic | Previous tool call + result (prevents loops) |

The paper found this decomposition critical for smaller models (7B–14B) —
exactly the range this project uses. Smaller models struggle with long
unstructured prompts but handle clearly sectioned ones much better.

**Concrete change:** Refactor `SYSTEM_PROMPT` in `code_writer.rs` and
`PLANNER_SYSTEM` in `orchestrator.rs` into builder functions that assemble
the prompt from tagged sections. The `build_prompt` function already does
some of this (workspace root, indexer hints, transcript) but the sections
aren't labeled for the model.

### 4. Review-Before-Propose (Move Review Upstream)

**Problem:** The Review agent currently runs AFTER the CodeWriter has
finished and the user has already accepted/rejected every proposal inline.
By the time Review flags issues, the damage is done — rejected proposals
were already shown to the user and accepted ones are on disk.

**Improvement:** Run the Review agent on each individual mutation BEFORE
proposing it to the user. In the CodeWriter's tool loop
(`code_writer.rs:489`), after `outcome.mutated` but before
`propose_and_await`, dispatch the staged operation to Review. If Review
rejects, revert the session and inject the rejection reason into the
transcript so the model can self-correct on the next iteration — the user
never sees the bad proposal.

This turns Review from a post-hoc audit into a real-time guardrail.

### 5. Semantic Search as a First-Class Context Source

**Problem:** The semantic index (`index.rs`) is powerful but underused. It's
only available as a tool the CodeWriter can optionally call. The
orchestrator and error agent have no access to it. The `FileStructure`
agent's `query` handler does pure substring matching on paths, ignoring
code content entirely.

**Improvement:** Make semantic search a standard step in the orchestrator
pipeline. After the `FileStructure` path query, run a semantic search with
the user's request as the query. Pass the top-K code snippets (not just
paths) to the CodeWriter alongside the instruction. This gives the model
relevant code context before it starts its tool loop, reducing the number
of `read_file` calls needed to orient.

**Concrete change:** In `orchestrator.rs::run_job`, after the
`query_result` step, call `rt.semantic_index.ensure_built()` +
`rt.semantic_index.search()` and include the top 5 snippets in the
`write_code` message context.

### 6. Edit Validation via `bash` (Local Compilation Checks)

**Problem:** The CodeWriter proposes edits that may not compile, and the
only feedback loop is the user rejecting the proposal. The error agent only
activates when VS Code reports diagnostics — but CLI users get no such
feedback.

**Improvement:** After a mutating tool call is accepted, automatically run
a lightweight compilation check (`cargo check`, `tsc --noEmit`,
`python -m py_compile`, etc.) via the existing `bash` tool infrastructure.
Inject the compiler output into the transcript so the model can self-correct
in the same loop iteration rather than waiting for the error agent's
separate pipeline.

This mirrors RepoTransAgent's Refine Agent test-execution phase: generate →
test → reflect → correct, all within the same loop.

### 7. Conversation Memory for Multi-Turn Edits

**Problem:** The orchestrator passes up to 16 turns of conversation
history, but this history is only text — it doesn't include which files
were modified, which proposals were accepted/rejected, or what the
previous CodeWriter tool loop actually did. A follow-up request like
"now add tests for what you just wrote" forces the model to rediscover
everything from scratch.

**Improvement:** Persist a structured job summary after each completed job:
files touched, operations accepted/rejected, review notes. Include this
structured context in subsequent planner prompts so the model knows exactly
what changed and can build on it rather than re-exploring.

### References

- *RepoTransAgent: Multi-Agent LLM Framework for Repository-Aware Code
  Translation* — arXiv 2508.17720 (multi-agent decomposition, dynamic
  prompting, error-driven context retrieval, iterative refinement)
- *TypeScript Repository Indexing for Code Agent Retrieval* — arXiv
  2604.18413 (graph-based code indexing, function-level dependency graphs,
  UniAST format, ABCoder framework)

## Development

A Makefile wraps the common loops:

```bash
make build          # debug build
make release        # optimised release binary
make serve          # run the daemon on :3000
make chat MSG="..."  # send a chat to the running daemon
make health         # hit /health on the running daemon
make test           # full suite
make check          # fmt + clippy gate
make redis-up       # start Redis (docker compose, falls back to brew)
make apply-editor VSCODE=/path/to/vscode  # apply the editor overlay
```

The backend recovers from agent panics on its own; each agent loop logs and
sleeps briefly before retrying. The `Bus` type uses a multiplexed write
connection but opens a *dedicated* connection for every blocking `BLPOP`
so an agent that's idle can never stall the chat handler's writes.

## License

MIT.
