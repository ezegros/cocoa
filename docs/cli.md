# CLI reference

Use `cocoa --help` or `cocoa COMMAND --help` for the interface in your installed version.

```sh
cocoa [OPTIONS] [PROMPT]
cocoa COMMAND
```

Without a subcommand, Cocoa opens interactive chat. An optional prompt sends immediately; Cocoa stays open after the answer. Use `cocoa exec` for one-shot execution. There is no `--prompt` flag.

## Launch options

| Option | Purpose |
| --- | --- |
| `-w, --worktree NAME` | Create a branch and isolated worktree for a fresh interactive session; name required |
| `-m, --model MODEL` | Configured alias or direct `provider/model` |
| `--thinking LEVEL` | Thinking for a direct `--model` selection; `default` omits it |
| `-a, --alias ALIAS` | Select a configured alias; conflicts with `--model` |
| `--data-dir DIRECTORY` | Session storage directory; also accepted by subcommands |
| `--theme THEME` | Override the palette for this launch |
| `--reduced-motion` | Use a static busy indicator |
| `-h, --help` | Show help |
| `-V, --version` | Show version |

The theme names and execution settings are in [configuration](configuration.md). Thinking levels depend on the [provider](providers.md). An alias uses its selected choice's saved `effort`; `--thinking` cannot override an alias. Without an explicit model or alias, `default_alias` selects the startup alias.

## One-shot execution

```sh
cocoa exec --model openai/gpt-6.1-sol --thinking high "Explain how this project is organized"
cocoa exec --alias medium --export run.json < instruction.txt > outcome.json
```

`exec` runs one text instruction in the current directory and exits. It needs no terminal on stdin or stdout. Instruction arguments are joined with spaces; with no instruction arguments, Cocoa reads UTF-8 stdin until EOF. Arguments take precedence over stdin. Put flags before the instruction, or use `--` before instruction text beginning with `-`. Blank, invalid UTF-8 and oversized input fail before session creation. The configured `input_bytes` bound applies to both input sources. Stdin is text only, not a stream of follow-up messages or image attachments.

Execution uses the same session builder, persistent JavaScript REPL, coding tools, configured MCP tools, child agents, compaction policy and SQLite history as interactive chat. It creates a new session, not a resumed conversation. Configure models and credentials beforehand. Missing selections, invalid configuration and unavailable model credentials fail instead of opening setup or authentication. Configured startup fallback still applies; unresolved provider failures terminate instead of opening manual retry. `--theme` and `--reduced-motion` have no effect on `exec`.

When the root answers, Cocoa saves that answer and closes the session. Closing cancels outstanding children and waits for owned engines and coding environments to settle under the configured cleanup deadlines. The root must await any child whose result it needs. Child cancellation is explicit in `child_outcomes` and the optional full export; a completed root does not mean every child completed its assignment.

SIGINT and SIGTERM request cancellation through the engine and the same cleanup path. Cocoa keeps waiting for settlement rather than dropping the running future. A signal during session construction is remembered while construction completes its ownership and cleanup obligations. Credential refreshes already in progress finish before exit under their own deadlines. Cancellation does not undo workspace changes, stop arbitrary detached processes, or prove that a remote provider stopped generating. `cleanup: "confirmed"` covers Cocoa's owned resources and bookkeeping, not arbitrary external effects. SIGKILL, a host crash or a broken output pipe can prevent a terminal JSON result.

After execution and shutdown, stdout contains one compact JSON object followed by a newline. Diagnostics go to stderr. Argument-parser errors occur before execution and use stderr without this JSON object; help and version do not execute anything.

| Field | Meaning |
| --- | --- |
| `schema_version` | Outcome format version, currently `1` |
| `session_id`, `root_agent_id` | Permanent session reference and root identity, or `null` before creation |
| `invocation_id` | Completed root invocation ID, or `null` if no authoritative answer returned |
| `final_answer` | Authoritative root answer, retained even if later cleanup or export fails; otherwise `null` |
| `outcome` | `completed`, `failed` or `cancelled`; this is an execution result, not a task grade |
| `cleanup` | `not_required` before owned session resources exist, `confirmed` after successful close, or `unconfirmed` when settlement is unknown |
| `failure` | `configuration`, `input`, `startup`, `execution`, `cleanup`, `export` or `signals`; otherwise `null` |
| `signal` | `SIGINT`, `SIGTERM` or `null` |
| `accounting` | Whole-session request coverage and token totals from durable root, child and compaction evidence; `null` when unavailable |
| `child_outcomes` | Agent-ID map of retained child results, or `null` if export evidence is unavailable. A null entry is unknown, not completed. The durable `Completed.value.status` distinguishes `completed`, `cancelled` and settled `failed` results; `Failed` records coordination failures |
| `exported` | Whether the requested `--export PATH` file was successfully written |

Exit status `0` means the root answered, cleanup settled and final evidence collection succeeded. It does **not** mean the instruction was correctly solved, tests passed, or a benchmark verifier accepted the workspace. Other failures return `1`. Handled SIGINT and SIGTERM return `130` and `143`, including when cleanup remains unconfirmed. Always inspect `cleanup` before treating a cancelled run as settled.

`--export PATH` writes the sanitized whole-session JSON described below, after shutdown and before the stdout outcome. The destination must be a new file in an existing directory. Cocoa writes it privately and refuses to replace an existing file. An export error makes execution fail without discarding an already completed root answer. A failed or cancelled run still attempts export when it has a session attachment. Neither this command nor its export scores correctness or runs a benchmark.

## Updates

`cocoa update` installs a newer stable release unless the installation is pinned. Use `--check` to report availability, `--version VERSION` to install and persistently pin an exact release, or `--unpin` to resume stable updates. These options are mutually exclusive. No terminal, model credentials or valid provider configuration is required.

Updates take effect on the next launch. Source-installed and unmanaged executables require their original installation method. See [automatic updates, pinning and rollback](installation.md#update-roll-back-or-uninstall).

## Models and authentication

| Command | Purpose |
| --- | --- |
| `cocoa models` | Select an alias or discover provider models |
| `cocoa aliases` | Edit alias model lists, effort, priorities, per-model fallback-only toggles and the saved default |
| `cocoa login PROVIDER` | Save an API key from hidden input or stdin; ChatGPT opens browser sign-in |
| `cocoa logout PROVIDER` | Remove a saved credential or open ChatGPT account sign-out |

For a simple update, `cocoa alias NAME --provider PROVIDER --model MODEL [--thinking LEVEL]` changes the alias's first regular choice, leaving the rest of its model list intact. `MODEL` is the provider's model ID without the provider prefix. Omitting `--thinking` keeps the choice's effort; `--thinking default` clears it to the provider default. A new alias gets one regular choice at priority `0` with `fallback_only: false`.

Selecting an alias by name starts with its first regular choice. The alias picker lists alias names without a first-model detail. Left/right selects among the highlighted alias's saved regular choices, keeping each choice's saved effort. The separate provider model browser supports effort changes for direct selections. Use the alias editor to change saved effort.

Ctrl+T cycles regular choices within the current alias before the first prompt. Automatic fallback uses only choices with `fallback_only: true`, in ascending priority order with ties keeping their list order. It is limited to [startup failures](configuration.md#startup-fallback); Cocoa does not replay partial or uncertain output or switch models once the conversation has started.

## Sessions

```sh
cocoa sessions list --limit 10
cocoa sessions search "authentication" --all
cocoa resume --last
```

| Command | Options |
| --- | --- |
| `cocoa sessions list` | `--all`, `--limit 1..50`, `--before CURSOR`, `--json` |
| `cocoa sessions search QUERY` | `--all`, `--limit 1..50`, `--json` |
| `cocoa sessions export SESSION` | `--json` for compact JSON; otherwise pretty-printed JSON |
| `cocoa resume [SESSION]` | `--all`, `--cwd DIRECTORY` |
| `cocoa resume --last` | `--all`, `--cwd DIRECTORY` |

List/search default to the current directory and twenty results. They require no terminal or credentials and do not create missing storage. Search is literal and case-insensitive across titles and root conversation text.

List JSON contains `sessions` and `next_cursor`. Pass that cursor to `--before` with the same scope flags. Text output prints its cursor to stderr. Search has no pagination. Treat session IDs and cursors as opaque strings.

Resume accepts a full ID or an unambiguous `sess_` prefix with at least eight hexadecimal digits. Omit the ID to open the picker. `--last` cannot be combined with an ID. `--cwd` explicitly relocates a saved project. See [resume behavior](usage.md#continue-a-session) before changing settings or moving data.

### Run exports

`cocoa sessions export SESSION --json` exports retained evidence without model credentials, configuration resolution, recovery or execution. It accepts the same full ID or unique prefix as resume and requires exclusive session ownership, so close a running session before exporting it. Missing storage or sessions fail without creating a new session.

The versioned export contains session metadata, effective nonsecret settings, agent membership and terminal outcomes, per-agent histories and checkpoints, shared context, and a request ledger with actual provider/model/effort/output limits, request purpose, timestamps, reported usage and terminal status. It does not include credentials, provider continuations or provider-private reasoning. Prompts, answers, tool output, paths and file contents may still contain secrets. Review exports before sharing them; they are neither replay tokens nor workspace snapshots.

`accounting.reported_usage` sums known cumulative counters and can be a lower bound. `accounting.usage` is `null` unless all requests have final usage and the ledger covers the whole session. Unknown usage is not zero. Cache read/write counters remain independently unknown when a provider omits them. `ledger_complete: false` identifies older or forked evidence that cannot establish complete request accounting. No monetary cost is inferred from token counts or subscription access.

Both CLI export paths use whole-export bounds of 10,000 source rows and 64 MiB. Exceeding either returns an explicit error, never silently truncated evidence. A null child outcome or unmatched request start remains unknown; exporting never turns it into completion or reruns work.

## MCP servers

| Command | Purpose |
| --- | --- |
| `cocoa mcp` | Open the server menu |
| `cocoa mcp presets [--json]` | List curated endpoints and setup requirements without connecting |
| `cocoa mcp add NAME [URL]` | Save and connect a server; omit URL to use a matching preset |
| `cocoa mcp auth NAME` | Authorize an OAuth server in the browser |
| `cocoa mcp list` | Connect enabled servers and show status |
| `cocoa mcp tools NAME` | Show callable names and JSON schemas |
| `cocoa mcp enable NAME` | Allow new calls |
| `cocoa mcp disable NAME` | Stop new calls while retaining credentials |
| `cocoa mcp logout NAME` | Forget local OAuth credentials |
| `cocoa mcp remove NAME` | Remove the server configuration and local credentials |

`mcp add` accepts `--preset NAME` instead of a URL, `--client-id ID`, repeatable `--header-env HEADER=ENV`, `--no-auth` to skip browser login, and `--no-connect` to save without connecting. Header credentials and OAuth client registration are mutually exclusive. See [MCP setup and security](customization.md#mcp-servers).

## Search selection

```sh
cocoa search exa
cocoa search disabled
cocoa search mcp SERVER
```

Selection saves configuration for new sessions without making a network request. The MCP server must already exist and be enabled. Disabling built-in search does not disable other MCP tools or shell networking. See [web search](customization.md#web-search).
