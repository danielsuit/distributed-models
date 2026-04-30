Build an open source code editor called "Distributed Models" forked from 
VSCodium/VS Code OSS. The editor has a built in multi-agent AI system 
that uses local Ollama models. Do not build this as an extension, 
build it directly into the editor source.

EDITOR BASE:
- Fork from VSCodium or VS Code OSS (github.com/microsoft/vscode)
- Remove Microsoft telemetry and branding
- Rename to "Distributed Models" throughout the UI
- Keep all standard VS Code editor features

CORE PROBLEM TO SOLVE:
Local models cannot use tool calls to write files directly. Instead:
- The editor itself handles ALL file operations natively in TypeScript
- Models only need to return structured JSON, no tool use required
- Format: { "action": "create|edit|delete", "file": "path", "content": "..." }
- The editor parses this JSON and performs the file operation itself
- This means ANY local Ollama model can write files regardless of tool support

AGENTS (Rust backend):
1. Orchestrator Agent (qwen2.5-coder:7b)
   - Receives user request from editor sidebar
   - Breaks into subtasks, delegates to other agents via Redis
   - Tells each agent to respond in structured JSON only

2. File Structure Agent (llama3.2:3b)
   - Maintains live JSON map of entire workspace file tree
   - Updates on every file change
   - Returns relevant file paths and structure when queried

3. Code Writer Agent (qwen2.5-coder:14b or largest available)
   - Receives coding tasks from orchestrator
   - Gets file context from File Structure Agent
   - MUST respond in this exact JSON format:
     {
       "action": "create",
       "file": "src/main.rs", 
       "content": "complete file contents here"
     }
   - Always writes complete files, never partial content

4. Error Agent (7b model)
   - Watches build errors and diagnostics
   - Returns fix instructions in same JSON format

5. Review Agent (7b model)
   - Validates Code Writer JSON before file is written
   - Returns approved: true/false with reason

COMMUNICATION:
- Redis queues for agent messaging
- Queues: agent:orchestrator, agent:filestructure, agent:codewriter, 
  agent:error, agent:review
- Message format: { id, from, to, task, context, result }

RUST BACKEND:
- src/main.rs - starts all agents as async tokio tasks
- src/agents/orchestrator.rs
- src/agents/file_structure.rs
- src/agents/code_writer.rs
- src/agents/error_agent.rs
- src/agents/review.rs
- src/config.rs - Redis URL, Ollama endpoint, model per agent
- Exposes a local REST API on port 3000 that the editor talks to

EDITOR INTEGRATION (TypeScript, built into VS Code OSS source):
- src/vs/workbench/contrib/distributedModels/browser/sidebar.ts
  - Chat UI panel built into workbench
  - Shows which agent is currently active
  - Streams responses as they come in

- src/vs/workbench/contrib/distributedModels/browser/fileOperations.ts
  - Parses JSON responses from agents
  - Shows Accept/Reject preview before writing
  - Performs actual file writes using VS Code's file system API
  - This is the key piece that bypasses tool use limitations

- src/vs/workbench/contrib/distributedModels/browser/agentClient.ts
  - Talks to Rust backend REST API on port 3000
  - Handles streaming responses
  - Manages agent status updates

- src/vs/workbench/contrib/distributedModels/browser/fileWatcher.ts
  - Watches workspace for changes
  - Feeds updates to File Structure Agent

- src/vs/workbench/contrib/distributedModels/browser/diagnosticsWatcher.ts
  - Monitors editor diagnostics
  - Triggers Error Agent on build failures

SETUP FILES:
- Cargo.toml
- docker-compose.yml for Redis
- .env with model assignments
- README.md with full build and setup instructions
- install.sh that:
  1. Clones VS Code OSS source
  2. Applies our modifications
  3. Installs Redis via docker
  4. Pulls required Ollama models
  5. Builds Rust backend
  6. Builds the editor with yarn
  7. Produces a runnable binary called distributed-models

Create every single file with complete contents, no placeholders or TODOs.
Project folder: distributed-models
# Distributed Models

A local, multi-agent AI coding backend written in Rust. It runs five
specialized agents that talk to each other over Redis and call local
[Ollama](https://ollama.com) models. There is no GUI: clients drive the
backend through an HTTP/WebSocket API or the bundled CLI.

## What it does

Send the backend a natural-language coding request. It will:

1. Plan the work using a small "orchestrator" model.
2. Optionally ask a small "file-structure" model for the most relevant paths
   in your workspace.
3. Hand the actual coding task to a larger "code-writer" model that returns
   complete files (never diffs).
4. Run a "review" model over the proposed files before they reach you.
5. Stream every step back to the client and prompt for accept/reject on each
   file. A diagnostics-driven "error" agent can also self-trigger fix runs
   when callers feed it compiler errors.

Everything runs on your machine. Redis is the only dependency that needs to
be online besides Ollama itself.

## Architecture

```
                        ┌───────────────────────┐
                        │   Client (CLI / API)  │
                        └──────────┬────────────┘
                                   │ WebSocket /ws
                                   ▼
                        ┌───────────────────────┐
                        │    axum HTTP server   │
                        └──────────┬────────────┘
                                   │ Redis lists + pub/sub
                                   ▼
   ┌────────────┬───────────────┬───────────────┬───────────┬────────────┐
   │Orchestrator│ FileStructure │   CodeWriter  │  Review   │ ErrorAgent │
   │   (7B)     │     (3B)      │   (14B/32B)   │   (7B)    │    (7B)    │
   └─────┬──────┴───────┬───────┴───────┬───────┴─────┬─────┴──────┬─────┘
         └──────────────┴──── Ollama HTTP API ────────┴────────────┘
```

Each agent is a long-running tokio task. They poll their inbox queue with
`BLPOP` (`agent:orchestrator`, `agent:filestructure`, `agent:codewriter`,
`agent:review`, `agent:error`) and call Ollama via HTTP. The orchestrator
publishes status, log, file-proposal, assistant-message and error events on
the `events:extension` Redis pub/sub channel; the HTTP server forwards them
to connected WebSocket clients.

## Prerequisites

- Rust 1.74+ (`rustup` recommended)
- Docker (for the bundled Redis), or a local `redis-server`
- [Ollama](https://ollama.com/download)

The `install.sh` script will install/start Redis and Ollama on macOS and
common Linux distros, pull the default models, and build the release binary.

## Quick start

```bash
git clone <this-repo>
cd distributed-models

./install.sh        # one-shot setup

./target/release/distributed-models serve
```

In another terminal:

```bash
./target/release/distributed-models health
./target/release/distributed-models chat "Write a hello-world axum server"
```

The CLI streams every agent status update to stdout and prompts you to accept
or reject each proposed file. Pass `--auto-accept` to take everything without
prompting, or `--workspace /path/to/dir` to attach a workspace root.

## Manual setup

If you'd rather wire things up yourself:

```bash
docker compose up -d redis           # start Redis on :6379
ollama serve &                       # start Ollama on :11434
ollama pull qwen2.5-coder:7b         # and the other models in .env
cargo run --release -- serve         # start the backend on :7878
```

## Configuration

All configuration is environment-driven; see `.env` for the defaults.

| Variable | Default | Purpose |
| --- | --- | --- |
| `DM_HOST` | `127.0.0.1` | HTTP/WebSocket bind host |
| `DM_PORT` | `7878` | HTTP/WebSocket bind port |
| `REDIS_URL` | `redis://127.0.0.1:6379/` | Redis URL used as the message bus |
| `OLLAMA_ENDPOINT` | `http://127.0.0.1:11434` | Ollama HTTP base URL |
| `DM_MODEL_ORCHESTRATOR` | `qwen2.5-coder:7b` | Planning agent |
| `DM_MODEL_FILE_STRUCTURE` | `llama3.2:3b` | Workspace ranker |
| `DM_MODEL_CODE_WRITER` | `qwen2.5-coder:14b` | File generator |
| `DM_MODEL_ERROR` | `qwen2.5-coder:7b` | Diagnostics handler |
| `DM_MODEL_REVIEW` | `qwen2.5-coder:7b` | Output validator |
| `RUST_LOG` | `info,distributed_models=debug` | tracing filter |

Override any of them in your shell or by editing `.env`.

## HTTP / WebSocket API

The backend listens on `DM_HOST:DM_PORT`. Endpoints:

- `GET /` - liveness blurb.
- `GET /health` - JSON with config + model assignments.
- `GET /ws` - bidirectional WebSocket. Clients send JSON messages and receive
  JSON events.

### Client → server messages

```jsonc
{ "type": "user_message", "text": "Refactor foo.rs", "workspace_root": "/path" }
{ "type": "file_snapshot", "workspace_root": "/path", "files": [ ... ] }
{ "type": "file_change", "workspace_root": "/path", "change": { "kind": "changed", "path": "src/foo.rs" } }
{ "type": "diagnostics", "workspace_root": "/path", "diagnostics": [ { "file": "src/foo.rs", "line": 10, "column": 5, "severity": "error", "message": "..." } ] }
{ "type": "proposal_decision", "proposal_id": "<uuid>", "accepted": true }
```

### Server → client events

```jsonc
{ "type": "agent_status", "job_id": "...", "agent": "orchestrator", "status": "planning" }
{ "type": "log", "job_id": "...", "agent": "filestructure", "message": "Indexed 42 entries" }
{ "type": "file_proposal", "job_id": "...", "proposal_id": "<uuid>", "file": "src/foo.rs", "content": "...", "review_notes": "..." }
{ "type": "assistant_message", "job_id": "...", "text": "Done. Accepted: src/foo.rs" }
{ "type": "error", "job_id": "...", "message": "..." }
{ "type": "job_complete", "job_id": "..." }
```

Every file in a proposal must be accepted or rejected before the orchestrator
sends `assistant_message` and `job_complete`.

## Project layout

```
.
├── Cargo.toml
├── docker-compose.yml          # Redis container
├── install.sh                  # One-shot setup script
├── .env                        # Default env vars
├── README.md
└── src
    ├── main.rs                 # CLI: serve | chat | health
    ├── cli.rs                  # WebSocket-based chat client
    ├── server.rs               # axum HTTP/WebSocket server
    ├── bus.rs                  # Redis helpers (BLPOP/RPUSH/PUBLISH)
    ├── messages.rs             # Wire types shared by every agent
    ├── ollama.rs               # Thin async wrapper around Ollama
    ├── proposals.rs            # Pending file-proposal registry
    ├── config.rs               # Env-driven configuration
    └── agents
        ├── mod.rs              # Spawns every agent task
        ├── orchestrator.rs     # Plans + drives each chat job
        ├── file_structure.rs   # Maintains the workspace map
        ├── code_writer.rs      # Generates complete files
        ├── error_agent.rs      # Reacts to diagnostics
        └── review.rs           # Validates code-writer output
```

## Custom clients

The bundled CLI is a working WebSocket client (see `src/cli.rs` for ~120 lines
of reference code). You can talk to the backend from any language: the
WebSocket protocol is plain JSON. Examples:

```bash
# Watch events with websocat
websocat ws://127.0.0.1:7878/ws

# Send a one-shot message with curl + websocat
echo '{"type":"user_message","text":"hello"}' | websocat -n1 ws://127.0.0.1:7878/ws
```

## Development

```bash
cargo check        # quick type check
cargo build        # debug build
cargo run -- serve # run the daemon
cargo test         # (no tests yet)
```

The backend recovers from agent panics on its own; each agent loop logs and
sleeps briefly before retrying.

## License

MIT.
