# Pyroscope Tracer for GStreamer

This crate provides a GStreamer tracer that sends profiling data to a Pyroscope server.

## Kinda works

For this to be useful, you must have debug symbols for all of your gstreamer dependencies.

You can try and install them, or resolve them with `debuginfod`, but I was unable to do so easily.

The option which worked for me was to rebuild gstreamer and then link it.

```bash
# clone the repository
git clone https://gitlab.freedesktop.org/gstreamer/gstreamer.git
cd gstreamer

# checkout the tag you want
git checkout 1.24.0

# or wherever this repo is on your filesystem.
cp ../gst-tracer-otel/tracer/pyroscope/gstreamer/native.flags.ini .

# build with native.flags.ini
meson setup builddir -Dtests=disabled -Dexamples=disabled  -Dgpl=enabled --buildtype=debugoptimized -Dstrip=false --native-file native.flags.ini
meson compile -C builddir

# load the environment
./gst-env.py bash

# build this plugin
cd ../gst-tracer-otel && just build-package gst-pyroscope-tracer

# the plugin to your path
export GST_PLUGIN_PATH="$GST_PLUGIN_PATH:$(pwd)/target/release"

# at this point you will have the plugin available and all debug symbols in gstreamer.
```

## Usage

The tracer exposes the following properties, mirroring the previous environment variables:

- `server-url` – URL of the Pyroscope server (**default:** `http://localhost:4040`)
- `tracer-name` – name used to identify the tracer in Pyroscope (**default:** `gst.pyroscope`)
- `sample-rate` – sampling rate in Hz (**default:** `100`)
- `stop-agent-on-dispose` – whether to stop the Pyroscope agent on dispose (**default:** `true`)
- `tags` – additional tags in the form `k1=v1,k2=v2` (**default:** empty)

Enable the tracer with custom properties via `GST_TRACERS`:

```bash
GST_TRACERS='pyroscope(server-url=http://localhost:4040,tracer-name=gst.pyroscope,sample-rate=100,stop-agent-on-dispose=true,tags="k1=v1")'
```

## Test locally

First build the plugin, as usual.

Then run the following commands to set up a local Pyroscope server and Grafana instance:

```bash
# grafana as our UI
# 4137 is grpc & 4318 is http otel, which we don't really need here, but kept for consistency
docker run -p 3000:3000 -p 4040:4040 -p 4317:4317 -p 4318:4318 -d grafana/otel-lgtm
```

Finally, run the following command to start the tracer:

```bash
GST_PLUGIN_PATH=target/release:target/debug/ GST_TRACERS='pyroscope' GST_DEBUG=GST_TRACER:5,pyroscope:6 \
gst-launch-1.0 videotestsrc ! videoconvert ! autovideosink
```

You can then access Grafana at `http://localhost:3000` and see the profiling data.
