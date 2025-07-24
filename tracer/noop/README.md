# noop-latency

A simple test plugin that does not do anything useful; I use this to test the behavior of specific GStreamer element
hooks; this was before I realized `log` does roughly the same thing.

However, I am keeping it around for now as a basic template and for testing purposes.

## Building

```bash
just build-package noop-latency
```

## Usage

Same as the others.
