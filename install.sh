#!/usr/bin/env bash
# install.sh - one-shot setup for the Distributed Models project.
#
# Steps:
#   1. Start Redis (docker compose, then redis-server / brew / apt fallback).
#   2. Install / start Ollama and pull the configured models.
#   3. Build the Rust backend in release mode.
#   4. Clone microsoft/vscode into this repo (if --with-editor is passed)
#      and apply the
#      editor overlay so the result builds as the "Distributed Models" editor.
#
# Run from the repo root:
#   ./install.sh                 # just the backend
#   ./install.sh --with-editor   # also clone & patch vscode (requires npm)
#
# Editor build is heavy (~10-15 GB checkout, 30-90 minutes on first run) so
# it is opt-in.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

WITH_EDITOR=0
EDITOR_DIR="$REPO_ROOT/editor/vscode-oss"
LEGACY_EDITOR_DIR="$REPO_ROOT/../distributed-models-editor"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --with-editor) WITH_EDITOR=1; shift ;;
        --editor-dir) EDITOR_DIR="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,17p' "$0"
            exit 0
            ;;
        *) echo "unknown flag: $1" >&2; exit 64 ;;
    esac
done

log() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }
warn() { printf "\033[1;33m!!\033[0m %s\n" "$*" >&2; }
die() { printf "\033[1;31mxx\033[0m %s\n" "$*" >&2; exit 1; }

if [ -f .env ]; then
    log "Loading defaults from .env"
    set -a
    # shellcheck disable=SC1091
    . ./.env
    set +a
fi

DM_MODEL_ORCHESTRATOR=${DM_MODEL_ORCHESTRATOR:-qwen2.5-coder:7b}
DM_MODEL_FILE_STRUCTURE=${DM_MODEL_FILE_STRUCTURE:-llama3.2:3b}
DM_MODEL_CODE_WRITER=${DM_MODEL_CODE_WRITER:-qwen2.5-coder:14b}
DM_MODEL_ERROR=${DM_MODEL_ERROR:-qwen2.5-coder:7b}
DM_MODEL_REVIEW=${DM_MODEL_REVIEW:-qwen2.5-coder:7b}

# ---------------------------------------------------------------------------
# 1. Redis
# ---------------------------------------------------------------------------
docker_running() {
    command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1
}

start_redis() {
    if docker_running && docker compose version >/dev/null 2>&1; then
        log "Starting Redis via docker compose"
        docker compose up -d redis
        return 0
    fi
    if docker_running && command -v docker-compose >/dev/null 2>&1; then
        log "Starting Redis via docker-compose"
        docker-compose up -d redis
        return 0
    fi
    if command -v docker >/dev/null 2>&1 && ! docker_running; then
        warn "Docker is installed but the daemon is not running; falling back to a local Redis install."
    fi
    if command -v redis-server >/dev/null 2>&1; then
        if pgrep -x redis-server >/dev/null 2>&1; then
            log "redis-server already running locally"
        else
            log "Starting redis-server in the background"
            redis-server --daemonize yes
        fi
        return 0
    fi
    case "$(uname -s)" in
        Darwin)
            if command -v brew >/dev/null 2>&1; then
                log "Installing redis via Homebrew"
                brew install redis
                brew services start redis
                return 0
            fi
            ;;
        Linux)
            if command -v apt-get >/dev/null 2>&1; then
                log "Installing redis via apt-get (sudo required)"
                sudo apt-get update -y
                sudo apt-get install -y redis-server
                sudo systemctl enable --now redis-server
                return 0
            fi
            if command -v dnf >/dev/null 2>&1; then
                log "Installing redis via dnf"
                sudo dnf install -y redis
                sudo systemctl enable --now redis
                return 0
            fi
            ;;
    esac
    die "Could not install or start Redis automatically. Please install it manually."
}

start_redis

# ---------------------------------------------------------------------------
# 2. Ollama
# ---------------------------------------------------------------------------
ensure_ollama() {
    if command -v ollama >/dev/null 2>&1; then
        return 0
    fi
    case "$(uname -s)" in
        Darwin)
            if command -v brew >/dev/null 2>&1; then
                log "Installing Ollama via Homebrew"
                brew install ollama
                return 0
            fi
            warn "Homebrew not found. Install Ollama manually from https://ollama.com/download"
            return 1
            ;;
        Linux)
            log "Installing Ollama via official script"
            curl -fsSL https://ollama.com/install.sh | sh
            return 0
            ;;
        *)
            warn "Unknown OS - install Ollama manually from https://ollama.com/download"
            return 1
            ;;
    esac
}

ensure_ollama || warn "Continuing without Ollama installation step."

start_ollama() {
    if curl -fsS "${OLLAMA_ENDPOINT:-http://127.0.0.1:11434}/api/tags" >/dev/null 2>&1; then
        log "Ollama API already reachable"
        return 0
    fi
    if command -v ollama >/dev/null 2>&1; then
        log "Starting ollama serve in the background"
        nohup ollama serve >/tmp/distributed-models-ollama.log 2>&1 &
        sleep 2
    fi
}

start_ollama

pull_model() {
    local model="$1"
    if ! command -v ollama >/dev/null 2>&1; then
        warn "ollama CLI missing; skipping pull for $model"
        return
    fi
    if ollama list 2>/dev/null | awk '{print $1}' | grep -Fxq "$model"; then
        log "Model already present: $model"
    else
        log "Pulling Ollama model: $model"
        ollama pull "$model"
    fi
}

pull_model "$DM_MODEL_ORCHESTRATOR"
pull_model "$DM_MODEL_FILE_STRUCTURE"
pull_model "$DM_MODEL_CODE_WRITER"
pull_model "$DM_MODEL_ERROR"
pull_model "$DM_MODEL_REVIEW"

# ---------------------------------------------------------------------------
# 3. Rust backend
# ---------------------------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    die "cargo not found - install Rust from https://rustup.rs first."
fi

log "Building Rust backend (release)"
cargo build --release

# ---------------------------------------------------------------------------
# 4. Editor (optional)
# ---------------------------------------------------------------------------
if [[ "$WITH_EDITOR" == "1" ]]; then
    if ! command -v git >/dev/null 2>&1; then
        die "git is required for --with-editor"
    fi
    if ! command -v npm >/dev/null 2>&1; then
        warn "npm not found; install Node.js (v22.x) before running 'npm i' inside the editor clone."
    fi

    # If the user previously cloned to ../distributed-models-editor (old default),
    # migrate that checkout into the new in-repo location automatically.
    if [[ "$EDITOR_DIR" == "$REPO_ROOT/editor/vscode-oss" ]] \
        && [[ ! -d "$EDITOR_DIR/.git" ]] \
        && [[ -d "$LEGACY_EDITOR_DIR/.git" ]]; then
        log "Migrating existing editor clone from $LEGACY_EDITOR_DIR to $EDITOR_DIR"
        mkdir -p "$(dirname "$EDITOR_DIR")"
        mv "$LEGACY_EDITOR_DIR" "$EDITOR_DIR"
    fi

    if [[ ! -d "$EDITOR_DIR/.git" ]]; then
        log "Cloning microsoft/vscode into $EDITOR_DIR"
        mkdir -p "$(dirname "$EDITOR_DIR")"
        git clone https://github.com/microsoft/vscode.git "$EDITOR_DIR"
    else
        log "Reusing existing vscode clone at $EDITOR_DIR"
    fi

    log "Applying Distributed Models overlay"
    "$REPO_ROOT/editor/apply.sh" "$EDITOR_DIR"

    if command -v npm >/dev/null 2>&1; then
        log "Running npm install inside the editor clone"
        ( cd "$EDITOR_DIR" && npm i )
        log "Run 'cd $EDITOR_DIR && npm run watch' in one terminal and './scripts/code.sh' in another to launch."
    else
        warn "Skipping npm install. Install Node.js (v22.x) and run 'npm i' inside $EDITOR_DIR before launching."
    fi
fi

log "Done."
echo
echo "Backend:"
echo "  ./target/release/distributed-models serve"
echo "  ./target/release/distributed-models health"
echo "  ./target/release/distributed-models chat 'hello'"
if [[ "$WITH_EDITOR" == "1" ]]; then
    echo
    echo "Editor:"
    echo "  cd $EDITOR_DIR && npm run watch     # in one terminal"
    echo "  cd $EDITOR_DIR && ./scripts/code.sh   # in another"
fi
