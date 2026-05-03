# Distributed Models Editor Overlay

This directory contains the source files that turn a fresh
[`microsoft/vscode`](https://github.com/microsoft/vscode) clone into
**Distributed Models**: an open-source code editor with the multi-agent AI
system built directly into the workbench.

> Building a VS Code OSS fork from scratch is a multi-gigabyte, multi-step
> process. This overlay does **not** include the editor source itself; it
> only contains the files we add and a script that copies them into a
> vscode clone you provide.

## Layout

```
editor/
├── README.md                   (this file)
├── apply.sh                    Copy the overlay into a vscode clone
├── product.overrides.json      Branding / product.json patches
└── src/
    └── vs/workbench/contrib/distributedModels/
        ├── browser/
        │   ├── distributedModels.contribution.ts
        │   ├── sidebar.ts
        │   ├── fileOperations.ts
        │   ├── agentClient.ts
        │   ├── fileWatcher.ts
        │   ├── diagnosticsWatcher.ts
        │   └── media/
        │       ├── sidebar.css
        │       └── distributed-models.svg
        └── common/
            ├── distributedModels.ts
            └── types.ts
```

## Apply to a vscode clone

```bash
git clone https://github.com/microsoft/vscode ../vscode-fork
cd distributed-models
./editor/apply.sh ../vscode-fork
cd ../vscode-fork
yarn         # or npm install
yarn watch   # in one terminal
yarn run electron   # in another
```

The `apply.sh` script will:

1. Copy every file under `editor/src/` into the corresponding path inside the
   target vscode clone.
2. Append a registration line to
   `src/vs/workbench/workbench.common.main.ts` so the contribution loads at
   startup.
3. Apply the product.json overrides for branding (name, version, applicationName
   etc.) so the binary boots as "Distributed Models".

## Communicates with the Rust backend

All UI elements in this overlay talk to the Rust backend over its REST + SSE
API on `http://127.0.0.1:3000` — nothing here uses tool-calling, every model
write goes through `{action, file, content}` JSON which the editor itself
materialises via `IFileService`.

## What's intentionally not done here

- **Telemetry stripping.** VSCodium's [`patches/`](https://github.com/VSCodium/vscodium/tree/master/patches)
  set is the right reference. `product.overrides.json` zeroes out
  telemetry endpoints, but you'll want to additionally apply VSCodium's
  patch series for a fully scrubbed build.
- **Native build & packaging.** Producing a signed `.app`/`.deb`/`.exe`
  binary is environment-specific. The top-level `install.sh` will run
  `yarn` against the patched clone but won't sign or package.

This overlay is meant to be a clear, applyable starting point — not a
turnkey replacement for the full VSCodium pipeline.
