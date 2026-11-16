# Customization

Cocoa supports project instructions, skills and HTTP MCP tools. These guide or extend the agent; none creates a sandbox or grants isolation from your user's files and processes.

## AGENTS.md

Put repository-specific commands, constraints and conventions in `AGENTS.md`. Cocoa loads instructions in this order:

1. Optional global instructions at `~/.config/cocoa/AGENTS.md`.
2. Ancestor `AGENTS.md` files strictly below your home and above the launch directory, broadest first.
3. The launch directory's `AGENTS.md`.

Discovery does not stop at a Git boundary. Outside home, Cocoa loads global and launch-directory instructions only. It does not automatically load `CLAUDE.md` or `AGENTS.override.md`.

Nested rules load as the agent accesses files or runs commands in their directory tree. Shell commands must set `working_directory`; a `cd` inside command text does not load rules. Nested discovery stays inside the original launch directory.

Direct user instructions take precedence, project rules override global rules, and narrower scopes override broader ones. Instructions guide the model; they do not enforce access controls.

Keep each file under 64 KiB and write it as UTF-8 text.

## Skills

Skills package instructions and supporting files for a particular task. Type `$` to browse, or include `$skill-name` as a whitespace-separated reference in a prompt. The agent loads the selected body before working. `/` is reserved for built-in commands.

Create a skill directory containing `SKILL.md` under one of these roots:

| Scope | Roots |
| --- | --- |
| Personal | `~/.agents/skills/`, `~/.config/cocoa/skills/` |
| Project | `.agents/skills/`, `.cocoa/skills/` at the launch directory and ancestors strictly below home |

Outside home, only launch-directory project roots participate. `SKILL.md` requires YAML frontmatter delimited by `---`, a `name`, a `description` and a nonblank Markdown body. Names have 1–64 lowercase ASCII letters/digits with single internal hyphens; descriptions have at most 1024 characters. Files are limited to 64 KiB, with up to 64 discovered skills. Use unique names so a `$` reference identifies one skill.

Put supporting files beside `SKILL.md` and reference them relative to its directory. Restart after adding or renaming skills to refresh completion. Loading a skill does not execute its scripts, install dependencies or add permissions.

## MCP servers

MCP servers expose external tools to root and child agents. Browse curated endpoints and their authentication requirements without connecting:

```sh
cocoa mcp presets
cocoa mcp presets --json
```

Add a preset, use it under another name, or provide a custom URL:

```sh
cocoa mcp add notion
cocoa mcp add work-notion --preset notion
cocoa mcp add docs https://example.com/mcp
```

Replace the example URL with your server. Adding saves the configuration, connects and opens browser OAuth when needed. A failed connection leaves the saved entry for retry. Use `--no-connect` to save without contacting it, or `--no-auth` to connect without browser sign-in. `cocoa mcp auth NAME` authorizes later. `--client-id ID` supplies a pre-registered public OAuth client when dynamic registration is unavailable.

Check each server's region, permissions and authentication requirements before connecting. A preset does not grant account access; some services require vendor approval or authentication Cocoa does not support.

Use `/mcp` for the menu or the [CLI commands](cli.md#mcp-servers) to inspect tools and manage access. Disabling stops new calls while keeping credentials. Logout forgets local OAuth credentials; removal also deletes the configuration. Neither undoes effects or revokes remote consent.

### Header authentication

Use environment references for API-key servers. With `EXA_API_KEY` already exported:

```sh
cocoa mcp add exa https://mcp.exa.ai/mcp --header-env x-api-key=EXA_API_KEY
```

The configuration stores the variable name, not its value. The variable must contain the complete header value, including `Bearer ` where required. Up to eight `Authorization` or `X-*` headers are allowed per server. Header authentication and OAuth client registration cannot be combined. Missing or rejected keys do not trigger anonymous or OAuth fallback; `mcp logout` cannot unset environment variables.

Only connect trusted endpoints; they receive the headers you configure. The agent's shell can still access secrets in files and other processes running as you.

### Supported servers

Cocoa supports HTTP MCP tools over HTTPS, or plain HTTP on a literal loopback IP. Both stateless and Streamable HTTP servers are supported. Stdio servers, MCP resources and prompts are not supported. Configure at most sixteen servers.

The agent discovers and calls server tools through its [REPL](repl.md#mcp-tools).

## Web search

Built-in Exa search is enabled by default without an account or API key. An explicit search sends the supplied query, not the conversation or provider credentials. Queries can contain text the agent chose from your task, so do not assume they are free of sensitive information. Keyless service access can be rate-limited.

```sh
cocoa search exa
cocoa search disabled
cocoa search mcp exa
```

The MCP option requires an existing enabled server and directs the agent to its native tools instead of `cocoa.web_search`. Adding a server alone does not select it. Selection saves configuration for new sessions without making a network request.

The corresponding `web_search` values are `{"provider":"exa"}`, `{"provider":"disabled"}` and `{"provider":"mcp","server":"exa"}`. A failed MCP search server does not fall back to Exa; select a different provider explicitly. Disabling search does not block shell networking or unrelated MCP tools.
