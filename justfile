default:
  @just --list

# Setup local development environment.
setup:
  ./.devcontainer/setup.sh

# Remove build artifacts.
clean:
  cargo clean

# Build the project with a specific profile and target.
build profile="test" target="x86_64-unknown-linux-gnu":
  cargo build --profile "{{profile}}" --target "{{target}}"

# Format and lint the codebase.
[group('lint')]
lint:
  cargo fmt --all -- --check
  cargo clippy --all-features -- -D warnings

# Run security audits on the project dependencies.
[group('lint')]
audit:
  cargo audit

# Run tests with coverage analysis.
[group('lint')]
coverage:
  cargo build --profile test
  cargo tarpaulin --skip-clean --out Html

# Run the tests, or run a specific test if provided.
[group('test')]
test test="":
  cargo build --profile test
  cargo test "{{test}}"

# Run tests with address sanitizer enabled, or provide a specific test name to run it against that.
[group('test')]
[group('asan')]
test-address-sanitizer test="given_basic_pipeline_when_run_then_metrics_captured" target="x86_64-unknown-linux-gnu":
  cargo +nightly build --profile test --target "{{target}}"
  RUST_BACKTRACE=1 LSAN_OPTIONS="suppressions=$(pwd)/.github/sanitizer/lsan.supp" RUSTFLAGS="-Z sanitizer=address" cargo +nightly test "{{test}}" --profile test --target "{{target}}" 2>&1

# Test the .github CI workflow using `act`.
[group('test')]
test-ci:
  act --workflows ".github/workflows/ci.yaml" --secret-file "" --var-file "" --input-file "" --eventpath ""

# Run benchmarks and profile them using `perf`.
# This will create a directory `target/bench/perf` and store the profiling data there.
# The `perf_opts` variable can be used to pass additional options to `perf record`.
[doc('`perf benchmark` any `tests` prefixed with `bench_`')]
[group('test')]
[group('perf')]
bench-perf perf_opts="":
    mkdir -p target/bench/perf
    cargo build --profile "profiling"
    # Find all test executables without running them and then profile each one.
    branch_name=$(git rev-parse --abbrev-ref HEAD); \
    for test_executable in $(cargo test --profile profiling --no-run --message-format=json | jq -r 'select(.profile.test == true) | .filenames[]'); do \
        for bench in $($test_executable --list | grep ::bench | awk -F'::' '{print $NF}' | awk -F':' '{print $1}' | sort -u); do \
          perf record {{perf_opts}} -o "target/bench/perf/$bench.$branch_name.data" $test_executable "$bench"; \
          perf report -i "target/bench/perf/$bench.$branch_name.data" >> "target/bench/perf/$bench.$branch_name.txt"; \
        done \
    done


# Similar to `bench-perf`, but uses `--call-graph dwarf` for profiling.
# This is useful for more detailed profiling information, especially for understanding call stacks.
# Check out the 'target/bench/perf/*.txt' file this produces for insights into where time is being spent in benchmarks.
[doc('Benchmark perf using `--call-graph dwarf`')]
[group('test')]
[group('perf')]
bench-perf-dwarf:
    just bench-perf "--call-graph dwarf"

# Compare the performance of two benchmarks using `perf diff`.
# This will compare the performance of the benchmark `test` between the current branch and the main branch.
# It requires that the benchmarks have been run and the data files are available in `.github/perf/` and `target/bench/perf/`.
# The `test` parameter should be the name of the benchmark to compare.
# Example usage: `just bench-perf-diff test="bench_prom_latency_through_pipeline"`
# This will compare the performance of the benchmark `bench_prom_latency_through_pipeline` between the current branch and the main branch.
[doc('Compare performance of a new benchmark using `perf diff` against a saved baseline')]
[group('test')]
[group('perf')]
bench-perf-diff test="bench_prom_latency_through_pipeline":
    just bench-perf
    perf diff --dsos libgstoteltracer.so --dsos libgstprometheustracer.so .github/perf/{{test}}.*.data target/bench/perf/{{test}}.*.data

# Promotes a benchmark to the `.github/perf/` directory, allowing it to be used in performance comparisons.
# This is intended to be run from main or for release tags.
[doc('Promote a benchmark to the `.github/perf/` directory')]
[group('test')]
[group('perf')]
bench-perf-update:
    just bench-perf
    cp target/bench/perf/*.data .github/perf/
