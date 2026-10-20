#!/usr/bin/env python3
"""Build and test with ASan, loading the exact cdylib reported by Cargo."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default="x86_64-unknown-linux-gnu")
    parser.add_argument("--test", default="")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    env = os.environ.copy()
    env.pop("CARGO_ENCODED_RUSTFLAGS", None)
    env["RUSTFLAGS"] = env.get("RUSTFLAGS", "") + " -Zsanitizer=address -C link-arg=-Wl,--export-dynamic"
    env["GST_REGISTRY_FORK"] = "no"
    env.setdefault("LSAN_OPTIONS", f"suppressions={root / '.github/sanitizer/lsan.supp'}")
    common = ["-p", "gst-prometheus-tracer", "--target", args.target]
    build = subprocess.run(
        ["cargo", "+nightly", "build", *common, "--message-format=json"],
        cwd=root, env=env, stdout=subprocess.PIPE, text=True, check=True,
    )
    artifacts = [json.loads(line) for line in build.stdout.splitlines()]
    plugins = [
        path
        for artifact in artifacts
        if artifact.get("reason") == "compiler-artifact"
        and artifact["target"]["name"] == "gstprometheustracer"
        for path in artifact["filenames"]
        if Path(path).suffix in (".so", ".dylib", ".dll")
    ]
    if len(plugins) != 1:
        raise RuntimeError(f"Expected one tracer cdylib, got {plugins}")
    # Scan only the core elements used by this suite. Loading unrelated system
    # plugins in-process introduces their startup allocations into LeakSanitizer.
    plugins_dir = Path(subprocess.check_output(
        ["pkg-config", "--variable=pluginsdir", "gstreamer-1.0"], text=True,
    ).strip())
    with tempfile.TemporaryDirectory(prefix="gst-prom-asan-") as directory:
        stage = Path(directory)
        plugin = stage / Path(plugins[0]).name
        plugin.symlink_to(plugins[0])
        core = plugins_dir / "libgstcoreelements.so"
        (stage / core.name).symlink_to(core)
        env["GST_PLUGIN_SYSTEM_PATH_1_0"] = str(stage)
        env["GST_PLUGIN_PATH"] = str(stage)
        env["GST_PROM_TEST_PLUGIN"] = str(plugin)
        result = subprocess.run(
            ["cargo", "+nightly", "test", *common, args.test], cwd=root, env=env,
        )
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
