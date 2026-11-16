# General Coding Guidelines

* Prioritize code correctness and clarity. Speed and efficiency are secondary priorities unless otherwise specified.
* Implementation comments explain WHY, not WHAT. Default to no implementation comment. Add one only when a hidden constraint or workaround would surprise a future reader. Never reference the current task / PR.
* Prefer implementing functionality in existing files unless it is a new logical component. Avoid creating many small files.
* Avoid creative additions unless explicitly requested.
* Do it right the first time. Code is easiest to change while it's hot; don't defer known fixes.
* Code reads top to bottom. Public consts and public fns first, helpers below, deeper helpers below those.

## Documentation

* Document public API contracts with `///`, and crates and modules with `//!`. Explain behavior, input constraints, and relevant type invariants. Skip trivial restatements of names and signatures.
* Document failure conditions in `# Errors` and input-dependent panic conditions in `# Panics`. Documenting a panic does not justify introducing one.
* Include examples where they clarify usage. Examples are optional; do not require one for every item.

## Naming

* Use full words for names, in every language. Write `queue`, not `q`. Allow conventional generic names such as `T` and short local lifetimes such as `'a`; use descriptive names when their roles are not obvious.
* `snake_case` for variables, functions and modules, `CamelCase` for types and traits, `SCREAMING_SNAKE_CASE` for consts.
* Append qualifiers and name big-endian. Write `connection_delay_min_ms`, not `min_connection_delay`.
* Standard Rust method names and ownership conventions take precedence over qualifier ordering. Use `as_` for cheap borrowed views, `to_` for conversions that do work or copy values, and `into_` for consuming conversions. Name getters `foo()` and `foo_mut()`, not `get_foo()`.

## Bounds, asserts and errors

* Put a limit on everything: bound resources, concurrency, loops, and queues. Avoid recursion. Schedule work at fixed intervals rather than reacting to stimuli.
* Assert arguments, returns, and invariants, in the positive and the negative space. Use `assert!`/`debug_assert!` for programmer errors and violated invariants, not malformed external input. Add context explaining why the invariant must hold, rather than repeating the condition.
* Minimize the surface area of interfaces and define fault models. Push control flow up and data flow down.
* Use `Option` for absence or failures that need no explanation, and `Result` for recoverable failures that need context. Propagate errors with `?`. Reserve panics for programmer errors and violated invariants.
* Do not use `unwrap()` or `expect()`. Prefer pattern matching over check-then-unwrap.
* Never silently discard errors with `let _ =` on fallible operations. Always handle errors appropriately:
  - Propagate errors with `?` when the calling function should handle them
  - Use `.log_err()` or similar when you need to ignore errors but want visibility
  - Use explicit error handling with `match` or `if let Err(...)` when you need custom logic
  - Example: avoid `let _ = client.request(...).await?;` - use `client.request(...).await?;` instead
* Return structured error enums, not `String`.

## Types

* Make invalid states unrepresentable. When some fields only exist in one state, put them in an enum variant, not in a set of independent `Option`s.
* A type that validates its input keeps its fields private, builds through a constructor that returns `Result<Self, Error>`, and exposes getters.
* A type that enforces nothing has public fields and no getters. Build it with a struct literal.
* Use fixed-width types (`u32`, `i64`) for data that is stored or sent over the wire. Use `usize` for in-memory lengths and indexes.
* Use newtypes instead of type aliases when values have distinct meanings or could be confused.
* Prefer named fields over multi-field tuples and tuple structs. Single-field newtypes and obvious local uses are exceptions.
* Use enums instead of boolean parameters when the meaning would be unclear at the call site.
* Prefer idiomatic Rust over free helpers. Implement `FromStr`, not `parse_foo`. Implement `Display`, not `format_foo`. Implement `From`/`TryFrom`, not `to_foo_from_bar`.
* Prefer `#[derive(...)]` over manual trait implementations when the derived semantics fit. Add trait implementations as needed in application code.
* If a type provides `len()`, also provide `is_empty()`.
* Use `#[must_use]` on functions or types when ignoring the result would misuse the API. Do not duplicate it on a function whose return type is already marked `#[must_use]`, such as `Result`.
* Never create files with `mod.rs` paths. Write `src/some_module.rs`, not `src/some_module/mod.rs`.

## APIs and control flow

* Keep application code direct. Use concrete parameters, slices, and `Vec` returns when they make the code simpler. Use more flexible library APIs when callers benefit; do not generalize speculatively.
* Prefer inherent methods. Define traits to enable generic programming, dynamic dispatch, or extension APIs, not merely to organize code.
* Use loops when iterator chains obscure branching or mutate external state. Keep short, expressive combinators such as `any` and `find`.
* Add explicit types when `.parse()`, `.into()`, or a complex iterator chain leaves the result type unclear.

## Unsafe code

* Never write unsafe Rust. No exceptions for performance, FFI, or convenience.

## Performance

* Do performance sketches up front: network, storage, memory, compute × bandwidth, latency. Performance is won at design time, not after profiling.
* Batch work: separate the control plane from the data plane, and amortize fixed costs with bigger units of work.
* When borrowing immutable data, accept `&T`, `&str`, and `&[T]` instead of references to owned containers when only their contents are needed.
* Prefer unique ownership and borrowing. Use `Rc` or `Arc` when shared ownership makes the design simpler.
* Avoid copying and (de)serialization in hot paths. Take `&self`, `&str` and `&[T]`, and clone nothing in hot loops. Cold paths may clone when it makes the code simpler.
* Use `Vec::with_capacity(n)` when the size is known.
* Minimize branches at the call site: prefer simpler return types. Define variables near where they are used.
* Use variable shadowing to scope clones in async contexts, so borrowed references live as briefly as possible.

# Useful Scripts

Run the scripts from the project root.
- Run `./scripts/test` for testing. Additional arguments go to nextest.
- Run `./scripts/lint` to check. Additional arguments go to Clippy's Cargo options.
Do not run `cargo test` or `cargo clippy` and `cargo fmt` , use the scripts instead.

# Testing

* Write integration and e2e tests only. Rust tests live under `<crate>/tests/`, and `src/` has no `#[cfg(test)]` modules.
* Use a snapshot assertion only where the exact output is the contract, such as metric names and labels or error details.
* Assert behavior inline, next to its input. For example, assert which item got which result.

# Commits & PRs

Follow CONTRIBUTING.md for commit and PR guidelines
