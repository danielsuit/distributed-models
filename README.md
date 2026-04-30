Build a VS Code extension in TypeScript called "Distributed Models" - a local 
multi-agent AI coding assistant that uses Ollama models. 

The system uses specialized agents that communicate via Redis:

AGENTS:
1. Orchestrator Agent (qwen2.5-coder:7b or similar small model)
   - Receives user request from VS Code sidebar
   - Breaks it into subtasks and delegates to other agents
   - Assembles final response back to VS Code

2. File Structure Agent (small 3b model like llama3.2:3b)
   - Maintains a live JSON map of the entire workspace file tree
   - Watches for file changes using VS Code file system watcher
   - When asked, returns relevant file paths and structure to other agents

3. Code Writer Agent (larger model like qwen2.5-coder:14b or 32b)
   - Receives specific coding tasks from orchestrator
   - Gets file context from File Structure Agent
   - Writes complete files, never diffs or patches
   - Returns exact file path + complete file contents

4. Error Agent (7b model)
   - Watches VS Code diagnostics and terminal output
   - When a build error occurs, automatically triggers and suggests fixes
   - Sends fix tasks back through the orchestrator

5. Review Agent (7b model)
   - Validates Code Writer output before it gets written to disk
   - Checks for obvious errors, missing imports, syntax issues

COMMUNICATION:
- Redis queues for agent-to-agent messaging
- Each agent has an inbox queue: agent:orchestrator, agent:filestructure, 
  agent:codewriter, agent:error, agent:review
- Message format: JSON with fields: id, from, to, task, context, result

BACKEND (Rust):
- src/main.rs - starts all agent workers as async tokio tasks
- src/agents/orchestrator.rs - orchestrator logic
- src/agents/file_structure.rs - file tree management
- src/agents/code_writer.rs - code generation
- src/agents/error_agent.rs - error watching
- src/agents/review.rs - output validation
- src/config.rs - Redis URL, Ollama endpoint, model assignments per agent
- Each agent polls its Redis queue with BLPOP and calls Ollama API

VSCODE EXTENSION (TypeScript):
- extension/package.json - extension manifest named "distributed-models"
- extension/src/extension.ts - activates sidebar and connects to Rust backend
- extension/src/sidebar.ts - chat UI panel (like Cursor's sidebar)
- extension/src/fileWatcher.ts - watches workspace and feeds File Structure Agent
- extension/src/diagnosticsWatcher.ts - feeds errors to Error Agent
- Sidebar UI: clean chat interface, shows which agent is currently working,
  displays file operations before applying them with an Accept/Reject button

FILE OPERATIONS:
- Never use diffs or patches
- Code Writer returns: { "file": "src/main.rs", "content": "..." }
- Extension shows a preview diff in VS Code before writing
- User clicks Accept to write the file, Reject to discard

SETUP FILES:
- Cargo.toml with all Rust dependencies
- extension/package.json with vsce, typescript dependencies  
- docker-compose.yml for Redis
- .env with default model assignments
- README.md with full setup instructions
- install.sh script that installs Redis, pulls Ollama models, builds Rust 
  backend, and installs VS Code extension all in one command

Create every single file with complete contents, no placeholders or TODOs.
The project folder should be called "distributed-models"