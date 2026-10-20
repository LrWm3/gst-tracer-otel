/* Linux benchmark: wall time, source-thread CPU, process CPU, exporter CPU ticks.
 * Drain is outside pipeline timing. Every push is counted by the OTLP receiver. */
#include <gst/gst.h>
#include <dirent.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

typedef struct {
  guint64 start, cpu, buffers, churn_every;
  GstElement *unrelated;
} Stream;
static guint64 cpu_time(clockid_t clock) {
  struct timespec ts;
  clock_gettime(clock, &ts);
  return (guint64) ts.tv_sec * 1000000000 + ts.tv_nsec;
}
static GstPadProbeReturn probe(GstPad *pad, GstPadProbeInfo *info, gpointer data) {
  (void) pad;
  Stream *stream = data;
  if (GST_PAD_PROBE_INFO_TYPE(info) & GST_PAD_PROBE_TYPE_BUFFER) {
    if (!stream->start) stream->start = cpu_time(CLOCK_THREAD_CPUTIME_ID);
    if (stream->churn_every && ++stream->buffers % stream->churn_every == 0) {
      char *name = g_strdup_printf("unrelated-%" G_GUINT64_FORMAT, stream->buffers);
      gst_object_set_name(GST_OBJECT(stream->unrelated), name);
      g_free(name);
    }
  }
  if ((GST_PAD_PROBE_INFO_TYPE(info) & GST_PAD_PROBE_TYPE_EVENT_DOWNSTREAM) &&
      GST_EVENT_TYPE(GST_PAD_PROBE_INFO_EVENT(info)) == GST_EVENT_EOS)
    stream->cpu = cpu_time(CLOCK_THREAD_CPUTIME_ID) - stream->start;
  return GST_PAD_PROBE_OK;
}
static guint64 exporter_ticks(void) {
  DIR *dir = opendir("/proc/self/task");
  if (!dir) return 0;
  struct dirent *entry;
  guint64 total = 0;
  while ((entry = readdir(dir))) {
    char path[512], line[4096];
    snprintf(path, sizeof path, "/proc/self/task/%s/stat", entry->d_name);
    FILE *f = fopen(path, "r");
    if (!f) continue;
    if (fgets(line, sizeof line, f) &&
        (strstr(line, "gst-otel-export") || strstr(line, "OpenTelemetry"))) {
      char *rest = strrchr(line, ')');
      if (rest) {
        char *save, *value = strtok_r(rest + 2, " ", &save);
        for (int field = 3; value; field++, value = strtok_r(NULL, " ", &save))
          if (field == 14 || field == 15) total += g_ascii_strtoull(value, NULL, 10);
      }
    }
    fclose(f);
  }
  closedir(dir);
  return total;
}
int main(int argc, char **argv) {
  gst_init(&argc, &argv);
  if (argc != 2 && argc != 3) return 2;
  int stream_count = argc == 3 ? atoi(argv[2]) : 1;
  if (stream_count < 1 || stream_count > 64) return 2;
  Stream *streams = g_new0(Stream, stream_count);
  const char *churn = g_getenv("OTEL_BENCH_CHURN_EVERY");
  GError *error = NULL;
  GstElement *pipeline = gst_parse_launch(argv[1], &error);
  if (error || !pipeline) return 3;
  for (int i = 0; i < stream_count; i++) {
    streams[i].churn_every = churn ? g_ascii_strtoull(churn, NULL, 10) : 0;
    if (streams[i].churn_every) streams[i].unrelated = gst_element_factory_make("identity", NULL);
    char *name = g_strdup_printf("source%d", i);
    GstElement *source = gst_bin_get_by_name(GST_BIN(pipeline), name);
    g_free(name);
    if (!source) return 3;
    GstPad *src = gst_element_get_static_pad(source, "src");
    gst_pad_add_probe(src, GST_PAD_PROBE_TYPE_BUFFER | GST_PAD_PROBE_TYPE_EVENT_DOWNSTREAM, probe, &streams[i], NULL);
    gst_object_unref(src);
    gst_object_unref(source);
  }
  GstBus *bus = gst_element_get_bus(pipeline);
  guint64 process_start = cpu_time(CLOCK_PROCESS_CPUTIME_ID);
  guint64 start = gst_util_get_timestamp();
  if (gst_element_set_state(pipeline, GST_STATE_PLAYING) == GST_STATE_CHANGE_FAILURE) return 4;
  GstMessage *msg = gst_bus_timed_pop_filtered(bus, 45 * GST_SECOND, GST_MESSAGE_EOS | GST_MESSAGE_ERROR);
  guint64 elapsed = gst_util_get_timestamp() - start;
  int result = (!msg || GST_MESSAGE_TYPE(msg) != GST_MESSAGE_EOS) ? 5 : 0;
  gst_element_set_state(pipeline, GST_STATE_NULL);
  GstElement *sink = gst_bin_get_by_name(GST_BIN(pipeline), "sink0");
  GstPad *pad = sink ? gst_element_get_static_pad(sink, "sink") : NULL;
  int refs = pad ? GST_OBJECT_REFCOUNT_VALUE(pad) : -1;
  if (pad) gst_object_unref(pad);
  if (sink) gst_object_unref(sink);
  /* Also give the old SDK batch processor time to drain for baseline comparisons. */
  const char *drain = g_getenv("OTEL_BENCH_DRAIN_MS");
  g_usleep(drain ? g_ascii_strtoull(drain, NULL, 10) * 1000 : 2000000);
  guint64 stream_cpu = 0;
  for (int i = 0; i < stream_count; i++) {
    stream_cpu += streams[i].cpu;
    gst_clear_object(&streams[i].unrelated);
  }
  g_free(streams);
  printf("RESULT elapsed_ns=%" G_GUINT64_FORMAT " stream_cpu_ns=%" G_GUINT64_FORMAT
         " process_cpu_ns=%" G_GUINT64_FORMAT " exporter_cpu_ns=%" G_GUINT64_FORMAT
         " sink_pad_refs=%d status=%d\n", elapsed, stream_cpu,
         cpu_time(CLOCK_PROCESS_CPUTIME_ID) - process_start,
         exporter_ticks() * 1000000000 / sysconf(_SC_CLK_TCK), refs, result);
  GList *tracers = gst_tracing_get_active_tracers();
  for (GList *item = tracers; item; item = item->next) {
    if (g_object_class_find_property(G_OBJECT_GET_CLASS(item->data), "stats")) {
      GstStructure *stats = NULL;
      g_object_get(item->data, "stats", &stats, NULL);
      if (stats) {
        char *text = gst_structure_to_string(stats);
        printf("COUNTERS %s\n", text);
        g_free(text);
        gst_structure_free(stats);
      }
    }
  }
  g_list_free_full(tracers, gst_object_unref);
  if (msg) gst_message_unref(msg);
  gst_object_unref(bus);
  gst_object_unref(pipeline);
  return result;
}
