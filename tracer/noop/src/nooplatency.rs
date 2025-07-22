/*
*
* This library is free software; you can redistribute it and/or
* modify it under the terms of the GNU Library General Public
* License as published by the Free Software Foundation; either
* version 2 of the License, or (at your option) any later version.
 *
 * This library is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * Library General Public License for more details.
 *
 * You should have received a copy of the GNU Library General Public
 * License along with this library; if not, write to the
 * Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
 * Boston, MA 02110-1301, USA.
 */
use glib;
use glib::subclass::prelude::*;
use gobject_sys::GCallback;
use gst::ffi;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gstreamer as gst;
use std::sync::LazyLock;
static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "noop-latency",
        gst::DebugColorFlags::empty(),
        Some("Noop tracer"),
    )
});

// Our Tracer subclass
mod imp {
    use super::*;
    use glib::translate::ToGlibPtr;

    #[derive(Default)]
    pub struct NoopTracer;

    #[glib::object_subclass]
    impl ObjectSubclass for NoopTracer {
        const NAME: &'static str = "nooptracer";
        type Type = super::NoopTracer;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for NoopTracer {
        // Called once when the class is initialized
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let tracer_obj: &gst::Tracer = obj.upcast_ref();

            // Hook callbacks
            unsafe extern "C" fn do_push_buffer_pre(
                _tracer: *mut gst::Tracer,
                _ts: u64,
                ffi_pad: *mut gst::ffi::GstPad,
            ) {
                let pad = gst::Pad::from_glib_ptr_borrow(&ffi_pad);
                gst::debug!(
                    CAT,
                    "noop tracer: do_push_buffer_pre called on {}.{} {}.{}, but noop tracer does nothing",
                    pad.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                    pad.name(),
                    pad.peer().map(|p| p.name()).unwrap_or("unknown".into()),
                    pad.peer().map(|p| p.parent()).flatten().map(|p| p.name()).unwrap_or("unknown".into())
                );
            }

            unsafe extern "C" fn do_pull_range_pre(
                _tracer: *mut gst::Tracer,
                _ts: u64,
                ffi_pad: *mut gst::ffi::GstPad,
            ) {
                let pad = gst::Pad::from_glib_ptr_borrow(&ffi_pad);
                gst::debug!(
                    CAT,
                    "noop tracer: do_pull_range_pre called on {}.{} {}.{}, but noop tracer does nothing",
                    pad.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                    pad.name(),
                    pad.peer().map(|p| p.name()).unwrap_or("unknown".into()),
                    pad.peer().map(|p| p.parent()).flatten().map(|p| p.name()).unwrap_or("unknown".into())
                );
            }

            unsafe extern "C" fn do_push_buffer_post(
                _tracer: *mut gst::Tracer,
                _ts: u64,
                ffi_pad: *mut gst::ffi::GstPad,
            ) {
                let pad = gst::Pad::from_glib_ptr_borrow(&ffi_pad);
                gst::debug!(
                    CAT,
                    "noop tracer: do_push_buffer_post called on {}.{} {}.{}, but noop tracer does nothing",
                    pad.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                    pad.name(),
                    pad.peer().map(|p| p.name()).unwrap_or("unknown".into()),
                    pad.peer().map(|p| p.parent()).flatten().map(|p| p.name()).unwrap_or("unknown".into())
                );
            }

            unsafe extern "C" fn do_pull_range_post(
                _tracer: *mut gst::Tracer,
                _ts: u64,
                ffi_pad: *mut gst::ffi::GstPad,
            ) {
                let pad = gst::Pad::from_glib_ptr_borrow(&ffi_pad);
                gst::debug!(
                    CAT,
                    "noop tracer: do_pull_range_post called on {}.{} {}.{}, but noop tracer does nothing",
                    pad.parent().map(|p| p.name()).unwrap_or("unknown".into()),
                    pad.name(),
                    pad.peer().map(|p| p.name()).unwrap_or("unknown".into()),
                    pad.peer().map(|p| p.parent()).flatten().map(|p| p.name()).unwrap_or("unknown".into())
                );
            }
            unsafe {
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    b"pad-push-pre\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_push_buffer_pre as *const ()),
                );
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    b"pad-push-post\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_push_buffer_post as *const ()),
                );
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    b"pad-pull-range-pre\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_pull_range_pre as *const ()),
                );
                ffi::gst_tracing_register_hook(
                    tracer_obj.to_glib_none().0,
                    b"pad-pull-range-post\0".as_ptr() as *const _,
                    std::mem::transmute::<_, GCallback>(do_pull_range_post as *const ()),
                );
            }
        }
    }

    impl GstObjectImpl for NoopTracer {}
    impl TracerImpl for NoopTracer {}
}

glib::wrapper! {
    pub struct NoopTracer(ObjectSubclass<imp::NoopTracer>)
        @extends gst::Tracer, gst::Object;
}

// Register the plugin with GStreamer
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    // Register the tracer factory
    gst::Tracer::register(Some(plugin), "noop-latency", NoopTracer::static_type())?;

    Ok(())
}
