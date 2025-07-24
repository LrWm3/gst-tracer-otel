# Pyroscope Tracer for GStreamer

This crate provides a GStreamer tracer that sends profiling data to a Pyroscope server.

## Proof of concept only

This is a proof of concept implementation of a GStreamer tracer that sends profiling data to a Pyroscope server.

I was unhappy with the results and am not planning on developing this further at the moment. In particular, stack traces
don't have the element names in them, so you can't see which elements are consuming the most CPU time.

## Usage

The following environment variables are used to configure the tracer:

| Variable                              | Description                                                                                                                             | Default                 |
| ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- | ----------------------- |
| `GST_PYROSCOPE_SERVER_URL`            | The URL of the Pyroscope server to send profiling data to.                                                                              | `http://localhost:4040` |
| `GST_PYROSCOPE_TRACER_NAME`           | The name of the tracer. This is used to identify the tracer in the Pyroscope UI.                                                        | `gstreamer`             |
| `GST_PYROSCOPE_SAMPLE_RATE`           | The sample rate in hz for the tracer. This controls how often profiling data is sent to the server.                                     | `100`                   |
| `GST_PYROSCOPE_STOP_AGENT_ON_DISPOSE` | Whether to stop the Pyroscope agent when the tracer is disposed.                                                                        | `true`                  |
| `GST_PYROSCOPE_TAGS`                  | Additional tags to add to the profiling data. This can be used to add custom metadata to the profiling data. Specified as 'k1=v1,k2=v2' | ``                      |

## Test locally

First build the plugin, as usual.

Then run the following commands to set up a local Pyroscope server and Grafana instance:

```bash
docker network create pyroscope-demo
docker run --name pyroscope --network=pyroscope-demo -d -p 4040:4040 grafana/pyroscope
docker run -d --name=grafana   --network=pyroscope-demo   -p 3000:3000   -e "GF_INSTALL_PLUGINS=grafana-pyroscope-app"  -e "GF_AUTH_ANONYMOUS_ENABLED=true"   -e "GF_AUTH_ANONYMOUS_ORG_ROLE=Admin"   -e "GF_AUTH_DISABLE_LOGIN_FORM=true"   grafana/grafana:main
```

Finally, run the following command to start the tracer:

```bash
GST_PLUGIN_PATH=target/debug/ GST_TRACERS='pyroscope(flags=element)' GST_DEBUG=GST_TRACER:5 \
gst-launch-1.0 videotestsrc ! videoconvert ! autovideosink
```

You can then access the Pyroscope UI at `http://localhost:4040` and Grafana at `http://localhost:3000` and see the profiling data.
