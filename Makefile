# Distributed Models — common developer tasks
#
# Run `make help` for the list of targets.
#
# Most targets assume you have a local Redis on 127.0.0.1:6379. If you don't,
# either `brew services start redis` or `docker compose up -d redis` from
# inside the repo (a `compose.yml` ships in the install scripts).

CARGO ?= cargo
DM_PORT ?= 3000
REDIS_URL ?= redis://127.0.0.1:6379/

# `cargo run` doesn't honour stdin colours by default; force them on.
export CARGO_TERM_COLOR := always

.DEFAULT_GOAL := help

.PHONY: help
help: ## Show this help.
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage: make \033[36m<target>\033[0m\n\nTargets:\n"} \
		/^[a-zA-Z_-]+:.*?##/ { printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

# ---------------------------------------------------------------------------
# Build & lint
# ---------------------------------------------------------------------------

.PHONY: build
build: ## Debug build of the binary + library.
	$(CARGO) build --all-targets

.PHONY: release
release: ## Optimised release build at target/release/distributed-models.
	$(CARGO) build --release

.PHONY: fmt
fmt: ## Apply rustfmt to the workspace.
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Verify formatting without rewriting files (CI gate).
	$(CARGO) fmt --all -- --check

.PHONY: clippy
clippy: ## Lint with clippy, denying warnings (CI gate).
	$(CARGO) clippy --all-targets --all-features -- -D warnings

.PHONY: check
check: fmt-check clippy ## Quick sanity check (fmt + clippy).

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

.PHONY: test
test: ## Run the full test suite (skips Redis-backed tests if Redis is down).
	$(CARGO) test --all-targets

.PHONY: test-unit
test-unit: ## Just the pure-logic tests (no network, no Redis).
	$(CARGO) test --test protocol --test parsers --test proposal_store

.PHONY: test-integration
test-integration: ## Tests that need Redis + a mock Ollama.
	$(CARGO) test --test http_server --test ollama_client --test agent_flow

.PHONY: test-flow
test-flow: ## Just the end-to-end agent pipeline test.
	$(CARGO) test --test agent_flow -- --nocapture

# ---------------------------------------------------------------------------
# Run / dev
# ---------------------------------------------------------------------------

.PHONY: serve
serve: ## Start the backend in foreground on $$DM_PORT (default 3000).
	$(CARGO) run --release -- serve --port $(DM_PORT)

.PHONY: chat
chat: ## Send a one-shot chat to a running backend (override MSG="...").
	$(CARGO) run --release -- chat $${MSG:-"explain what this project does"}

.PHONY: health
health: ## Hit /health on a running backend.
	$(CARGO) run --release -- health

# ---------------------------------------------------------------------------
# Redis helpers
# ---------------------------------------------------------------------------

.PHONY: redis-up
redis-up: ## Bring Redis up via docker compose (requires Docker).
	@docker compose up -d redis 2>/dev/null || \
		(echo "docker not available; trying brew services" && brew services start redis)

.PHONY: redis-down
redis-down: ## Stop the Redis service.
	@docker compose down 2>/dev/null || brew services stop redis 2>/dev/null || true

.PHONY: redis-flush
redis-flush: ## FLUSHALL on the configured Redis (use with care).
	@redis-cli -u "$(REDIS_URL)" FLUSHALL

# ---------------------------------------------------------------------------
# Editor overlay
# ---------------------------------------------------------------------------

.PHONY: apply-editor
apply-editor: ## Apply the editor/ overlay onto a vscode-oss clone at $$VSCODE.
	@if [ -z "$(VSCODE)" ]; then \
		echo "Usage: make apply-editor VSCODE=/path/to/vscode" >&2; \
		exit 2; \
	fi
	./editor/apply.sh "$(VSCODE)"

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------

.PHONY: clean
clean: ## Remove build artefacts.
	$(CARGO) clean
