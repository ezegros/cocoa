# Cocoa

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/cocoa/svg/cocoa-mark-cream.svg">
    <img src="docs/brand/cocoa/svg/cocoa-mark-brown.svg" alt="Cocoa pod" height="96">
  </picture>
</p>

Cocoa is an open-source coding agent for your terminal, written in Rust. Ask it to explore a codebase, change files or run tests.

The agent works through a persistent JavaScript REPL. It can combine tool calls, process their results and reuse variables across steps without sending every intermediate result back to the model. Each subagent gets its own conversation and REPL while sharing your project files.

Choose a hosted model or local Ollama, connect tools through MCP, and guide the agent with AGENTS.md and skills.

## Get started

Install on macOS or Linux:

```sh
curl -fsSL https://github.com/ezegros/cocoa/releases/latest/download/install.sh | bash
```

See [installation](docs/installation.md) for prerequisites, version pinning and source builds. Release installation requires a published release.

Open a terminal in your project and run:

```sh
cocoa
```

Follow setup to connect a provider and select a model. Then describe the work, or supply an initial prompt:

```sh
cocoa "Explain how this project is organized"
```

Press Enter to send, Ctrl+J for a new line, or Esc twice to stop a turn. Type `/` to browse commands, `@` to reference a file, or `$` to choose a skill. Continue the most recent conversation with `cocoa resume --last`.

For automation after configuring a model and credentials, use `cocoa exec "INSTRUCTION"` or pipe text to `cocoa exec`. It runs once without a terminal and prints a JSON outcome after cleanup. See [one-shot execution](docs/cli.md#one-shot-execution) for the contract. A successful exit means execution completed, not that the task was solved correctly.

**Cocoa can edit files and run shell commands with your user permissions, without approval prompts or a sandbox.** Use trusted projects and tools, and review its changes.

## Documentation

| Guide | Covers |
| --- | --- |
| [Installation](docs/installation.md) | Install, update and uninstall |
| [Usage](docs/usage.md) | Terminal controls, images, sessions and troubleshooting |
| [Configuration](docs/configuration.md) | Settings, model aliases, credentials and data locations |
| [Providers](docs/providers.md) | Models, authentication and subscription restrictions |
| [CLI reference](docs/cli.md) | Subcommands and flags |
| [Slash commands](docs/slash-commands.md) | Commands inside chat |
| [Customization](docs/customization.md) | AGENTS.md, skills, MCP and search |
| [Architecture](docs/architecture.md) | How Cocoa's components work together |
| [REPL API](docs/repl.md) | Installed JavaScript capabilities for advanced use |

## Contribute

[CONTRIBUTING.md](CONTRIBUTING.md) covers building from source, tests and pull requests.

## License

[Apache-2.0](LICENSE).
