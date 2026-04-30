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