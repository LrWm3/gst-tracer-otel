# AGENTS

## Development

Before submitting changes, run the following commands or the appropriate `just` recipes:

- `cargo fmt --all`
- `cargo clippy --workspace --all-features -- -D warnings`
- `cargo test`
- `cargo audit`
- `cargo +nightly udeps -p gst-otel-tracer -p gst-prometheus-tracer -p gst-pyroscope-tracer --all-targets`

These commands keep the code formatted, linted, tested, and check for security and dependency issues.

## Style

- Code should be formatted with `cargo fmt --all` and free of clippy warnings.
- Follow commit messages in the form `<type>(<component>): <summary>`, where `type` is one of `doc`, `impl`, `fix`, or `test`.
- Include tests for new features and changes when possible.

## Pull Requests

- Reference any relevant issues.
- Keep pull requests focused; prefer small, clear changes.
- Ensure documentation is updated alongside code when needed.

