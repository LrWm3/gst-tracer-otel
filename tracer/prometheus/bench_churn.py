#!/usr/bin/env python3
"""Stream a fixed buffer count while a second thread creates/destroys graphs.

Use as the child command of bench_matched.py with the system Python and PyGObject.
"""
import argparse
import json
import resource
import threading
import time

import gi

gi.require_version("Gst", "1.0")
from gi.repository import Gst


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--buffers", type=int, default=1_000_000)
    parser.add_argument("--changes", type=int, default=200)
    args = parser.parse_args()
    Gst.init(None)
    stream = Gst.parse_launch(
        f"fakesrc num-buffers={args.buffers} ! identity ! fakesink sync=false"
    )
    failures = []

    def churn():
        try:
            for index in range(args.changes):
                graph = Gst.parse_launch("fakesrc num-buffers=1 ! identity ! fakesink")
                graph.set_name(f"churn_{index}")
                try:
                    graph.set_state(Gst.State.PLAYING)
                    message = graph.get_bus().timed_pop_filtered(
                        5 * Gst.SECOND, Gst.MessageType.EOS | Gst.MessageType.ERROR
                    )
                    if message is None or message.type == Gst.MessageType.ERROR:
                        raise RuntimeError("churn pipeline failed or timed out")
                finally:
                    graph.set_state(Gst.State.NULL)
                del graph
                time.sleep(0.005)
        except Exception as error:
            failures.append(str(error))

    worker = threading.Thread(target=churn)
    started = time.monotonic()
    try:
        stream.set_state(Gst.State.PLAYING)
        worker.start()
        message = stream.get_bus().timed_pop_filtered(
            30 * Gst.SECOND, Gst.MessageType.EOS | Gst.MessageType.ERROR
        )
        elapsed = time.monotonic() - started
        if message is None or message.type == Gst.MessageType.ERROR:
            raise RuntimeError("streaming pipeline failed or timed out")
    finally:
        stream.set_state(Gst.State.NULL)
        if worker.ident is not None:
            worker.join()
    if failures:
        raise RuntimeError(failures)
    tracers = Gst.tracing_get_active_tracers()
    metrics = next((tracer.emit("metrics") for tracer in tracers if tracer.__gtype__.name == "promlatencytracer"), "")
    print(json.dumps(dict(
        buffers=args.buffers, graph_changes=args.changes, stream_wall_s=elapsed,
        peak_rss_kib=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
        retired_graph_series=sum("churn_" in line for line in metrics.splitlines()),
    )))


if __name__ == "__main__":
    main()
