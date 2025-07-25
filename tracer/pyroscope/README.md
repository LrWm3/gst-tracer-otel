# Pyroscope Tracer for GStreamer

This crate provides a GStreamer tracer that sends profiling data to a Pyroscope server.

## Kinda works

For this to be useful, you must have debug symbols for all of your gstreamer dependencies.

I am still trying to figure out how to do this myself; currently by going back and forth between perf & then installing missing dbgsym packages & rebuilding the world.

If anyone has suggestions on how to more readily achieve this I would be open to learning.

## Usage

The following environment variables are used to configure the tracer:

| Variable                              | Description                                                                                                                             | Default                 |
| ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- | ----------------------- |
| `GST_PYROSCOPE_SERVER_URL`            | The URL of the Pyroscope server to send profiling data to.                                                                              | `http://localhost:4040` |
| `GST_PYROSCOPE_TRACER_NAME`           | The name of the tracer. This is used to identify the tracer in the Pyroscope UI.                                                        | `gstreamer`             |
| `GST_PYROSCOPE_SAMPLE_RATE`           | The sample rate in hz for the tracer. This controls how often profiling data is sent to the server.                                     | `100`                   |
| `GST_PYROSCOPE_STOP_AGENT_ON_DISPOSE` | Whether to stop the Pyroscope agent when the tracer is disposed. Stopping the agent can take up to 2 minutes.                           | `true`                  |
| `GST_PYROSCOPE_TAGS`                  | Additional tags to add to the profiling data. This can be used to add custom metadata to the profiling data. Specified as 'k1=v1,k2=v2' | ``                      |

## Test locally

First build the plugin, as usual.

Then run the following commands to set up a local Pyroscope server and Grafana instance:

```bash
export GST_PYROSCOPE_SERVER_URL=http://localhost:4040

# grafana as our UI
# 4137 is grpc & 4318 is http otel, which we don't really need here, but kept for consistency
docker run -p 3000:3000 -p 4040:4040 -p 4317:4317 -p 4318:4318 -d grafana/otel-lgtm
```

Finally, run the following command to start the tracer:

```bash
GST_PLUGIN_PATH=target/release:target/debug/ GST_TRACERS='pyroscope(flags=element)' GST_DEBUG=GST_TRACER:5,pyroscope:6 \
gst-launch-1.0 videotestsrc ! videoconvert ! autovideosink
```

You can then access Grafana at `http://localhost:3000` and see the profiling data.
