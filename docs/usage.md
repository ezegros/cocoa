# Usage

Run `cocoa` from the directory you want the agent to work in. First-run setup connects a [provider](providers.md) and selects a model. Ask a question or describe a change in ordinary language; you do not need to write JavaScript.

```sh
cocoa "Find where requests are authenticated and explain the flow"
```

A positional prompt sends immediately and leaves chat open. Use `cocoa exec "INSTRUCTION"` to run once without a terminal, or `cocoa exec < instruction.txt` to read stdin. It prints a JSON execution outcome and exits after cleanup. Configure a model and credentials first; `exec` never opens setup or manual provider recovery. See the [one-shot CLI contract](cli.md#one-shot-execution) for signals, child cancellation, exports and exit statuses.

## Choose a model

Before the first prompt, open `/models` or press Ctrl+P. Choose an alias, browse a provider's models, or enter an exact `provider/model` ID. With no aliases configured, `/models` and Ctrl+P open the provider model browser directly. Select a configured alias directly with `/model NAME`, or launch with `cocoa --model NAME` or `cocoa --alias NAME`.

The alias picker shows alias names without a first-model detail. Use ↑/↓ to highlight an alias and ←/→ to select among its saved regular choices. Enter selects the choice; Esc cancels. Each choice uses its saved effort, which these controls do not change. Selecting an alias directly by name starts with its first regular choice.

Ctrl+T cycles the regular model choices within the current alias before the conversation starts. Regular choices have `fallback_only: false` and follow ascending priority order, with ties keeping their list order. Fallback-only choices never appear in the alias picker choices or this cycle. Use `/aliases` or `cocoa aliases` to edit alias model lists, effort, priorities and the saved default. The editor has a fallback-only toggle for each model.

In the separate provider model browser, use ↑/↓ to highlight a model and ←/→ to change its thinking effort on the scale at the bottom. Enter selects both; Esc cancels. The scale uses the model's reported levels when available, otherwise the provider's levels with an unverified-support notice. `default` leaves effort to the provider. These changes apply only to the session, not the saved alias. To change an alias choice's saved effort, use the alias editor.

An alias can try choices marked `fallback_only: true` at startup if credentials or provider setup fail, or if the provider explicitly rejects the initial request with an HTTP error before any successful response. Fallbacks follow ascending priority order, with ties keeping their list order. Cocoa does not automatically replay partial or uncertain output. A session keeps its model fixed after it starts, even on failure. Use `/new` for a fresh conversation before choosing another model. Selecting a model does not change your saved default. See [model aliases and startup fallback](configuration.md#model-aliases).

## Compose a message

| Input | Action |
| --- | --- |
| Enter | Send the draft |
| Ctrl+J | Insert a newline |
| Ctrl+Enter | Insert a newline when the terminal forwards it |
| `@` at the start of a word | Find a file or directory |
| `$` | Find a skill and insert `$skill-name` |
| `/` | Browse [slash commands](slash-commands.md) |
| Up with an empty draft | Recall the last sent message |
| Down while recalling | Restore the saved draft |
| Ctrl+U | Clear the whole draft |
| Ctrl+W | Delete the previous word |

In a file picker, Up/Down selects, Enter inserts a path, and Tab opens a directory or accepts a file. Press Enter again to send. A mention inserts path text, not file contents. The picker skips hidden and ignored entries; this does not restrict paths the agent can access.

With an existing draft, Up moves the cursor first. Press Up again at the first visual row to recall the last message while saving the draft. Editing recalled text ends history navigation.

On macOS, Command+Left/Right moves to the start/end of the draft, Option+Left/Right moves by word, Option+Backspace deletes a word, and Command+Backspace clears the draft. These shortcuts require the terminal to forward them.

## Attach images

Use Ctrl+V, or Command+V when forwarded, to read the clipboard on the machine running Cocoa. You can also paste or drag a PNG, JPEG, GIF or WebP file path into the draft. An `[Image N]` marker represents each attachment; there is no image preview. Animated images use their first frame.

A message accepts up to four images, each at most 8 MiB and 16,777,216 pixels. Encoded images and text must also fit the [input budget](configuration.md#settings); resize large images if the message exceeds it. Use a model that supports images.

Backspace immediately after a marker, or Delete immediately before it, removes the attachment. Clearing the draft removes all attachments. A remote Cocoa process cannot open a path or clipboard that exists only on your connecting machine.

## Direct ongoing work

While the agent runs, Enter queues a follow-up message. The agent reads it after its current step finishes; it does not interrupt an active tool call. Set `message_steering` to `false` to keep drafts unsent while busy. Cocoa reports messages left undelivered after cancellation or failure.

Press Esc twice within one second to cancel a turn. An open picker consumes Esc first. Ctrl+C clears a nonempty draft; with an empty draft it requests cancellation. A second Ctrl+C within three seconds requests exit.

Type exactly `exit`, `:q` or `:x` to close. Cancellation does not undo file changes or guarantee that remote work and detached processes stopped. Inspect what completed before retrying.

### Recover from provider interruptions

Cocoa automatically retries HTTP 429, 500, 502, 503 and 504 responses up to three times, including requests through ChatGPT. It follows a valid `Retry-After` of at most five minutes; otherwise it waits roughly 1, 2 and 4 seconds with jitter. A longer server-requested delay stops automatic retries rather than retrying too soon. Authorization, explicit quota and invalid-request errors are not retried automatically. Neither are connection failures or interrupted response streams.

If a request still cannot complete, interactive chat shows the affected agent and offers Retry or Cancel. Retry resends only its pending model request, preserving its JavaScript state and completed tool work. Cancel stops the named agent, not its siblings. A paused child remains active even after the parent finishes; its recovery choices stay available. Concurrent interruptions are presented one at a time. One-shot `cocoa exec` does not wait for manual recovery.

A rejected tool call is feedback for the model to correct, not an agent failure. Provider retry and pause notices are separate from individual tool results. Ending or restarting Cocoa does not preserve a paused child's live runtime.

## Read and copy results

Scroll with the mouse wheel or PageUp/PageDown. Click an activity header to expand tool details. Tab moves into activity navigation; arrows select or expand rows, Enter or Space toggles them, and Esc returns to the draft.

Drag to select and copy displayed text. Double-click selects a word; triple-click selects a paragraph. `/copy` copies the latest completed assistant answer, including its Markdown source. Copying requires terminal support for OSC 52 and permission to write the clipboard. Links require terminal hyperlink support and its mouse-reporting bypass gesture.

## Continue a session

Sessions are saved when you first submit a prompt. `/sessions` browses them; `/new` starts an empty conversation. From another launch:

```sh
cocoa resume --last
cocoa sessions search "authentication"
cocoa resume sess_01234567
```

Replace the example ID with a full saved ID or an unambiguous prefix containing at least eight hexadecimal digits. Listing and search default to the current directory; `--all` includes other directories. Search matches a case-insensitive literal substring in titles and root conversation text, not tool output or child conversations.

Resume restores the conversation and shared context, but not live JavaScript state or running child agents. It never replays earlier commands. Changed model, endpoint, account or execution settings can prevent resume. A session already open elsewhere cannot be resumed concurrently.

Resume uses the saved project directory. Use `cocoa resume SESSION --cwd DIRECTORY` to relocate a moved project. Switching away from a nonempty draft requires confirmation. See [data locations](configuration.md#file-locations) and [session flags](cli.md#sessions).

## Rewind a conversation

Use `/rewind` to choose an accepted prompt from the root conversation. Arrow keys select a prompt, Enter creates a new session with the conversation before that prompt, and Esc cancels. The selected prompt, including image attachments, returns to the editor for changes before you submit it. The original session remains available through `/sessions`.

Rewind requires an idle session with no active child agents or local operations. Cocoa asks before discarding an existing draft. Files and other external effects are unchanged. JavaScript bindings and handles reset; shared context and child agents are not copied into the new session. Rewind does not undo commands or replay them.

## Appearance and troubleshooting

- Use `/theme` to preview a palette. Save `theme` in [configuration](configuration.md) to keep it. `--reduced-motion` disables the animated busy indicator.
- If Enter submits instead of adding a newline, use Ctrl+J. Terminal-owned multiline paste can submit when the terminal sends carriage returns; direct clipboard paste avoids that ambiguity.
- If a model rejects a thinking level, omit it or select one supported by that provider and model.
- After a fatal error, follow the reset notice. Use Up to recover your prompt and inspect what completed before retrying.
- If a long conversation fails during compaction, its original history remains available. Check the [compaction settings and provider restrictions](configuration.md#compaction).

Cocoa runs with your user's filesystem and process permissions, without a sandbox or approval prompts. Use trusted configurations, skills and MCP servers. Review changes and avoid exposing secrets through prompts or tool output.
