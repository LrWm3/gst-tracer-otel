#!/usr/bin/env bash
# Run on an integrated memory nvidia device after staging sources and the three release plugin variants.
set -euo pipefail
: "${BENCH_IMAGE:?Set BENCH_IMAGE to the benchmark container image}"
: "${BENCH_PROCESSOR_ROOT:?Set BENCH_PROCESSOR_ROOT to the processor checkout}"
: "${BENCH_PLATFORM:?Set BENCH_PLATFORM to the external benchmark platform selector}"
docker run --rm --runtime nvidia --network host --name tracer-matched-capacity \
  --entrypoint /bin/bash \
  -v /home/user:/host-user \
  -v "${BENCH_PROCESSOR_ROOT}:/pac" -w /pac \
  -e BENCH_PLATFORM="$BENCH_PLATFORM" \
  -e BENCH_PLUGIN_PATH=/pac/rs/target/release \
  -e BENCH_NAME="${BENCH_NAME:-capacity-final}" \
  -e BENCH_VARIANTS="${BENCH_VARIANTS:-off,baseline,previous-guard,reviewed-final}" \
  -e BENCH_ROUNDS="${BENCH_ROUNDS:-2}" \
  "${BENCH_IMAGE}" -lc '
python3 /host-user/tracer-perf-46efcd7/bench_matched.py \
  --root /host-user/tracer-perf-46efcd7 --name "$BENCH_NAME" \
  --variants "$BENCH_VARIANTS" --rounds "$BENCH_ROUNDS" --skip-warmup-round \
  --preflight-element nvstreammux \
  --timeout 150 -- \
  python3 rs/rdds/benches/benchmark_timing.py \
  --candidate rust --scope full --platform "$BENCH_PLATFORM" \
  --uri /host-user/test_live_stream.mp4 \
  --engine-file /pac/outputs/model_b16_gpu0_fp16.engine \
  --decoder-extra-surfaces 55 --decoder-native --pre-infer-copy \
  --copy-output-buffers 80 --copy-memory-type 0 --copy-block-linear \
  --nv12-transform --slot-rgba-metrics --async-depth 3 \
  --mux-buffer-pool-size 4 --runtime-s 60 --warmup-s 10 \
  --output {output}.json
'
