# rust-lean-mcp

High-performance Rust clone of Python lean-lsp-mcp (v0.25.1).

## Development

- **TDD**: Write tests before implementation
- Run `cargo test --all` before committing
- Run `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings`
- Coverage target: 90%+ (cargo-llvm-cov)

## Architecture

3-crate workspace:
- `lean-lsp-client`: Standalone Lean LSP client (no MCP dependency)
- `lean-mcp-core`: Business logic, models, utilities (no MCP/LSP dependency)
- `lean-mcp-server`: MCP server binary (rmcp + tool handlers)

## Commits

- Conventional commit messages
- No Anthropic co-author lines
- Link to GitHub issues: `Closes #N` or `Refs #N`

## Research

Local research docs in `.research/` (gitignored).

Python source reference:
`/Users/lclose/.cache/uv/archive-v0/yQ84HN6D1LSglQkjySxci/lib/python3.14/site-packages/lean_lsp_mcp/`

## GitHub Project

Track all work via [GitHub Project board](https://github.com/users/LarsenClose/projects/2).
All stories are GitHub Issues with labels, milestones, and acceptance criteria.
