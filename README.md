# rust-lean-mcp

A high-performance Rust implementation of [lean-lsp-mcp](https://github.com/oOo0oOo/lean-lsp-mcp) — a Model Context Protocol server that bridges Lean 4's Language Server Protocol to AI assistants.

## Status

Under active development. See the [project board](https://github.com/users/LarsenClose/projects/2) for progress.

## Features (planned)

22 MCP tools for Lean 4 proof assistance:

- **Proof state** — goals, term goals
- **Code intelligence** — hover, completions, references, code actions
- **File analysis** — diagnostics, outline, declarations
- **Search** — LeanSearch, Loogle, LeanFinder, local ripgrep search
- **Build** — project build with progress, build coordination
- **Verification** — axiom checking, source scanning
- **Fast tactics** — REPL-based multi-attempt (~5x faster)
- **Profiling** — per-line proof performance analysis

## Architecture

3-crate workspace:

- `lean-lsp-client` — Standalone async Lean 4 LSP client
- `lean-mcp-core` — Business logic, models, utilities
- `lean-mcp-server` — MCP server binary (via rmcp)

## License

MIT
