# Local OpenTelemetry protobuf conversion patch

Source: crates.io `opentelemetry-proto` 0.30.0, Apache-2.0 (see LICENSE).
Released crate SHA-256:
`2e046fd7660710fe5a05e8748e70d9058dc15c94ba914e7c4faa7c728f0e8ddc`.
The package manifest, source, and tests are retained from the released crate.
`src/transform/trace.rs::group_spans_by_resource_and_scope` is the only upstream
production source change: group owned records and convert them by value instead of grouping
references and cloning each SpanData. The OTLP exporter and transport stay intact.

Cargo applies this version-pinned patch through the workspace root manifest.
It covers only the 0.30 crate; review/remove it when upgrading OpenTelemetry.
The original released conversion's scope grouping and resource behavior must
remain unchanged. Run the vendored trace conversion unit tests and the tracer's
collector-backed integration tests when modifying the patch.

The trace unit-test ID constructors are updated to the released 0.30 API (`From`);
the published crate tests still used removed `from_u64`/`from_u128` methods.

From the repository root, use its locked dependency versions for the standalone
conversion tests (the vendored crate is excluded from workspace membership):

```sh
cp Cargo.lock vendor/opentelemetry-proto/Cargo.lock
CARGO_TARGET_DIR="$PWD/target/vendor-tests" cargo test \
  --manifest-path vendor/opentelemetry-proto/Cargo.toml \
  --no-default-features --features gen-tonic-messages,trace --lib transform::trace
```
