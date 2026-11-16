# Configuration

Cocoa reads `~/.config/cocoa/config.json`. A missing file opens setup. Unknown keys and malformed JSON are errors; the file is limited to 64 KiB. Use `/aliases` or `cocoa aliases` for model configuration, or edit the file directly. API keys do not belong in this file.

## Minimal configuration

For a local, tool-capable Ollama model already installed as `qwen3-coder`:

```json
{
  "aliases": {
    "local": [
      { "provider": "ollama", "model": "qwen3-coder", "priority": 0, "fallback_only": false }
    ]
  },
  "default_alias": "local"
}
```

Replace the model ID with one available to you. Hosted providers require [authentication](#credentials). Each entry stores the provider and its model ID separately; a model ID may itself contain slashes.

## Model aliases

An alias names a nonempty list of model choices. Each entry requires `provider` and `model`, and can set `effort`, `priority` and `fallback_only`. `priority` is an unsigned 8-bit integer constrained to `0` through `99`, with a default of `0`. `fallback_only` is a boolean with a default of `false`.

```json
{
  "aliases": {
    "review": [
      { "provider": "openai", "model": "gpt-5", "effort": "high", "priority": 0, "fallback_only": false },
      { "provider": "openai", "model": "gpt-5", "effort": "low", "priority": 1, "fallback_only": false },
      { "provider": "ollama", "model": "qwen3-coder", "priority": 1, "fallback_only": true }
    ]
  },
  "default_alias": "review"
}
```

The fallback-only flag separates regular choices from startup fallbacks:

- `fallback_only: false` marks a regular choice. Regular choices are sorted in ascending priority order. Equal priorities keep their list order.
- `fallback_only: true` marks a startup fallback. Fallback-only choices are tried in ascending priority order. Equal priorities keep their list order.
- Every alias must have at least one regular choice. A fallback-only list is invalid.

To migrate an older configuration, replace each negative priority with its absolute positive value and set `fallback_only` to `true`. Keep regular choices at `fallback_only: false`, or omit the flag. Negative priorities are no longer valid.

Alias names use 1 through 64 ASCII letters, digits, underscores or hyphens. There may be up to 32 aliases, 32 choices per alias and 256 choices in total. Entries name provider models directly, not other aliases.

Omit `effort` to use the provider default. Supported values depend on the [provider and model](providers.md#thinking-and-model-restrictions).

`default_alias` must name a configured alias. `--alias NAME` or `--model NAME` overrides it for one launch; the two flags cannot be combined. `/model NAME` selects an alias before the conversation starts. `/models` offers aliases and discovered provider models. Direct `provider/model` selection remains available without defining an alias.

Selecting an alias by name starts with its first regular choice. The `/models` alias picker lists alias names without a first-model detail. Use left/right to select among the highlighted alias's saved regular choices, then Enter to select that choice for the session. This does not change its saved effort. The separate provider model browser supports effort changes for direct selections.

Before the first prompt, Ctrl+T cycles regular choices within the current alias, skipping fallback-only entries. It does not move to another alias. Use `/aliases` or `cocoa aliases` to edit model lists, effort, priorities and the saved default. The editor has a fallback-only toggle for each model. `/alias` is not a command; use `/aliases` for the editor. Selecting a model for one session does not change the saved default.

### Startup fallback

Fallback is limited to startup, before any successful model response. Cocoa tries the selected regular choice, then only choices with `fallback_only: true`, in ascending priority order with ties keeping their list order. It does so when credentials or provider setup fail, or when the provider explicitly rejects the initial request with an HTTP error. Other regular choices are available through the alias picker or Ctrl+T, not automatic fallback.

Cocoa does not automatically replay a request after partial or uncertain output, a transport interruption or an ambiguous timeout. Once the conversation starts, the model stays fixed; later errors do not switch models. Use `/new` to choose a different model for a fresh conversation.

## Credentials

Run `cocoa login PROVIDER` or `/login PROVIDER` for hidden API-key entry. CLI login also accepts a key through piped stdin. Never put a real key in command arguments. ChatGPT instead uses browser sign-in; Ollama needs no credential.

Credential precedence for hosted API-key providers is:

1. A configured `credential_env` selects that variable exclusively.
2. Otherwise, a saved credential takes precedence.
3. If no credential is saved, use the provider's [default environment variable](providers.md#supported-providers).

A missing explicit variable, malformed auth file or failed saved command is a credential error; Cocoa does not try a lower-precedence credential source. Startup may still try another configured model choice under the [fallback rules](#startup-fallback). To use your own variable name, merge this provider entry into your configuration:

```json
{
  "providers": {
    "openai": { "credential_env": "MY_OPENAI_KEY" }
  }
}
```

Provider entries may also set `endpoint` to a complete inference URL. HTTPS is required except on a literal loopback IP. URL credentials, queries and fragments are rejected. The URL is used verbatim and does not change the provider's protocol. ChatGPT rejects endpoint and credential overrides. Only use trusted endpoints; they receive the selected credential.

To retrieve a key from a password manager, enter a trusted command prefixed with `!` during login, for example `!security find-generic-password -ws 'openai'`. It must finish within five seconds and print only the key, without interactive input. Restart Cocoa after changing the command or its credential.

`logout` removes saved API credentials without revoking the key or unsetting environment variables. ChatGPT sign-out also attempts remote revocation and reports failures. Restart to replace credentials in a running session.

Saved credentials are plaintext. Cocoa requires owner-only permissions, `0600` for files and `0700` for their directories. This does not protect them from other processes running as you.

## Settings

| Key | Default | Meaning |
| --- | --- | --- |
| `providers` | `{}` | Optional endpoint and credential-variable overrides |
| `aliases` | `{}` | Named model lists with priorities and fallback-only flags |
| `default_alias` | Unset | Configured alias to select at startup unless overridden |
| `theme` | `dark-chocolate` | `dark-chocolate`, `cacao-grove`, `milk-chocolate` or `terminal-native` |
| `reduced_motion` | `false` | Static busy indicator |
| `message_steering` | `true` | Queue messages while the agent works |
| `web_search` | `{"provider":"exa"}` | Built-in Exa, disabled, or an MCP server; see [search](customization.md#web-search) |
| `mcp_servers` | `{}` | HTTP server configuration managed by `cocoa mcp`; see [MCP](customization.md#mcp-servers) |
| `input_bytes` | `8388608` | Prompt text and encoded images, from 1 byte to 8 MiB |
| `output_bytes` | `65536` | Cell/tool feedback, from 1 byte to 1 MiB; not a model-answer limit |
| `timeout_ms` | `600000` | Model-stream inactivity and each JavaScript cell, from 1 ms to one hour; not a whole-turn deadline |
| `settlement_timeout_ms` | `2000` | Error cleanup deadline, from 1 to 60,000 ms |
| `shutdown_timeout_ms` | `2000` | Session shutdown deadline, from 1 to 60,000 ms |
| `agents_active` | `4` | Concurrent children, from 1 to 16 |
| `agents_retained` | `8` | Retained children/results, from `agents_active` to 32 |
| `compaction` | See below | Encoded-request and summary budgets |

`--theme` and `--reduced-motion` override presentation for one launch without saving it. `/theme` changes the current view only. Most execution settings apply when creating a session; changing them can prevent resume of an existing session.

## Compaction

Cocoa automatically summarizes older conversation history to make room for continued work. Original history remains available, and live JavaScript state is unchanged.

Add or update the `compaction` object in your configuration to change its budgets:

```json
{
  "compaction": {
    "request_bytes": 1048576,
    "trigger_bytes": 786432,
    "target_bytes": 524288,
    "summary_input_bytes": 2097152
  }
}
```

These are the defaults. All limits measure encoded bytes, not tokens:

| Key | Meaning |
| --- | --- |
| `request_bytes` | Maximum model request size |
| `trigger_bytes` | Request size that triggers summarization |
| `target_bytes` | Desired request size after summarization, not a guaranteed result |
| `summary_input_bytes` | Maximum complete summary request size; an oversized request fails before contacting the provider |

Require `0 < target_bytes < trigger_bytes < request_bytes` and a positive `summary_input_bytes`. Neither `request_bytes` nor `summary_input_bytes` may exceed 256 MiB. Increasing byte limits can still exceed the model's context window.

Summaries use the selected model and thinking settings. ChatGPT does not support summarization; start a new session with `/new` when the conversation reaches this limit. Failed summarization reports an error without deleting history.

## File locations

| Data | Location |
| --- | --- |
| Configuration | `~/.config/cocoa/config.json` |
| Saved API keys | `~/.config/cocoa/auth.json` |
| ChatGPT registrations/tokens | `~/.config/cocoa/oauth/chatgpt.json` |
| MCP registrations/tokens | `~/.config/cocoa/oauth/mcp/` |
| Global project instructions | `~/.config/cocoa/AGENTS.md` |
| Sessions | `$XDG_DATA_HOME/cocoa/sessions.sqlite3` when XDG_DATA_HOME is absolute; otherwise `~/.local/share/cocoa/sessions.sqlite3` |

The session default also applies on macOS. `--data-dir DIRECTORY` changes session storage without relocating config or credentials. Do not remove session files while another Cocoa process is using them.
