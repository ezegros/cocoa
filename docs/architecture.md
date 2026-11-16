# Architecture

Cocoa is a terminal coding agent built around a persistent JavaScript REPL. The model writes JavaScript, QuickJS evaluates it, and Rust implements file, process and external-tool calls. The agent can combine operations and retain results between steps, sending only selected output back to the model.

## The agent loop

1. The terminal accepts a user message.
2. The engine combines it with project instructions, shared context and conversation history.
3. The selected model answers or submits a JavaScript cell.
4. The REPL calls local tools or connected MCP servers and returns printed output and errors.
5. The model continues until it answers, fails or is cancelled.

## Workspace crates

| Crate | Responsibility |
| --- | --- |
| [`cocoa-core`](../crates/cocoa-core/src/lib.rs) | Agent loop, model and tool interfaces, shared context and compaction |
| [`cocoa-llm`](../crates/cocoa-llm/src/lib.rs) | Model-provider integrations and response streaming |
| [`cocoa-oauth`](../crates/cocoa-oauth/src/lib.rs) | Browser sign-in and saved OAuth credentials |
| [`cocoa-quickjs`](../crates/cocoa-quickjs/src/lib.rs) | JavaScript execution and bindings to Rust tools |
| [`cocoa-tools`](../crates/cocoa-tools/src/lib.rs) | Local files and processes, project instructions and skills |
| [`cocoa`](../crates/cocoa/src/lib.rs) | CLI and terminal UI, configuration, credentials, agent coordination, MCP and SQLite sessions |

The executable connects these libraries. The core depends on interfaces rather than a specific terminal, provider, database or filesystem implementation. QuickJS executes JavaScript; the tool layer performs operations in the local Unix environment.

## Agents and state

Each agent has its own conversation and JavaScript state. Variables survive ordinary cells and turns. Child agents share project files and explicit `context` records, but not private conversations or variables. Children cannot spawn further children.

File reads provide references that subsequent edits use to detect stale contents. Agents still share one filesystem; they do not work in isolated copies.

## Sessions and long conversations

Cocoa saves conversations and shared context in SQLite. [Resume](usage.md#continue-a-session) restores that information, starts fresh JavaScript runtimes and never replays earlier commands or restarts child work.

Automatic [compaction](configuration.md#compaction) summarizes older conversation history for the model while preserving the originals. It does not reset live JavaScript state.

## Security boundaries

Cocoa runs files and shell commands with your Unix user permissions, without a sandbox or approval prompts. Skills and AGENTS.md guide the model but do not enforce access controls. Cancellation does not roll back completed actions.

See the [REPL API](repl.md) for installed operations and [CONTRIBUTING.md](../CONTRIBUTING.md) for building and testing.
