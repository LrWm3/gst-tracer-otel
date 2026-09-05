#!/usr/bin/env bash
# Isolated, stream-copy RTSP publishers: no source decode or re-encode.
set -euo pipefail
: "${BENCH_IMAGE:?Set BENCH_IMAGE to the benchmark container image}"
: "${BENCH_PROCESSOR_ROOT:?Set BENCH_PROCESSOR_ROOT to the processor checkout}"
for name in tracer-matched-relay tracer-matched-sources tracer-matched-rtsp; do
  if docker container inspect "$name" >/dev/null 2>&1; then
    echo "Container already exists: $name" >&2
    exit 1
  fi
done
cleanup() {
  docker stop tracer-matched-sources tracer-matched-relay >/dev/null 2>&1 || true
}
trap cleanup EXIT
docker run --rm -d --network host --name tracer-matched-relay \
  -e MTX_RTSPADDRESS=:18554 -e MTX_RTSPTRANSPORTS=tcp \
  bluenviron/mediamtx:latest
sleep 2
docker run --rm -d --network host --name tracer-matched-sources \
  --entrypoint /bin/bash -v /home/user:/host-user \
  "${BENCH_IMAGE}" -lc '
gst-launch-1.0 -q filesrc location=/host-user/exp49-two-stream-660.mp4 \
  ! qtdemux ! h264parse config-interval=-1 ! identity sync=true \
  ! rtspclientsink location=rtsp://127.0.0.1:18554/camera-0 protocols=tcp &
gst-launch-1.0 -q filesrc location=/host-user/exp49-two-stream-660.mp4 \
  ! qtdemux ! h264parse config-interval=-1 ! identity sync=true \
  ! rtspclientsink location=rtsp://127.0.0.1:18554/camera-1 protocols=tcp &
wait
'
sleep 3
if [[ ${DIAG_ONLY:-0} == 1 ]]; then
  # Leave only these named test sources up for an interactive debugger run.
  trap - EXIT
  exit 0
fi
docker run --rm --runtime nvidia --network host --name tracer-matched-rtsp \
  --entrypoint /bin/bash -v /home/user:/host-user \
  -v "${BENCH_PROCESSOR_ROOT}:/pac" -w /pac \
  -e BENCH_PLUGIN_PATH=/pac/rs/target/release \
  "${BENCH_IMAGE}" -lc '
python3 /host-user/tracer-perf-46efcd7/bench_matched.py \
  --root /host-user/tracer-perf-46efcd7 --name rtsp-final \
  --variants off,previous-guard,reviewed-final --rounds 1 --skip-warmup-round \
  --preflight-element nvstreammux --timeout 150 -- \
  python3 rs/rdds/benches/benchmark_two_stream_rtsp.py \
  --uri rtsp://127.0.0.1:18554/camera-0 --uri rtsp://127.0.0.1:18554/camera-1 \
  --engine /pac/outputs/model_b16_gpu0_fp16.engine \
  --infer-config /host-user/exp49-block-linear/timing-rust-capacity-120/config_infer_primary_pelletnet.txt \
  --tracker-config config/artifacts/pellet_ai_pipeline:v87/config_rdsort.yaml \
  --ownership-stage copy --copy-block-linear --decoder-surfaces 55 \
  --copy-surfaces 80 --mux-pool 8 --match-timeout-ms 4000 \
  --runtime-s 90 --warmup-s 10 --queue-sample-ms 100 \
  --output-dir {output} --result {output}.json
'
