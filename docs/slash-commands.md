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
| `/update [--check \| --version VERSION \| --unpin]` | Update Cocoa, report availability or manage a persistent version pin |

`/resume` also accepts `--all` and `--cwd DIRECTORY`, as described in the [CLI reference](cli.md#sessions). Switching sessions requires idle work and confirmation before discarding a draft. A session's model stays fixed after it starts; use `/new` before choosing a different model.

`/update` also works during an active turn. It runs in the background without sending a model request, restarting Cocoa or changing the conversation. A new binary takes effect on the next launch. Its options match [`cocoa update`](cli.md#updates).

Selecting an alias by name starts with its first regular choice. The `/models` alias picker shows alias names without a first-model detail. Left/right selects among the highlighted alias's saved regular choices without changing their saved effort. The separate provider model browser still uses left/right to change effort for direct selections. Use `/aliases` to edit saved effort or toggle fallback-only status for each model. `/alias` is not a command.

The model selector puts pinned choices first, then up to three recent choices. The provider model browser groups the remaining models by provider. Each choice appears once. Provider sections are alphabetical. Within each provider, known onboarding suggestions come first, followed by unranked models alphabetically; ChatGPT keeps its discovered model ranking. This is a curated order, not measured popularity. Alias lists keep the remaining aliases alphabetical because an alias can span providers. Pins affect navigation only, not the active model, defaults, saved effort or fallback order.

Use Ctrl+Space to pin or unpin the highlighted alias or model. New pins go at the end. With a pin highlighted, use Alt+Up or Alt+Down to change its position. Pin order persists across restarts. Reordering is disabled while filtering; clear the filter first. Filtering in the provider model browser matches provider names, model IDs and display names without case sensitivity. Empty groups disappear.

Selector preferences live in ~/.config/cocoa/selector.json, separately from model configuration. Recent choices update when you confirm a selection, not when you browse or when a model falls back automatically.

Before the conversation starts, Ctrl+T cycles regular choices within the current alias. Fallback-only choices are not selectable in the alias picker or that cycle. See [model aliases](configuration.md#model-aliases) for priority ordering and [startup fallback](configuration.md#startup-fallback) for when Cocoa can try another choice. A direct model accepts an optional effort level; `default` uses the provider default. Alias selection uses the saved effort and cannot take a level override.

`/rewind` requires idle work with no active child agents. Up and Down select a prompt, Enter forks, and Esc cancels. The original session is preserved. Files are unchanged, JavaScript bindings and handles reset, and shared context and child agents are not copied. A nonempty draft requires confirmation before replacement.

Use `$skill-name` for [skills](customization.md#skills), not `/skill-name`. To exit, enter exactly `exit`, `:q` or `:x`; these are not slash commands. Keyboard shortcuts are in [usage](usage.md#compose-a-message).
