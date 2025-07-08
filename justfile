# Setup environment.
setup:
  ./.devcontainer/setup.sh

# Remove build artifacts.
clean:
  cargo clean

lint:
  cargo clippy --all-features -- -D warnings

# Build and test commands for the project.
build profile="test" target="x86_64-unknown-linux-gnu":
  cargo build --profile {{profile}} --target {{target}}

# Run the tests, or run a specific test if provided.
test test="":
  cargo test {{test}}

# Run tests with address sanitizer enabled, or provide a specific test name to run it against that.
test-address-sanitizer test="given_basic_pipeline_when_run_then_metrics_captured" target="x86_64-unknown-linux-gnu":
  RUST_BACKTRACE=1 LSAN_OPTIONS="suppressions=.github/sanitizer/lsan.supp" RUSTFLAGS="-Z sanitizer=address" cargo +nightly test {{test}} --profile test --target {{target}} 2>&1

# Test the CI workflow using `act`.
test-ci:
  act --workflows ".github/workflows/ci.yaml" --secret-file "" --var-file "" --input-file "" --eventpath ""
