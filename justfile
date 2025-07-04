# Setup
setup:
  ./.devcontainer/setup.sh

# Build and test commands for the project.
build PROFILE="test":
  cargo build --profile {{PROFILE}}

# Run the tests, or run a specific test if provided.
test TEST="": 
  cargo test {{TEST}}

# Run tests with address sanitizer enabled, or provide a specific test name to run it against that.
test-address-sanitizer TEST="given_basic_pipeline_when_run_then_metrics_captured": 
  RUST_BACKTRACE=1 LSAN_OPTIONS="suppressions=.github/sanitizer/lsan.supp" RUSTFLAGS="-Z sanitizer=address" cargo +nightly test {{TEST}} --target x86_64-unknown-linux-gnu 2>&1

# Test the CI workflow using `act`.
test-ci:
  act --workflows ".github/workflows/ci.yaml" --secret-file "" --var-file "" --input-file "" --eventpath ""
