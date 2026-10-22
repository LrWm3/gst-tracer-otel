# mars-noop

A simple test plugin that does not do anything useful; I use this to test the behavior of specific GStreamer element
hooks; this was before I realized `log` does roughly the same thing.

However, I am keeping it around for now as a basic template and for testing purposes.

## Building

```bash
just build-package gst-noop-tracer
```

## Usage

```sh
GST_PLUGIN_PATH=target/release GST_TRACERS=mars-noop \
  gst-launch-1.0 fakesrc num-buffers=10 ! fakesink
```
