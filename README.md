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
