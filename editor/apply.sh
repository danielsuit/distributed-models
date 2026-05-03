#!/usr/bin/env bash
# editor/apply.sh - Copy the Distributed Models overlay into a vscode clone.
#
# Usage:
#   ./editor/apply.sh /path/to/vscode-fork
#
# Idempotent: re-running is safe.

set -euo pipefail

if [[ $# -lt 1 ]]; then
	echo "usage: $0 /path/to/vscode-fork" >&2
	exit 64
fi

TARGET="$1"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ ! -d "$TARGET" ]]; then
	echo "error: target $TARGET does not exist" >&2
	exit 64
fi
if [[ ! -d "$TARGET/src/vs/workbench" ]]; then
	echo "error: target $TARGET does not look like a VS Code clone (missing src/vs/workbench)" >&2
	exit 65
fi

log() { printf "\033[1;34m==>\033[0m %s\n" "$*"; }

# 1. Copy every file under editor/src into target/, preserving the directory
#    layout so files land at their VS Code workbench paths.
log "Copying overlay sources into $TARGET"
mkdir -p "$TARGET/src/vs/workbench/contrib/distributedModels"
cp -R "$HERE/src/vs/workbench/contrib/distributedModels/." \
	"$TARGET/src/vs/workbench/contrib/distributedModels/"

# 2. Make sure the contribution is loaded by the workbench. We append an import
#    line to workbench.common.main.ts (idempotent).
MAIN="$TARGET/src/vs/workbench/workbench.common.main.ts"
if [[ ! -f "$MAIN" ]]; then
	echo "warning: $MAIN not found; you'll need to import the contribution manually" >&2
else
	IMPORT_LINE="import './contrib/distributedModels/browser/distributedModels.contribution.js';"
	if ! grep -Fq "$IMPORT_LINE" "$MAIN"; then
		log "Registering distributedModels contribution in workbench.common.main.ts"
		printf "\n// === Distributed Models ===\n%s\n" "$IMPORT_LINE" >> "$MAIN"
	else
		log "Contribution already registered in workbench.common.main.ts"
	fi
fi

# 2b. Disable built-in Chat/Copilot workbench contributions so Distributed
#     Models is the only chat surface in the activity bar.
if [[ -f "$MAIN" ]]; then
	log "Disabling built-in Chat/Copilot contributions"
	python3 - "$MAIN" <<'PYDM'
import pathlib
import re
import sys

main_path = pathlib.Path(sys.argv[1])
text = main_path.read_text()

# Keep core chat service contribution enabled (required by task/debug services).
text = re.sub(
    r"^(\s*)// \[DM DISABLED\] import '\./contrib/chat/browser/chat\.contribution\.js';\s*$",
    r"\1import './contrib/chat/browser/chat.contribution.js';",
    text,
    flags=re.MULTILINE,
)


# Restore any chat imports that may have been disabled by older overlay versions.
restore_imports = [
    "./contrib/chat/browser/chatSessions/chatSessions.contribution.js",
    "./contrib/chat/browser/contextContrib/chatContext.contribution.js",
    "./contrib/welcomeAgentSessions/browser/agentSessionsWelcome.contribution.js",
]
for imp in restore_imports:
    pattern = re.compile(rf"^(\s*)// \[DM DISABLED\] import '{re.escape(imp)}';\s*$", re.MULTILINE)
    text = pattern.sub(r"\1import '" + imp + "';", text)

chat_imports = [
    "./contrib/chat/browser/chat.view.contribution.js",
    "./contrib/inlineChat/browser/inlineChat.contribution.js",
]

for imp in chat_imports:
    pattern = re.compile(rf"^(\s*)import\s+['\"]{re.escape(imp)}['\"];\s*$", re.MULTILINE)
    text = pattern.sub(r"\1// [DM DISABLED] import '" + imp + "';", text)

main_path.write_text(text)
PYDM
fi

# 2c. Disable built-in Copilot extension bundle in the local fork.
COPILOT_DIR="$TARGET/extensions/copilot"
COPILOT_DISABLED_DIR="$TARGET/extensions/copilot.disabled"
if [[ -d "$COPILOT_DIR" ]]; then
	log "Disabling built-in Copilot extension"
	mv "$COPILOT_DIR" "$COPILOT_DISABLED_DIR"
fi

# 2d. Remove Copilot watch hooks from root package scripts so `npm run watch`
#     keeps working after Copilot is disabled.
PKG_JSON="$TARGET/package.json"
if [[ -f "$PKG_JSON" ]] && command -v python3 >/dev/null 2>&1; then
	log "Patching package.json watch scripts (remove watch-copilot)"
	python3 - "$PKG_JSON" <<'PYDM_PKG'
import json
import pathlib
import sys

pkg_path = pathlib.Path(sys.argv[1])
pkg = json.loads(pkg_path.read_text())
scripts = pkg.get("scripts", {})

watch = scripts.get("watch")
if isinstance(watch, str):
    watch = watch.replace(" watch-copilot", "")
    watch = watch.replace("watch-copilot ", "")
    scripts["watch"] = watch

scripts.pop("watch-copilot", None)
scripts.pop("watch-copilotd", None)
scripts.pop("kill-watch-copilotd", None)
scripts.pop("copilot:setup", None)
scripts.pop("copilot:get_token", None)

pkg["scripts"] = scripts
pkg_path.write_text(json.dumps(pkg, indent=2) + "\n")
PYDM_PKG
fi

# 3. Apply product.json overrides for branding. We do not overwrite the
#    target file blindly; we merge with python (always available on macOS
#    and most Linux distros).
PRODUCT_JSON="$TARGET/product.json"
OVERRIDES="$HERE/product.overrides.json"
if [[ -f "$PRODUCT_JSON" && -f "$OVERRIDES" ]] && command -v python3 >/dev/null 2>&1; then
	log "Merging product.json overrides"
	python3 - "$PRODUCT_JSON" "$OVERRIDES" <<'PY'
import json, sys, pathlib

product_path = pathlib.Path(sys.argv[1])
overrides_path = pathlib.Path(sys.argv[2])

product = json.loads(product_path.read_text())
overrides = json.loads(overrides_path.read_text())

for key, value in overrides.items():
	product[key] = value

product_path.write_text(json.dumps(product, indent=2) + "\n")
PY
else
	echo "warning: skipped product.json merge (need python3 + product.json + overrides)" >&2
fi

log "Overlay applied. Run 'npm i' inside $TARGET to install dependencies, then 'npm run watch' and './scripts/code.sh'."
