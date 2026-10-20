#!/usr/bin/env python3
"""Matched subprocess benchmark; run inside the benchmark container on an integrated memory nvidia device.

Each variant directory contains exactly one libgstprometheustracer.so. Separate
registries prevent GStreamer from silently using another variant's cached path.
Results include child-process CPU, not a projection from elapsed wall time.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import threading
import time
import urllib.error
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--sample-rate", type=int, default=16)
    parser.add_argument("--name", default="synthetic")
    parser.add_argument("--timeout", type=float, default=180)
    parser.add_argument("--variants", default="off,baseline,previous,reviewed")
    parser.add_argument("--skip-warmup-round", action="store_true")
    parser.add_argument("--keep-going", action="store_true")
    parser.add_argument("--preflight-element", default="fakesink")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.sample_rate < 1:
        parser.error("--sample-rate must be positive")
    variants = args.variants.split(",")
    command = args.command or [
        "gst-launch-1.0", "-q", "fakesrc", "num-buffers=1000000", "!",
        "identity", "name=work", "!", "fakesink", "sync=false",
    ]
    if command[0] == "--":
        command = command[1:]
    records = []
    for round_id in range(args.rounds + 1):
        if round_id == 0 and args.skip_warmup_round:
            continue
        # Warm each configuration once; rotate order to distribute time drift.
        order = variants[round_id % len(variants):] + variants[:round_id % len(variants)]
        for variant in order:
            run_id = f"{args.name}-{round_id}-{variant}"
            env = os.environ.copy()
            plugin = args.root / variant / "libgstprometheustracer.so"
            env.update(
                GST_TRACERS="" if variant == "off" else "prom-latency(port=19092)",
                GST_PLUGIN_PATH=f"{args.root / variant}:" + env.get("BENCH_PLUGIN_PATH", ""),
                GST_REGISTRY=str(args.root / f"registry-{args.name}-{variant}.bin"),
                GST_PROM_LATENCY_INCLUDE="*",
                GST_PROM_LATENCY_HISTOGRAM_SAMPLE_RATE=str(args.sample_rate),
                GST_PROM_LATENCY_PTS_AGE="false",
                GST_PROM_LATENCY_QUEUE_SAMPLE_INTERVAL_MS="100",
                GST_DEBUG="0",
            )
            output = args.root / run_id
            child_command = [arg.replace("{output}", str(output)) for arg in command]
            # Populate the registry in the same runtime (including NVIDIA
            # mounts) without tracing the plugin scanner itself.
            inspection_env = dict(env, GST_TRACERS="")
            subprocess.run(["gst-inspect-1.0", args.preflight_element], env=inspection_env,
                           stdout=subprocess.DEVNULL, check=True, timeout=30)
            if variant != "off":
                inspection = subprocess.check_output(["gst-inspect-1.0", "prom-latency"],
                                                     env=inspection_env, text=True, timeout=30)
                if str(plugin) not in inspection:
                    raise RuntimeError(f"Wrong plugin loaded: {inspection}")
            stop = threading.Event()
            scrapes = []

            def scrape():
                while not stop.wait(1):
                    try:
                        with urllib.request.urlopen("http://127.0.0.1:19092/metrics", timeout=1) as response:
                            payload = response.read()
                        scrapes.append(len(payload))
                    except (OSError, urllib.error.URLError):
                        pass

            thread = threading.Thread(target=scrape)
            if variant != "off":
                thread.start()
            before = resource.getrusage(resource.RUSAGE_CHILDREN)
            started = time.monotonic()
            try:
                with output.with_suffix(".log").open("w") as log:
                    result = subprocess.run(child_command, env=env, stdout=log, stderr=subprocess.STDOUT, timeout=args.timeout)
                elapsed = time.monotonic() - started
                after = resource.getrusage(resource.RUSAGE_CHILDREN)
            finally:
                stop.set()
                if thread.is_alive():
                    thread.join()
            record = dict(
                variant=variant, round=round_id, warmup=round_id == 0,
                histogram_sample_rate=args.sample_rate,
                wall_s=elapsed, cpu_s=after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime,
                returncode=result.returncode, scrapes=len(scrapes),
                max_scrape_bytes=max(scrapes, default=0), command=child_command,
                plugin_sha256=hashlib.sha256(plugin.read_bytes()).hexdigest() if variant != "off" else None,
            )
            records.append(record)
            (args.root / f"{args.name}-results.json").write_text(json.dumps(records, indent=2))
            print(json.dumps(record), flush=True)
            if result.returncode and not args.keep_going:
                raise SystemExit(f"Failed: {run_id}; see log")


if __name__ == "__main__":
    main()
