# Contributing to Cocoa

## Build from source

Cocoa development targets macOS and Linux. Install [rustup](https://rustup.rs), Bash and the native build tools:

- macOS: Xcode command-line tools and CMake.
- Debian/Ubuntu: `build-essential`, `cmake` and `pkg-config`.

From the workspace root:

```sh
rustup toolchain install
cargo build --locked -p cocoa --bin cocoa
```

The [pinned toolchain](rust-toolchain.toml) includes rustfmt and Clippy. To install the executable locally:

```sh
cargo install --locked --path crates/cocoa --bin cocoa
```

## Run checks

Install the check tools at the versions used in CI:

```sh
cargo install cargo-nextest --version 0.9.143 --locked
cargo install cargo-deny --version 0.20.2 --locked
```

Run from the workspace root:

```sh
./scripts/lint
./scripts/test
./scripts/test --doc
./scripts/audit
```

These check formatting, Clippy, integration tests, doctests and dependency policy. Tests need no provider credentials. Pass nextest arguments to select tests:

```sh
./scripts/test -p cocoa-core
./scripts/test -E 'test(alias)'
```

Use `./scripts/test --profile ci` to match CI's test runner settings. The audit requires network access and includes advisory exceptions listed in [deny.toml](deny.toml); a passing result does not mean those dependencies are fixed.

## Try the local mock

Run `cargo run --locked -p cocoa --example mock_llm` in one terminal. In another, from the project root:

```sh
cargo build --locked -p cocoa --bin cocoa
mock_home="$(mktemp -d)"
mkdir -p "$mock_home/.config/cocoa"
cat > "$mock_home/.config/cocoa/config.json" <<'EOF'
{
  "providers": { "ollama": { "endpoint": "http://127.0.0.1:8787/v1/chat/completions" } },
  "aliases": { "mock": [{ "provider": "ollama", "model": "mock" }] },
  "default_alias": "mock"
}
EOF
HOME="$mock_home" XDG_DATA_HOME="$mock_home/.local/share" ./target/debug/cocoa
```

Try `Calculate 7 * 3 + 2`, then another prompt to exercise retained JavaScript state. The fixture uses loopback HTTP and the real QuickJS runtime without a provider account. Stop the server with Ctrl+C when finished. The temporary home keeps mock configuration separate from your own.

## Changes and pull requests

Report security vulnerabilities through the private route in [SECURITY.md](SECURITY.md), not in public issues or pull requests.

- Keep changes focused and follow [AGENTS.md](AGENTS.md). Put integration tests under each crate's `tests/` directory.
- Keep `Cargo.lock` checked in; update dependencies deliberately and review the lockfile diff.
- Keep credentials and local session data out of commits.
- Document what readers need to use, configure, extend or contribute to Cocoa. Explain architecture at the component level; leave implementation details beside the code.
- Keep internal research, proposals, release runbooks and working notes in the ignored `.amp/in/` directory. Do not commit them or link public pages to them.
- Pull requests should explain why the change is needed, list checks run and disclose failures or platform limitations. Follow the commit and pull request title conventions below.

Binary distributions must include the required dependency notices and licenses.

## Commit messages and pull request titles

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) for commit subjects and pull request titles:

```text
type(scope): imperative description
```

The scope is optional. Use a component or area such as `cli`, `auth` or `mcp` when it helps identify what changed.

- `feat` introduces functionality.
- `fix` corrects a bug.
- `perf` improves performance.
- `refactor` changes code without adding functionality or fixing a bug.
- `docs`, `test`, `build` and `ci` cover documentation, tests, build tooling and continuous integration respectively.
- `chore` covers maintenance that does not fit the other types. Do not use it for user-facing features or fixes.

Write subjects that can stand alone in release notes. Describe the behavior that changed rather than implementation activity such as "update code" or "address review comments". For example:

```text
feat(cli): add JSON output for one-shot execution
fix(auth): preserve credentials after a failed refresh
```

Mark a breaking change with `!` before the colon or a `BREAKING CHANGE:` footer. Explain the incompatible behavior and required migration in the commit body and pull request description.

Use the same convention for pull request titles so squash commits preserve it. Before merging, check that the final commit subject describes the complete change, not an intermediate implementation step.

The release workflow currently uses GitHub-generated release notes, which summarize merged pull requests rather than parse Conventional Commits. Keep pull request titles suitable for those notes. Automatic grouping or versioning by commit type requires separate tooling.
