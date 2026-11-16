# Security policy

## Supported versions

Security fixes target the latest stable release. Older releases are not supported.
Until the first stable release, reports against the latest preview are handled on a best-effort basis.

## Report a vulnerability

Use [GitHub's private vulnerability reporting form](https://github.com/ezegros/cocoa/security/advisories/new).
Do not disclose an unpatched vulnerability in a public issue, discussion or pull request.

If the private form is unavailable, open an issue requesting a private reporting channel without describing the vulnerability.
Wait for a maintainer to provide a private channel before sharing details.

Include the following in your private report:

- Cocoa version or commit, operating system and affected component.
- A description of the impact and any prerequisites.
- Minimal reproduction steps using synthetic data where possible.

Remove API keys, OAuth tokens, private code and personal information from attachments.
Raw sessions, exports, logs and tool output can contain sensitive information; review them before sharing.
Do not send live credentials as proof of a vulnerability.

Maintainers will assess the report and coordinate disclosure with the reporter.
There is no guaranteed response or fix timeline.

## Security boundaries

Cocoa is intended for trusted projects and tools. It can edit files and execute commands with your user permissions, without approval prompts or a sandbox.
AGENTS.md files and skills guide the agent; they are not access controls. Cancellation does not roll back completed actions.

Saved credentials are plaintext files protected by owner-only Unix permissions, not encryption.
Other processes running as you, including the agent's shell commands, can access them.
See [credential storage](docs/configuration.md#credentials).

Hosted model providers receive content sent for inference. Configured MCP servers receive tool inputs and their configured authentication credentials.
Built-in web search is enabled by default, and queries can include sensitive text selected from your task.
Use trusted services and review the [MCP and search configuration](docs/customization.md).

These documented limitations do not rule out vulnerabilities in authentication, credential handling or other promised security boundaries.
