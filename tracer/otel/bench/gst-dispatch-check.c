/* Standalone regression check for the optional GStreamer dispatcher patch.
 * Run against both private builds using LD_LIBRARY_PATH. */
#include <gst/gst.h>

typedef struct { GstTracer parent; guint slot; } DispatchTracer;
typedef struct { GstTracerClass parent; } DispatchTracerClass;
G_DEFINE_TYPE(DispatchTracer, dispatch_tracer, GST_TYPE_TRACER)
static void dispatch_tracer_class_init(DispatchTracerClass *klass) { (void) klass; }
static void dispatch_tracer_init(DispatchTracer *self) { (void) self; }

static guint calls[3];
static void created(GstTracer *tracer, guint64 ts, GstMiniObject *object) {
  g_assert_nonnull(object);
  g_assert_cmpuint(ts, >, 0);
  calls[((DispatchTracer *) tracer)->slot]++;
}
static void wildcard(GstTracer *tracer, guint64 ts, ...) {
  (void) tracer;
  g_assert_cmpuint(ts, >, 0);
  calls[2]++;
}

int main(int argc, char **argv) {
  gst_init(&argc, &argv);
  DispatchTracer *tracers[3];
  for (guint i = 0; i < 3; i++) {
    tracers[i] = g_object_new(dispatch_tracer_get_type(), NULL);
    gst_object_ref_sink(tracers[i]);
    tracers[i]->slot = i;
  }

  gst_tracing_register_hook(GST_TRACER(tracers[0]), "mini-object-created", G_CALLBACK(created));
  GstBuffer *buffer = gst_buffer_new();
  g_assert_cmpuint(calls[0], ==, 1);
  g_assert_cmpuint(calls[1], ==, 0);
  gst_buffer_unref(buffer);

  gst_tracing_register_hook(GST_TRACER(tracers[1]), "mini-object-created", G_CALLBACK(created));
  buffer = gst_buffer_new();
  g_assert_cmpuint(calls[0], ==, 2);
  g_assert_cmpuint(calls[1], ==, 1);
  gst_buffer_unref(buffer);

  gst_tracing_register_hook(GST_TRACER(tracers[2]), NULL, G_CALLBACK(wildcard));
  guint before = calls[2];
  buffer = gst_buffer_new();
  g_assert_cmpuint(calls[0], ==, 3);
  g_assert_cmpuint(calls[1], ==, 2);
  g_assert_cmpuint(calls[2], >, before);

  /* No specific listener exists for ref/unref; wildcard still receives them. */
  before = calls[2];
  gst_buffer_ref(buffer);
  g_assert_cmpuint(calls[2], >, before);
  before = calls[2];
  gst_buffer_unref(buffer);
  g_assert_cmpuint(calls[2], >, before);
  gst_buffer_unref(buffer);
  g_assert_cmpuint(calls[0], ==, 3);
  g_assert_cmpuint(calls[1], ==, 2);

  for (guint i = 0; i < 3; i++) gst_object_unref(tracers[i]);
  gst_deinit();
  g_print("Specific, multiple, wildcard callbacks and timestamps passed.\n");
  return 0;
}
