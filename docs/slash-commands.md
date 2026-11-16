# Slash commands

Type `/` in chat to browse commands. Enter completes a partial command; Enter on a complete command executes it. Commands run locally rather than becoming model prompts. Use them while idle.

| Command | Behavior |
| --- | --- |
| `/models [NAME or provider/model] [LEVEL]` | Pick an alias or discovered model, or select one directly; a thinking override requires a direct model |
| `/model [NAME or provider/model] [LEVEL]` | Select an alias by name or a direct model, or open the model picker |
| `/aliases` | Edit alias model lists, effort, priorities, per-model fallback-only toggles and the saved default |
| `/login [PROVIDER]` | Choose a provider and save a hidden API key, or sign in to ChatGPT |
| `/logout [PROVIDER]` | Remove a saved key, or open ChatGPT account sign-out |
| `/usage` | Open ChatGPT Manage usage in the browser |
| `/mcp` | Manage servers, tools, authentication and access |
| `/sessions [QUERY]` | Browse saved sessions, optionally searching titles and root messages |
| `/resume [SESSION]` | Resume an ID or prefix, or open the picker |
| `/resume --last [--all]` | Resume the most recently active session in scope |
| `/new` | Start an empty conversation |
| `/rewind` | Pick an accepted root prompt, fork before it, and restore it with attachments to the editor |
| `/copy` | Copy the latest completed assistant answer with its Markdown source |
| `/theme [NAME]` | Choose or apply a palette for the current view, without saving it |

`/resume` also accepts `--all` and `--cwd DIRECTORY`, as described in the [CLI reference](cli.md#sessions). Switching sessions requires idle work and confirmation before discarding a draft. A session's model stays fixed after it starts; use `/new` before choosing a different model.

Selecting an alias by name starts with its first regular choice. The `/models` alias picker shows alias names without a first-model detail. Left/right selects among the highlighted alias's saved regular choices without changing their saved effort. The separate provider model browser still uses left/right to change effort for direct selections. Use `/aliases` to edit saved effort or toggle fallback-only status for each model. `/alias` is not a command.

Before the conversation starts, Ctrl+T cycles regular choices within the current alias. Fallback-only choices are not selectable in the alias picker or that cycle. See [model aliases](configuration.md#model-aliases) for priority ordering and [startup fallback](configuration.md#startup-fallback) for when Cocoa can try another choice. A direct model accepts an optional effort level; `default` uses the provider default. Alias selection uses the saved effort and cannot take a level override.

`/rewind` requires idle work with no active child agents. Up and Down select a prompt, Enter forks, and Esc cancels. The original session is preserved. Files are unchanged, JavaScript bindings and handles reset, and shared context and child agents are not copied. A nonempty draft requires confirmation before replacement.

Use `$skill-name` for [skills](customization.md#skills), not `/skill-name`. To exit, enter exactly `exit`, `:q` or `:x`; these are not slash commands. Keyboard shortcuts are in [usage](usage.md#compose-a-message).
