"""Investigate observable queue delay; this is not a production queue collector.

Run with /usr/bin/python3 (PyGObject). Synthetic (generation, PTS) keys are
unique in this workload only. No buffer pointers or references are retained.
"""
import argparse
import json
from pathlib import Path
import statistics
import threading
import time
import gi

gi.require_version('Gst', '1.0')
from gi.repository import Gst


def run(leaky, flush):
    pipeline = Gst.parse_launch(
        f'queue name=q max-size-buffers=3 max-size-bytes=0 max-size-time=0 leaky={leaky} ! '
        'identity sleep-time=10000 ! fakesink sync=false async=false')
    queue = pipeline.get_by_name('q')
    source = Gst.Pad.new('synthetic-source', Gst.PadDirection.SRC)
    assert source.set_active(True)
    assert source.link(queue.get_static_pad('sink')) == Gst.PadLinkReturn.OK
    generation = 0
    pending = {}
    samples = []
    calls = []
    flushed = 0
    unknown = 0
    flush_events = []

    def output(_pad, info):
        nonlocal unknown
        now = time.monotonic_ns()
        key = (generation, info.get_buffer().pts)
        start = pending.pop(key, None)
        if start is None:
            unknown += 1
        else:
            samples.append(dict(generation=generation, pts=key[1], arrival_to_output_ns=now-start))
        return Gst.PadProbeReturn.OK

    def events(_pad, info):
        event = info.get_event()
        if event.type in (Gst.EventType.FLUSH_START, Gst.EventType.FLUSH_STOP):
            flush_events.append(event.type.value_nick)
        return Gst.PadProbeReturn.OK

    queue.get_static_pad('src').add_probe(Gst.PadProbeType.BUFFER, output)
    queue.get_static_pad('sink').add_probe(Gst.PadProbeType.EVENT_DOWNSTREAM | Gst.PadProbeType.EVENT_FLUSH, events)
    assert pipeline.set_state(Gst.State.PLAYING) != Gst.StateChangeReturn.FAILURE
    assert source.push_event(Gst.Event.new_stream_start('synthetic'))
    assert source.push_event(Gst.Event.new_caps(Gst.Caps.from_string('application/octet-stream')))

    def segment():
        value = Gst.Segment.new()
        value.init(Gst.Format.TIME)
        assert source.push_event(Gst.Event.new_segment(value))

    segment()
    for index in range(100):
        if flush and index == 50:
            assert source.push_event(Gst.Event.new_flush_start())
            # FLUSH_START has stopped the queue's source task. Advance the
            # matching generation before buffers can flow again.
            flushed += len(pending)
            pending.clear()
            generation += 1
            assert source.push_event(Gst.Event.new_flush_stop(True))
            segment()
        pts = (index % 50 if flush else index) * Gst.MSECOND
        buffer = Gst.Buffer.new_allocate(None, 1, None)
        buffer.pts = pts
        buffer.duration = Gst.MSECOND
        start = time.monotonic_ns()
        pending[(generation, pts)] = start
        result = source.push(buffer)
        end = time.monotonic_ns()
        assert result == Gst.FlowReturn.OK, result
        calls.append(dict(generation=generation, pts=pts, push_ns=end-start))
    assert source.push_event(Gst.Event.new_eos())
    message = pipeline.get_bus().timed_pop_filtered(10*Gst.SECOND, Gst.MessageType.EOS | Gst.MessageType.ERROR)
    assert message is not None and message.type == Gst.MessageType.EOS, message
    discarded = len(pending)
    pending.clear()
    assert pipeline.set_state(Gst.State.NULL) != Gst.StateChangeReturn.FAILURE
    source.unlink(queue.get_static_pad('sink'))
    source.set_active(False)
    assert unknown == 0
    assert len(samples) + discarded + flushed == len(calls)
    return dict(leaky=leaky, flush=flush, inputs=len(calls), outputs=len(samples),
                discarded_without_output=discarded, cleared_by_flush=flushed,
                unknown_outputs=unknown, retained_after_cleanup=len(pending),
                flush_events=flush_events,
                push_median_ms=statistics.median(s['push_ns'] for s in calls)/1e6,
                boundary_median_ms=statistics.median(s['arrival_to_output_ns'] for s in samples)/1e6,
                calls=calls, samples=samples)


def seek_check():
    pipeline = Gst.parse_launch('audiotestsrc num-buffers=100 samplesperbuffer=80 ! queue name=q max-size-buffers=3 max-size-time=0 max-size-bytes=0 ! identity sleep-time=10000 ! fakesink sync=false async=false')
    queue = pipeline.get_by_name('q')
    generation = 0
    pending = {}
    cleared = 0
    inputs = 0
    outputs = []
    ready = threading.Event()
    events = []

    def incoming(_pad, info):
        nonlocal generation, cleared, inputs
        if info.type & Gst.PadProbeType.BUFFER:
            inputs += 1
            pending[(generation, info.get_buffer().pts)] = time.monotonic_ns()
        else:
            event = info.get_event()
            if event.type == Gst.EventType.FLUSH_START:
                events.append('flush-start')
                cleared += len(pending)
                pending.clear()
                generation += 1
            elif event.type == Gst.EventType.FLUSH_STOP:
                events.append('flush-stop')
        return Gst.PadProbeReturn.OK

    def outgoing(_pad, info):
        key = (generation, info.get_buffer().pts)
        start = pending.pop(key, None)
        outputs.append(dict(generation=generation, pts=key[1], matched=start is not None))
        if len(outputs) >= 20:
            ready.set()
        return Gst.PadProbeReturn.OK

    queue.get_static_pad('sink').add_probe(Gst.PadProbeType.BUFFER | Gst.PadProbeType.EVENT_DOWNSTREAM | Gst.PadProbeType.EVENT_FLUSH, incoming)
    queue.get_static_pad('src').add_probe(Gst.PadProbeType.BUFFER, outgoing)
    pipeline.set_state(Gst.State.PLAYING)
    assert ready.wait(5)
    assert pipeline.seek_simple(Gst.Format.TIME, Gst.SeekFlags.FLUSH, 0)
    message = pipeline.get_bus().timed_pop_filtered(10*Gst.SECOND, Gst.MessageType.EOS | Gst.MessageType.ERROR)
    assert message is not None and message.type == Gst.MessageType.EOS
    pipeline.set_state(Gst.State.NULL)
    retained = len(pending)
    pending.clear()
    assert generation == 1 and all(item['matched'] for item in outputs)
    assert {item['pts'] for item in outputs if item['generation'] == 0} & {item['pts'] for item in outputs if item['generation'] == 1}, 'seek should reuse timestamps'
    assert inputs == len(outputs) + cleared + retained
    return dict(inputs=inputs, outputs=len(outputs), cleared_by_flush=cleared, unmatched_at_eos=retained, generations=generation+1, events=events, retained_after_cleanup=len(pending), observations=outputs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    Gst.init(None)
    results = [run(leaky, flush) for leaky, flush in [('no', False), ('upstream', False), ('downstream', False), ('no', True)]]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(dict(gstreamer=Gst.version_string(), results=results, seek=seek_check()), indent=2)+'\n')
    for result in results:
        print({k: v for k, v in result.items() if k not in ('calls', 'samples')})


if __name__ == '__main__':
    main()
