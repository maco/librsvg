//! Rust implementation of the C API for librsvg.
//!
//! The C API of librsvg revolves around an `RsvgHandle` GObject class, which is
//! implemented as follows:
//!
//! * [`RsvgHandle`] and [`RsvgHandleClass`] are derivatives of `GObject` and
//! `GObjectClass`.  These are coded explicitly, instead of using
//! `glib::subclass::simple::InstanceStruct<T>` and
//! `glib::subclass::simple::ClassStruct<T>`, as the structs need need to be kept
//! ABI-compatible with the traditional C API/ABI.
//!
//! * The actual data for a handle (e.g. the `RsvgHandle`'s private data, in GObject
//! parlance) is in [`CHandle`].
//!
//! * Public C ABI functions are the `#[no_mangle]` functions with an `rsvg_` prefix.
//!
//! The C API is implemented in terms of the Rust API in `librsvg_crate`.  In effect,
//! `RsvgHandle` is a rather convoluted builder or adapter pattern that translates all the
//! historical idiosyncrasies of the C API into the simple Rust API.
//!
//! [`RsvgHandle`]: struct.RsvgHandle.html
//! [`RsvgHandleClass`]: struct.RsvgHandleClass.html
//! [`CHandle`]: struct.CHandle.html

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::ffi::{CStr, CString, OsStr};
use std::fmt;
use std::ops;
use std::path::PathBuf;
use std::ptr;
use std::slice;
use std::str;
use std::{f64, i32};

use gdk_pixbuf::Pixbuf;
use gio::prelude::*;
use glib::error::ErrorDomain;
use url::Url;

use glib::object::ObjectClass;
use glib::subclass;
use glib::subclass::object::ObjectClassSubclassExt;
use glib::subclass::prelude::*;
use glib::translate::*;
use glib::{
    glib_object_impl, glib_object_subclass, Bytes, Cast, ParamFlags, ParamSpec, StaticType,
    ToValue, Type,
};

use glib::types::instance_of;

use crate::api::{self, CairoRenderer, IntrinsicDimensions, Loader, LoadingError, SvgHandle};

use crate::{
    length::RsvgLength,
    rsvg_log,
    surface_utils::shared_surface::{SharedImageSurface, SurfaceType},
};

use super::dpi::Dpi;
use super::messages::{rsvg_g_critical, rsvg_g_warning};
use super::pixbuf_utils::{empty_pixbuf, pixbuf_from_surface};
use super::sizing::LegacySize;

include!(concat!(env!("OUT_DIR"), "/version.rs"));

// This is basically the same as api::RenderingError but with extra cases for
// the peculiarities of the C API.
enum RenderingError {
    RenderingError(api::RenderingError),

    // The RsvgHandle is created, but hasn't been loaded yet.
    HandleIsNotLoaded,
}

impl<T: Into<api::RenderingError>> From<T> for RenderingError {
    fn from(e: T) -> RenderingError {
        RenderingError::RenderingError(e.into())
    }
}

impl fmt::Display for RenderingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RenderingError::RenderingError(ref e) => e.fmt(f),
            RenderingError::HandleIsNotLoaded => write!(f, "SVG data is not loaded into handle"),
        }
    }
}

#[glib::gflags("RsvgHandleFlags")]
pub enum HandleFlags {
    #[gflags(name = "RSVG_HANDLE_FLAGS_NONE", nick = "flags-none")]
    NONE = 0,

    #[gflags(name = "RSVG_HANDLE_FLAG_UNLIMITED", nick = "flag-unlimited")]
    UNLIMITED = 1 << 0,

    #[gflags(
        name = "RSVG_HANDLE_FLAG_KEEP_IMAGE_DATA",
        nick = "flag-keep-image-data"
    )]
    KEEP_IMAGE_DATA = 1 << 1,
}

pub type RsvgHandleFlags = u32;

#[derive(Default, Copy, Clone)]
struct LoadFlags {
    unlimited_size: bool,
    keep_image_data: bool,
}

impl From<HandleFlags> for LoadFlags {
    fn from(flags: HandleFlags) -> LoadFlags {
        LoadFlags {
            unlimited_size: flags.contains(HandleFlags::UNLIMITED),
            keep_image_data: flags.contains(HandleFlags::KEEP_IMAGE_DATA),
        }
    }
}

impl From<LoadFlags> for HandleFlags {
    fn from(lflags: LoadFlags) -> HandleFlags {
        let mut hflags = HandleFlags::empty();

        if lflags.unlimited_size {
            hflags.insert(HandleFlags::UNLIMITED);
        }

        if lflags.keep_image_data {
            hflags.insert(HandleFlags::KEEP_IMAGE_DATA);
        }

        hflags
    }
}

/// GObject class struct for RsvgHandle.
///
/// This is not done through `glib::subclass::simple::ClassStruct<T>` because we need
/// to include the `_abi_padding` field for ABI compatibility with the C headers, and
/// `simple::ClassStruct` does not allow that.
#[repr(C)]
pub struct RsvgHandleClass {
    // Keep this in sync with rsvg.h:RsvgHandleClass
    parent: gobject_sys::GObjectClass,

    _abi_padding: [glib_sys::gpointer; 15],
}

/// GObject instance struct for RsvgHandle.
///
/// This is not done through `glib::subclass::simple::InstanceStruct<T>` because we need
/// to include the `_abi_padding` field for ABI compatibility with the C headers, and
/// `simple::InstanceStruct` does not allow that.
#[repr(C)]
pub struct RsvgHandle {
    // Keep this in sync with rsvg.h:RsvgHandle
    parent: gobject_sys::GObject,

    _abi_padding: [glib_sys::gpointer; 16],
}

#[allow(clippy::large_enum_variant)]
enum LoadState {
    // Just created the CHandle
    Start,

    // Being loaded using the legacy write()/close() API
    Loading { buffer: Vec<u8> },

    ClosedOk { handle: SvgHandle },

    ClosedError,
}

impl LoadState {
    fn set_from_loading_result(
        &mut self,
        result: Result<SvgHandle, LoadingError>,
    ) -> Result<(), LoadingError> {
        match result {
            Ok(handle) => {
                *self = LoadState::ClosedOk { handle };
                Ok(())
            }

            Err(e) => {
                *self = LoadState::ClosedError;
                Err(e)
            }
        }
    }
}

/// Holds the base URL for loading a handle, and the C-accessible version of it
///
/// There is a public API to query the base URL, and we need to
/// produce a CString with it.  However, that API returns a
/// *const char, so we need to maintain a long-lived CString along with the
/// internal Url.
#[derive(Default)]
struct BaseUrl {
    inner: Option<BaseUrlInner>,
}

struct BaseUrlInner {
    url: Url,
    cstring: CString,
}

impl BaseUrl {
    fn set(&mut self, url: Url) {
        let cstring = CString::new(url.as_str()).unwrap();

        self.inner = Some(BaseUrlInner { url, cstring });
    }

    fn get(&self) -> Option<&Url> {
        self.inner.as_ref().map(|b| &b.url)
    }

    fn get_gfile(&self) -> Option<gio::File> {
        self.get().map(|url| gio::File::new_for_uri(url.as_str()))
    }

    fn get_ptr(&self) -> *const libc::c_char {
        self.inner
            .as_ref()
            .map(|b| b.cstring.as_ptr())
            .unwrap_or_else(ptr::null)
    }
}

#[derive(Default, Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct RsvgRectangle {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl From<cairo::Rectangle> for RsvgRectangle {
    fn from(r: cairo::Rectangle) -> RsvgRectangle {
        RsvgRectangle {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

impl From<RsvgRectangle> for cairo::Rectangle {
    fn from(r: RsvgRectangle) -> cairo::Rectangle {
        cairo::Rectangle {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

/// Contains all the interior mutability for a RsvgHandle to be called
/// from the C API.
pub struct CHandle {
    inner: RefCell<CHandleInner>,
    load_state: RefCell<LoadState>,
}

struct CHandleInner {
    dpi: Dpi,
    load_flags: LoadFlags,
    base_url: BaseUrl,
    size_callback: SizeCallback,
    is_testing: bool,
}

unsafe impl ClassStruct for RsvgHandleClass {
    type Type = CHandle;
}

unsafe impl InstanceStruct for RsvgHandle {
    type Type = CHandle;
}

impl ops::Deref for RsvgHandleClass {
    type Target = ObjectClass;

    fn deref(&self) -> &Self::Target {
        unsafe {
            let self_ptr: *const RsvgHandleClass = self;
            &*(self_ptr as *const Self::Target)
        }
    }
}

impl ops::DerefMut for RsvgHandleClass {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            let self_ptr: *mut RsvgHandleClass = self;
            &mut *(self_ptr as *mut Self::Target)
        }
    }
}

static PROPERTIES: [subclass::Property<'_>; 11] = [
    subclass::Property("flags", |name| {
        ParamSpec::flags(
            name,
            "Flags",
            "Loading flags",
            HandleFlags::static_type(),
            0,
            ParamFlags::READWRITE | ParamFlags::CONSTRUCT_ONLY,
        )
    }),
    subclass::Property("dpi-x", |name| {
        ParamSpec::double(
            name,
            "Horizontal DPI",
            "Horizontal resolution in dots per inch",
            0.0,
            f64::MAX,
            0.0,
            ParamFlags::READWRITE | ParamFlags::CONSTRUCT,
        )
    }),
    subclass::Property("dpi-y", |name| {
        ParamSpec::double(
            name,
            "Vertical DPI",
            "Vertical resolution in dots per inch",
            0.0,
            f64::MAX,
            0.0,
            ParamFlags::READWRITE | ParamFlags::CONSTRUCT,
        )
    }),
    subclass::Property("base-uri", |name| {
        ParamSpec::string(
            name,
            "Base URI",
            "Base URI for resolving relative references",
            None,
            ParamFlags::READWRITE | ParamFlags::CONSTRUCT,
        )
    }),
    subclass::Property("width", |name| {
        ParamSpec::int(
            name,
            "Image width",
            "Image width",
            0,
            i32::MAX,
            0,
            ParamFlags::READABLE,
        )
    }),
    subclass::Property("height", |name| {
        ParamSpec::int(
            name,
            "Image height",
            "Image height",
            0,
            i32::MAX,
            0,
            ParamFlags::READABLE,
        )
    }),
    subclass::Property("em", |name| {
        ParamSpec::double(name, "em", "em", 0.0, f64::MAX, 0.0, ParamFlags::READABLE)
    }),
    subclass::Property("ex", |name| {
        ParamSpec::double(name, "ex", "ex", 0.0, f64::MAX, 0.0, ParamFlags::READABLE)
    }),
    subclass::Property("title", |name| {
        ParamSpec::string(name, "deprecated", "deprecated", None, ParamFlags::READABLE)
    }),
    subclass::Property("desc", |name| {
        ParamSpec::string(name, "deprecated", "deprecated", None, ParamFlags::READABLE)
    }),
    subclass::Property("metadata", |name| {
        ParamSpec::string(name, "deprecated", "deprecated", None, ParamFlags::READABLE)
    }),
];

impl ObjectSubclass for CHandle {
    const NAME: &'static str = "RsvgHandle";

    type ParentType = glib::Object;

    // We don't use subclass:simple::InstanceStruct and ClassStruct
    // because we need to maintain the respective _abi_padding of each
    // of RsvgHandleClass and RsvgHandle.

    type Instance = RsvgHandle;
    type Class = RsvgHandleClass;

    glib_object_subclass!();

    fn class_init(klass: &mut RsvgHandleClass) {
        klass.install_properties(&PROPERTIES);
    }

    fn new() -> Self {
        CHandle {
            inner: RefCell::new(CHandleInner {
                dpi: Dpi::default(),
                load_flags: LoadFlags::default(),
                base_url: BaseUrl::default(),
                size_callback: SizeCallback::default(),
                is_testing: false,
            }),
            load_state: RefCell::new(LoadState::Start),
        }
    }
}

impl StaticType for CHandle {
    fn static_type() -> Type {
        CHandle::get_type()
    }
}

impl ObjectImpl for CHandle {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("flags", ..) => {
                let v: HandleFlags = value.get_some().expect("flags value has incorrect type");
                self.set_flags(v);
            }

            subclass::Property("dpi-x", ..) => {
                let dpi_x: f64 = value.get_some().expect("dpi-x value has incorrect type");
                self.set_dpi_x(dpi_x);
            }

            subclass::Property("dpi-y", ..) => {
                let dpi_y: f64 = value.get_some().expect("dpi-y value has incorrect type");
                self.set_dpi_y(dpi_y);
            }

            subclass::Property("base-uri", ..) => {
                let v: Option<String> = value.get().expect("base-uri value has incorrect type");

                // rsvg_handle_set_base_uri() expects non-NULL URI strings,
                // but the "base-uri" property can be set to NULL due to a missing
                // construct-time property.

                if let Some(s) = v {
                    self.set_base_url(&s);
                }
            }

            _ => unreachable!("invalid property id {}", id),
        }
    }

    #[rustfmt::skip]
    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("flags", ..) => Ok(self.get_flags().to_value()),

            subclass::Property("dpi-x", ..) => Ok(self.get_dpi_x().to_value()),
            subclass::Property("dpi-y", ..) => Ok(self.get_dpi_y().to_value()),

            subclass::Property("base-uri", ..) => Ok(self.get_base_url().to_value()),

            subclass::Property("width", ..) =>
                Ok(self.get_dimensions_or_empty().width.to_value()),

            subclass::Property("height", ..) =>
                Ok(self.get_dimensions_or_empty().height.to_value()),

            subclass::Property("em", ..) =>
                Ok(self.get_dimensions_or_empty().em.to_value()),

            subclass::Property("ex", ..) =>
                Ok(self.get_dimensions_or_empty().ex.to_value()),

            // the following three are deprecated
            subclass::Property("title", ..)    => Ok(None::<String>.to_value()),
            subclass::Property("desc", ..)     => Ok(None::<String>.to_value()),
            subclass::Property("metadata", ..) => Ok(None::<String>.to_value()),

            _ => unreachable!("invalid property id={} for RsvgHandle", id),
        }
    }
}

// Keep in sync with tests/src/reference.rs
pub(crate) fn checked_i32(x: f64) -> Result<i32, cairo::Error> {
    cast::i32(x).map_err(|_| cairo::Error::InvalidSize)
}

// Keep in sync with rsvg.h:RsvgPositionData
#[repr(C)]
pub struct RsvgPositionData {
    pub x: libc::c_int,
    pub y: libc::c_int,
}

// Keep in sync with rsvg.h:RsvgDimensionData
#[repr(C)]
pub struct RsvgDimensionData {
    pub width: libc::c_int,
    pub height: libc::c_int,
    pub em: f64,
    pub ex: f64,
}

impl RsvgDimensionData {
    // This is not #[derive(Default)] to make it clear that it
    // shouldn't be the default value for anything; it is actually a
    // special case we use to indicate an error to the public API.
    pub fn empty() -> RsvgDimensionData {
        RsvgDimensionData {
            width: 0,
            height: 0,
            em: 0.0,
            ex: 0.0,
        }
    }
}

// Keep in sync with rsvg.h:RsvgSizeFunc
pub type RsvgSizeFunc = Option<
    unsafe extern "C" fn(
        inout_width: *mut libc::c_int,
        inout_height: *mut libc::c_int,
        user_data: glib_sys::gpointer,
    ),
>;

struct SizeCallback {
    size_func: RsvgSizeFunc,
    user_data: glib_sys::gpointer,
    destroy_notify: glib_sys::GDestroyNotify,
    in_loop: Cell<bool>,
}

impl SizeCallback {
    fn new(
        size_func: RsvgSizeFunc,
        user_data: glib_sys::gpointer,
        destroy_notify: glib_sys::GDestroyNotify,
    ) -> Self {
        SizeCallback {
            size_func,
            user_data,
            destroy_notify,
            in_loop: Cell::new(false),
        }
    }

    fn call(&self, width: libc::c_int, height: libc::c_int) -> (libc::c_int, libc::c_int) {
        unsafe {
            let mut w = width;
            let mut h = height;

            if let Some(ref f) = self.size_func {
                f(&mut w, &mut h, self.user_data);
            };

            (w, h)
        }
    }

    fn start_loop(&self) {
        assert!(!self.in_loop.get());
        self.in_loop.set(true);
    }

    fn end_loop(&self) {
        assert!(self.in_loop.get());
        self.in_loop.set(false);
    }

    fn get_in_loop(&self) -> bool {
        self.in_loop.get()
    }
}

impl Default for SizeCallback {
    fn default() -> SizeCallback {
        SizeCallback {
            size_func: None,
            user_data: ptr::null_mut(),
            destroy_notify: None,
            in_loop: Cell::new(false),
        }
    }
}

impl Drop for SizeCallback {
    fn drop(&mut self) {
        unsafe {
            if let Some(ref f) = self.destroy_notify {
                f(self.user_data);
            };
        }
    }
}
pub trait CairoRectangleExt {
    fn from_size(width: f64, height: f64) -> Self;
}

impl CairoRectangleExt for cairo::Rectangle {
    fn from_size(width: f64, height: f64) -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width,
            height,
        }
    }
}

impl CHandle {
    fn set_base_url(&self, url: &str) {
        let state = self.load_state.borrow();

        match *state {
            LoadState::Start => (),
            _ => {
                rsvg_g_critical(
                    "Please set the base file or URI before loading any data into RsvgHandle",
                );
                return;
            }
        }

        match Url::parse(&url) {
            Ok(u) => {
                rsvg_log!("setting base_uri to \"{}\"", u.as_str());
                let mut inner = self.inner.borrow_mut();
                inner.base_url.set(u);
            }

            Err(e) => {
                rsvg_log!(
                    "not setting base_uri to \"{}\" since it is invalid: {}",
                    url,
                    e
                );
            }
        }
    }

    fn set_base_gfile(&self, file: &gio::File) {
        self.set_base_url(&file.get_uri());
    }

    fn get_base_url(&self) -> Option<String> {
        let inner = self.inner.borrow();
        inner.base_url.get().map(|url| url.as_str().to_string())
    }

    fn get_base_url_as_ptr(&self) -> *const libc::c_char {
        let inner = self.inner.borrow();
        inner.base_url.get_ptr()
    }

    fn set_dpi_x(&self, dpi_x: f64) {
        let mut inner = self.inner.borrow_mut();
        let dpi = inner.dpi;
        inner.dpi = Dpi::new(dpi_x, dpi.y());
    }

    fn set_dpi_y(&self, dpi_y: f64) {
        let mut inner = self.inner.borrow_mut();
        let dpi = inner.dpi;
        inner.dpi = Dpi::new(dpi.x(), dpi_y);
    }

    fn get_dpi_x(&self) -> f64 {
        let inner = self.inner.borrow();
        inner.dpi.x()
    }

    fn get_dpi_y(&self) -> f64 {
        let inner = self.inner.borrow();
        inner.dpi.y()
    }

    fn set_flags(&self, flags: HandleFlags) {
        let mut inner = self.inner.borrow_mut();
        inner.load_flags = LoadFlags::from(flags);
    }

    fn get_flags(&self) -> HandleFlags {
        let inner = self.inner.borrow();
        HandleFlags::from(inner.load_flags)
    }

    fn set_size_callback(
        &self,
        size_func: RsvgSizeFunc,
        user_data: glib_sys::gpointer,
        destroy_notify: glib_sys::GDestroyNotify,
    ) {
        let mut inner = self.inner.borrow_mut();
        inner.size_callback = SizeCallback::new(size_func, user_data, destroy_notify);
    }

    fn write(&self, buf: &[u8]) {
        let mut state = self.load_state.borrow_mut();

        match *state {
            LoadState::Start => {
                *state = LoadState::Loading {
                    buffer: Vec::from(buf),
                }
            }

            LoadState::Loading { ref mut buffer } => {
                buffer.extend_from_slice(buf);
            }

            _ => {
                rsvg_g_critical("Handle must not be closed in order to write to it");
            }
        }
    }

    fn close(&self) -> Result<(), LoadingError> {
        let inner = self.inner.borrow();
        let mut state = self.load_state.borrow_mut();

        match *state {
            LoadState::Start => {
                *state = LoadState::ClosedError;
                Err(LoadingError::XmlParseError(String::from(
                    "caller did not write any data",
                )))
            }

            LoadState::Loading { ref buffer } => {
                let bytes = Bytes::from(&*buffer);
                let stream = gio::MemoryInputStream::from_bytes(&bytes);

                let base_file = inner.base_url.get_gfile();
                self.read_stream(state, &stream.upcast(), base_file.as_ref(), None)
            }

            // Closing is idempotent
            LoadState::ClosedOk { .. } => Ok(()),
            LoadState::ClosedError => Ok(()),
        }
    }

    fn read_stream_sync(
        &self,
        stream: &gio::InputStream,
        cancellable: Option<&gio::Cancellable>,
    ) -> Result<(), LoadingError> {
        let state = self.load_state.borrow_mut();
        let inner = self.inner.borrow();

        match *state {
            LoadState::Start => {
                let base_file = inner.base_url.get_gfile();
                self.read_stream(state, stream, base_file.as_ref(), cancellable)
            }

            LoadState::Loading { .. } | LoadState::ClosedOk { .. } | LoadState::ClosedError => {
                rsvg_g_critical(
                    "handle must not be already loaded in order to call \
                     rsvg_handle_read_stream_sync()",
                );
                Err(LoadingError::Other(String::from("API ordering")))
            }
        }
    }

    fn read_stream(
        &self,
        mut load_state: RefMut<'_, LoadState>,
        stream: &gio::InputStream,
        base_file: Option<&gio::File>,
        cancellable: Option<&gio::Cancellable>,
    ) -> Result<(), LoadingError> {
        let loader = self.make_loader();

        load_state.set_from_loading_result(loader.read_stream(stream, base_file, cancellable))
    }

    fn get_handle_ref(&self) -> Result<Ref<'_, SvgHandle>, RenderingError> {
        let state = self.load_state.borrow();

        match *state {
            LoadState::Start => {
                rsvg_g_critical("Handle has not been loaded");
                Err(RenderingError::HandleIsNotLoaded)
            }

            LoadState::Loading { .. } => {
                rsvg_g_critical("Handle is still loading; call rsvg_handle_close() first");
                Err(RenderingError::HandleIsNotLoaded)
            }

            LoadState::ClosedError => {
                rsvg_g_critical(
                    "Handle could not read or parse the SVG; did you check for errors during the \
                     loading stage?",
                );
                Err(RenderingError::HandleIsNotLoaded)
            }

            LoadState::ClosedOk { .. } => Ok(Ref::map(state, |s| match *s {
                LoadState::ClosedOk { ref handle } => handle,
                _ => unreachable!(),
            })),
        }
    }

    fn make_loader(&self) -> Loader {
        let inner = self.inner.borrow();

        Loader::new()
            .with_unlimited_size(inner.load_flags.unlimited_size)
            .keep_image_data(inner.load_flags.keep_image_data)
    }

    fn has_sub(&self, id: &str) -> Result<bool, RenderingError> {
        let handle = self.get_handle_ref()?;
        Ok(handle.has_element_with_id(id)?)
    }

    fn get_dimensions_or_empty(&self) -> RsvgDimensionData {
        self.get_dimensions_sub(None)
            .unwrap_or_else(|_| RsvgDimensionData::empty())
    }

    fn get_dimensions_sub(&self, id: Option<&str>) -> Result<RsvgDimensionData, RenderingError> {
        let inner = self.inner.borrow();

        // This function is probably called from the cairo_render functions,
        // or is being erroneously called within the size_func.
        // To prevent an infinite loop we are saving the state, and
        // returning a meaningless size.
        if inner.size_callback.get_in_loop() {
            return Ok(RsvgDimensionData {
                width: 1,
                height: 1,
                em: 1.0,
                ex: 1.0,
            });
        }

        inner.size_callback.start_loop();

        let res = self
            .get_geometry_sub(id)
            .and_then(|(ink_r, _)| {
                // Keep these in sync with tests/src/reference.rs
                let width = checked_i32(ink_r.width.round())?;
                let height = checked_i32(ink_r.height.round())?;

                Ok((ink_r, width, height))
            })
            .map(|(ink_r, width, height)| {
                let (w, h) = inner.size_callback.call(width, height);

                RsvgDimensionData {
                    width: w,
                    height: h,
                    em: ink_r.width,
                    ex: ink_r.height,
                }
            });

        inner.size_callback.end_loop();

        res
    }

    fn get_position_sub(&self, id: Option<&str>) -> Result<RsvgPositionData, RenderingError> {
        let inner = self.inner.borrow();

        if id.is_none() {
            return Ok(RsvgPositionData { x: 0, y: 0 });
        }

        self.get_geometry_sub(id)
            .and_then(|(ink_r, _)| {
                let width = checked_i32(ink_r.width.round())?;
                let height = checked_i32(ink_r.height.round())?;

                Ok((ink_r, width, height))
            })
            .and_then(|(ink_r, width, height)| {
                inner.size_callback.call(width, height);

                Ok(RsvgPositionData {
                    x: checked_i32(ink_r.x)?,
                    y: checked_i32(ink_r.y)?,
                })
            })
    }

    fn make_renderer<'a>(&self, handle_ref: &'a Ref<'_, SvgHandle>) -> CairoRenderer<'a> {
        let inner = self.inner.borrow();

        let mut renderer = CairoRenderer::new(&*handle_ref).with_dpi(inner.dpi.x(), inner.dpi.y());

        if inner.is_testing {
            renderer = renderer.test_mode();
        }

        renderer
    }

    fn get_geometry_sub(
        &self,
        id: Option<&str>,
    ) -> Result<(cairo::Rectangle, cairo::Rectangle), RenderingError> {
        let handle = self.get_handle_ref()?;
        let renderer = self.make_renderer(&handle);

        Ok(renderer.legacy_layer_geometry(id)?)
    }

    fn set_stylesheet(&self, css: &str) -> Result<(), LoadingError> {
        match *self.load_state.borrow_mut() {
            LoadState::ClosedOk { ref mut handle } => handle.set_stylesheet(css),

            _ => {
                rsvg_g_critical(
                    "handle must already be loaded in order to call \
                     rsvg_handle_set_stylesheet()",
                );
                Err(LoadingError::Other(String::from("API ordering")))
            }
        }
    }

    fn render_cairo_sub(
        &self,
        cr: *mut cairo_sys::cairo_t,
        id: Option<&str>,
    ) -> Result<(), RenderingError> {
        let dimensions = self.get_dimensions_sub(None)?;
        if dimensions.width == 0 || dimensions.height == 0 {
            // nothing to render
            return Ok(());
        }

        let viewport = cairo::Rectangle {
            x: 0.0,
            y: 0.0,
            width: f64::from(dimensions.width),
            height: f64::from(dimensions.height),
        };

        self.render_layer(cr, id, &viewport)
    }

    fn get_pixbuf_sub(&self, id: Option<&str>) -> Result<Pixbuf, RenderingError> {
        let dimensions = self.get_dimensions_sub(None)?;

        if dimensions.width == 0 || dimensions.height == 0 {
            return Ok(empty_pixbuf()?);
        }

        let surface = cairo::ImageSurface::create(
            cairo::Format::ARgb32,
            dimensions.width,
            dimensions.height,
        )?;

        {
            let cr = cairo::Context::new(&surface);
            let cr_raw = cr.to_raw_none();
            self.render_cairo_sub(cr_raw, id)?;
        }

        let surface = SharedImageSurface::wrap(surface, SurfaceType::SRgb)?;

        Ok(pixbuf_from_surface(&surface)?)
    }

    fn render_document(
        &self,
        cr: *mut cairo_sys::cairo_t,
        viewport: &cairo::Rectangle,
    ) -> Result<(), RenderingError> {
        let cr = check_cairo_context(cr)?;

        let handle = self.get_handle_ref()?;

        let renderer = self.make_renderer(&handle);
        Ok(renderer.render_document(&cr, viewport)?)
    }

    fn get_geometry_for_layer(
        &self,
        id: Option<&str>,
        viewport: &cairo::Rectangle,
    ) -> Result<(RsvgRectangle, RsvgRectangle), RenderingError> {
        let handle = self.get_handle_ref()?;
        let renderer = self.make_renderer(&handle);

        Ok(renderer
            .geometry_for_layer(id, viewport)
            .map(|(i, l)| (RsvgRectangle::from(i), RsvgRectangle::from(l)))?)
    }

    fn render_layer(
        &self,
        cr: *mut cairo_sys::cairo_t,
        id: Option<&str>,
        viewport: &cairo::Rectangle,
    ) -> Result<(), RenderingError> {
        let cr = check_cairo_context(cr)?;

        let handle = self.get_handle_ref()?;

        let renderer = self.make_renderer(&handle);

        Ok(renderer.render_layer(&cr, id, viewport)?)
    }

    fn get_geometry_for_element(
        &self,
        id: Option<&str>,
    ) -> Result<(RsvgRectangle, RsvgRectangle), RenderingError> {
        let handle = self.get_handle_ref()?;

        let renderer = self.make_renderer(&handle);

        Ok(renderer
            .geometry_for_element(id)
            .map(|(i, l)| (RsvgRectangle::from(i), RsvgRectangle::from(l)))?)
    }

    fn render_element(
        &self,
        cr: *mut cairo_sys::cairo_t,
        id: Option<&str>,
        element_viewport: &cairo::Rectangle,
    ) -> Result<(), RenderingError> {
        let cr = check_cairo_context(cr)?;

        let handle = self.get_handle_ref()?;

        let renderer = self.make_renderer(&handle);

        Ok(renderer.render_element(&cr, id, element_viewport)?)
    }

    fn get_intrinsic_dimensions(&self) -> Result<IntrinsicDimensions, RenderingError> {
        let handle = self.get_handle_ref()?;
        let renderer = self.make_renderer(&handle);
        Ok(renderer.intrinsic_dimensions())
    }

    fn get_intrinsic_size_in_pixels(&self) -> Result<Option<(f64, f64)>, RenderingError> {
        let handle = self.get_handle_ref()?;
        let renderer = self.make_renderer(&handle);
        Ok(renderer.intrinsic_size_in_pixels())
    }

    fn set_testing(&self, is_testing: bool) {
        let mut inner = self.inner.borrow_mut();
        inner.is_testing = is_testing;
    }
}

fn is_rsvg_handle(obj: *const RsvgHandle) -> bool {
    unsafe { instance_of::<CHandle>(obj as *const _) }
}

fn is_input_stream(obj: *mut gio_sys::GInputStream) -> bool {
    unsafe { instance_of::<gio::InputStream>(obj as *const _) }
}

fn is_gfile(obj: *const gio_sys::GFile) -> bool {
    unsafe { instance_of::<gio::File>(obj as *const _) }
}

fn is_cancellable(obj: *mut gio_sys::GCancellable) -> bool {
    unsafe { instance_of::<gio::Cancellable>(obj as *const _) }
}

fn get_rust_handle<'a>(handle: *const RsvgHandle) -> &'a CHandle {
    let handle = unsafe { &*handle };
    handle.get_impl()
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_type() -> glib_sys::GType {
    CHandle::get_type().to_glib()
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_error_get_type() -> glib_sys::GType {
    Error::static_type().to_glib()
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_flags_get_type() -> glib_sys::GType {
    HandleFlags::static_type().to_glib()
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_set_base_uri(
    handle: *const RsvgHandle,
    uri: *const libc::c_char,
) {
    rsvg_return_if_fail! {
        rsvg_handle_set_base_uri;

        is_rsvg_handle(handle),
        !uri.is_null(),
    }

    let rhandle = get_rust_handle(handle);

    assert!(!uri.is_null());
    let uri: String = from_glib_none(uri);

    rhandle.set_base_url(&uri);
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_set_base_gfile(
    handle: *const RsvgHandle,
    raw_gfile: *mut gio_sys::GFile,
) {
    rsvg_return_if_fail! {
        rsvg_handle_set_base_gfile;

        is_rsvg_handle(handle),
        is_gfile(raw_gfile),
    }

    let rhandle = get_rust_handle(handle);

    assert!(!raw_gfile.is_null());

    let file: gio::File = from_glib_none(raw_gfile);

    rhandle.set_base_gfile(&file);
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_base_uri(
    handle: *const RsvgHandle,
) -> *const libc::c_char {
    rsvg_return_val_if_fail! {
        rsvg_handle_get_base_uri => ptr::null();

        is_rsvg_handle(handle),
    }

    let rhandle = get_rust_handle(handle);

    rhandle.get_base_url_as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_set_dpi(handle: *const RsvgHandle, dpi: libc::c_double) {
    rsvg_return_if_fail! {
        rsvg_handle_set_dpi;

        is_rsvg_handle(handle),
    }

    let rhandle = get_rust_handle(handle);
    rhandle.set_dpi_x(dpi);
    rhandle.set_dpi_y(dpi);
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_set_dpi_x_y(
    handle: *const RsvgHandle,
    dpi_x: libc::c_double,
    dpi_y: libc::c_double,
) {
    rsvg_return_if_fail! {
        rsvg_handle_set_dpi_x_y;

        is_rsvg_handle(handle),
    }

    let rhandle = get_rust_handle(handle);
    rhandle.set_dpi_x(dpi_x);
    rhandle.set_dpi_y(dpi_y);
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_set_size_callback(
    handle: *const RsvgHandle,
    size_func: RsvgSizeFunc,
    user_data: glib_sys::gpointer,
    destroy_notify: glib_sys::GDestroyNotify,
) {
    rsvg_return_if_fail! {
        rsvg_handle_set_size_callback;

        is_rsvg_handle(handle),
    }

    let rhandle = get_rust_handle(handle);

    rhandle.set_size_callback(size_func, user_data, destroy_notify);
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_internal_set_testing(
    handle: *const RsvgHandle,
    testing: glib_sys::gboolean,
) {
    rsvg_return_if_fail! {
        rsvg_handle_internal_set_testing;

        is_rsvg_handle(handle),
    }

    let rhandle = get_rust_handle(handle);

    rhandle.set_testing(from_glib(testing));
}

trait IntoGError {
    type GlibResult;

    fn into_gerror(self, error: *mut *mut glib_sys::GError) -> Self::GlibResult;
}

impl<E: fmt::Display> IntoGError for Result<(), E> {
    type GlibResult = glib_sys::gboolean;

    fn into_gerror(self, error: *mut *mut glib_sys::GError) -> Self::GlibResult {
        match self {
            Ok(()) => true.to_glib(),

            Err(e) => {
                set_gerror(error, 0, &format!("{}", e));
                false.to_glib()
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_read_stream_sync(
    handle: *const RsvgHandle,
    stream: *mut gio_sys::GInputStream,
    cancellable: *mut gio_sys::GCancellable,
    error: *mut *mut glib_sys::GError,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_read_stream_sync => false.to_glib();

        is_rsvg_handle(handle),
        is_input_stream(stream),
        cancellable.is_null() || is_cancellable(cancellable),
        error.is_null() || (*error).is_null(),
    }

    let rhandle = get_rust_handle(handle);

    let stream = gio::InputStream::from_glib_none(stream);
    let cancellable: Option<gio::Cancellable> = from_glib_none(cancellable);

    rhandle
        .read_stream_sync(&stream, cancellable.as_ref())
        .into_gerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_write(
    handle: *const RsvgHandle,
    buf: *const u8,
    count: usize,
    error: *mut *mut glib_sys::GError,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_write => false.to_glib();

        is_rsvg_handle(handle),
        error.is_null() || (*error).is_null(),
        (!buf.is_null() && count != 0) || (count == 0),
    }

    let rhandle = get_rust_handle(handle);
    let buffer = slice::from_raw_parts(buf, count);
    rhandle.write(buffer);

    true.to_glib()
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_close(
    handle: *const RsvgHandle,
    error: *mut *mut glib_sys::GError,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_close => false.to_glib();

        is_rsvg_handle(handle),
        error.is_null() || (*error).is_null(),
    }

    let rhandle = get_rust_handle(handle);

    rhandle.close().into_gerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_has_sub(
    handle: *const RsvgHandle,
    id: *const libc::c_char,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_has_sub => false.to_glib();

        is_rsvg_handle(handle),
    }

    let rhandle = get_rust_handle(handle);

    if id.is_null() {
        return false.to_glib();
    }

    let id: String = from_glib_none(id);
    rhandle.has_sub(&id).unwrap_or(false).to_glib()
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_render_cairo(
    handle: *const RsvgHandle,
    cr: *mut cairo_sys::cairo_t,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_render_cairo => false.to_glib();

        is_rsvg_handle(handle),
        !cr.is_null(),
    }

    let rhandle = get_rust_handle(handle);

    rhandle
        .render_cairo_sub(cr, None)
        .into_gerror(ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_render_cairo_sub(
    handle: *const RsvgHandle,
    cr: *mut cairo_sys::cairo_t,
    id: *const libc::c_char,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_render_cairo_sub => false.to_glib();

        is_rsvg_handle(handle),
        !cr.is_null(),
    }

    let rhandle = get_rust_handle(handle);
    let id: Option<String> = from_glib_none(id);

    rhandle
        .render_cairo_sub(cr, id.as_deref())
        .into_gerror(ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_pixbuf(
    handle: *const RsvgHandle,
) -> *mut gdk_pixbuf_sys::GdkPixbuf {
    rsvg_return_val_if_fail! {
        rsvg_handle_get_pixbuf => ptr::null_mut();

        is_rsvg_handle(handle),
    }

    let rhandle = get_rust_handle(handle);

    match rhandle.get_pixbuf_sub(None) {
        Ok(pixbuf) => pixbuf.to_glib_full(),
        Err(e) => {
            rsvg_log!("could not render: {}", e);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_pixbuf_sub(
    handle: *const RsvgHandle,
    id: *const libc::c_char,
) -> *mut gdk_pixbuf_sys::GdkPixbuf {
    rsvg_return_val_if_fail! {
        rsvg_handle_get_pixbuf_sub => ptr::null_mut();

        is_rsvg_handle(handle),
    }

    let rhandle = get_rust_handle(handle);
    let id: Option<String> = from_glib_none(id);

    match rhandle.get_pixbuf_sub(id.as_deref()) {
        Ok(pixbuf) => pixbuf.to_glib_full(),
        Err(e) => {
            rsvg_log!("could not render: {}", e);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_dimensions(
    handle: *const RsvgHandle,
    dimension_data: *mut RsvgDimensionData,
) {
    rsvg_handle_get_dimensions_sub(handle, dimension_data, ptr::null());
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_dimensions_sub(
    handle: *const RsvgHandle,
    dimension_data: *mut RsvgDimensionData,
    id: *const libc::c_char,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_get_dimensions_sub => false.to_glib();

        is_rsvg_handle(handle),
        !dimension_data.is_null(),
    }

    let rhandle = get_rust_handle(handle);

    let id: Option<String> = from_glib_none(id);

    match rhandle.get_dimensions_sub(id.as_deref()) {
        Ok(dimensions) => {
            *dimension_data = dimensions;
            true.to_glib()
        }

        Err(e) => {
            rsvg_log!("could not get dimensions: {}", e);
            *dimension_data = RsvgDimensionData::empty();
            false.to_glib()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_position_sub(
    handle: *const RsvgHandle,
    position_data: *mut RsvgPositionData,
    id: *const libc::c_char,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_get_position_sub => false.to_glib();

        is_rsvg_handle(handle),
        !position_data.is_null(),
    }

    let rhandle = get_rust_handle(handle);

    let id: Option<String> = from_glib_none(id);

    match rhandle.get_position_sub(id.as_deref()) {
        Ok(position) => {
            *position_data = position;
            true.to_glib()
        }

        Err(e) => {
            let p = &mut *position_data;

            p.x = 0;
            p.y = 0;

            rsvg_log!("could not get position: {}", e);
            false.to_glib()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_new() -> *const RsvgHandle {
    let obj: *mut gobject_sys::GObject = glib::Object::new(CHandle::get_type(), &[])
        .unwrap()
        .to_glib_full();

    obj as *mut _
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_new_with_flags(flags: RsvgHandleFlags) -> *const RsvgHandle {
    let obj: *mut gobject_sys::GObject = glib::Object::new(
        CHandle::get_type(),
        &[("flags", &HandleFlags::from_bits_truncate(flags))],
    )
    .unwrap()
    .to_glib_full();

    obj as *mut _
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_new_from_file(
    filename: *const libc::c_char,
    error: *mut *mut glib_sys::GError,
) -> *const RsvgHandle {
    rsvg_return_val_if_fail! {
        rsvg_handle_new_from_file => ptr::null();

        !filename.is_null(),
        error.is_null() || (*error).is_null(),
    }

    let file = match PathOrUrl::new(filename) {
        Ok(p) => p.get_gfile(),

        Err(s) => {
            set_gerror(error, 0, &s);
            return ptr::null_mut();
        }
    };

    rsvg_handle_new_from_gfile_sync(file.to_glib_none().0, 0, ptr::null_mut(), error)
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_new_from_gfile_sync(
    file: *mut gio_sys::GFile,
    flags: RsvgHandleFlags,
    cancellable: *mut gio_sys::GCancellable,
    error: *mut *mut glib_sys::GError,
) -> *const RsvgHandle {
    rsvg_return_val_if_fail! {
        rsvg_handle_new_from_gfile_sync => ptr::null();

        is_gfile(file),
        cancellable.is_null() || is_cancellable(cancellable),
        error.is_null() || (*error).is_null(),
    }

    let raw_handle = rsvg_handle_new_with_flags(flags);

    let rhandle = get_rust_handle(raw_handle);

    let file = gio::File::from_glib_none(file);
    rhandle.set_base_gfile(&file);

    let cancellable: Option<gio::Cancellable> = from_glib_none(cancellable);

    let res = file
        .read(cancellable.as_ref())
        .map_err(LoadingError::from)
        .and_then(|stream| rhandle.read_stream_sync(&stream.upcast(), cancellable.as_ref()));

    match res {
        Ok(()) => raw_handle,

        Err(e) => {
            set_gerror(error, 0, &format!("{}", e));
            gobject_sys::g_object_unref(raw_handle as *mut _);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_new_from_stream_sync(
    input_stream: *mut gio_sys::GInputStream,
    base_file: *mut gio_sys::GFile,
    flags: RsvgHandleFlags,
    cancellable: *mut gio_sys::GCancellable,
    error: *mut *mut glib_sys::GError,
) -> *const RsvgHandle {
    rsvg_return_val_if_fail! {
        rsvg_handle_new_from_stream_sync => ptr::null();

        is_input_stream(input_stream),
        base_file.is_null() || is_gfile(base_file),
        cancellable.is_null() || is_cancellable(cancellable),
        error.is_null() || (*error).is_null(),
    }

    let raw_handle = rsvg_handle_new_with_flags(flags);

    let rhandle = get_rust_handle(raw_handle);

    let base_file: Option<gio::File> = from_glib_none(base_file);
    if let Some(base_file) = base_file {
        rhandle.set_base_gfile(&base_file);
    }

    let stream: gio::InputStream = from_glib_none(input_stream);
    let cancellable: Option<gio::Cancellable> = from_glib_none(cancellable);

    match rhandle.read_stream_sync(&stream, cancellable.as_ref()) {
        Ok(()) => raw_handle,

        Err(e) => {
            set_gerror(error, 0, &format!("{}", e));
            gobject_sys::g_object_unref(raw_handle as *mut _);
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_new_from_data(
    data: *const u8,
    data_len: usize,
    error: *mut *mut glib_sys::GError,
) -> *const RsvgHandle {
    rsvg_return_val_if_fail! {
        rsvg_handle_new_from_data => ptr::null();

        (!data.is_null() && data_len != 0) || (data_len == 0),
        data_len <= std::isize::MAX as usize,
        error.is_null() || (*error).is_null(),
    }

    // We create the MemoryInputStream without the gtk-rs binding because of this:
    //
    // - The binding doesn't provide _new_from_data().  All of the binding's ways to
    // put data into a MemoryInputStream involve copying the data buffer.
    //
    // - We can't use glib::Bytes from the binding either, for the same reason.
    //
    // - For now, we are using the other C-visible constructor, so we need a raw pointer to the
    //   stream, anyway.

    assert!(data_len <= std::isize::MAX as usize);
    let data_len = data_len as isize;

    let raw_stream = gio_sys::g_memory_input_stream_new_from_data(data as *mut u8, data_len, None);

    let ret = rsvg_handle_new_from_stream_sync(
        raw_stream,
        ptr::null_mut(), // base_file
        0,
        ptr::null_mut(), // cancellable
        error,
    );

    gobject_sys::g_object_unref(raw_stream as *mut _);
    ret
}

unsafe fn set_out_param<T: Copy>(
    out_has_param: *mut glib_sys::gboolean,
    out_param: *mut T,
    value: &Option<T>,
) {
    let has_value = if let Some(ref v) = *value {
        if !out_param.is_null() {
            *out_param = *v;
        }

        true
    } else {
        false
    };

    if !out_has_param.is_null() {
        *out_has_param = has_value.to_glib();
    }
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_free(handle: *mut RsvgHandle) {
    gobject_sys::g_object_unref(handle as *mut _);
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_set_stylesheet(
    handle: *const RsvgHandle,
    css: *const u8,
    css_len: usize,
    error: *mut *mut glib_sys::GError,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_set_stylesheet => false.to_glib();

        is_rsvg_handle(handle),
        !css.is_null() || (css.is_null() && css_len == 0),
        error.is_null() || (*error).is_null(),
    }

    let rhandle = get_rust_handle(handle);

    let css = match (css, css_len) {
        (p, 0) if p.is_null() => "",
        (_, _) => {
            let s = slice::from_raw_parts(css, css_len);
            match str::from_utf8(s) {
                Ok(s) => s,
                Err(e) => {
                    set_gerror(error, 0, &format!("CSS is not valid UTF-8: {}", e));
                    return false.to_glib();
                }
            }
        }
    };

    rhandle.set_stylesheet(css).into_gerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_intrinsic_dimensions(
    handle: *const RsvgHandle,
    out_has_width: *mut glib_sys::gboolean,
    out_width: *mut RsvgLength,
    out_has_height: *mut glib_sys::gboolean,
    out_height: *mut RsvgLength,
    out_has_viewbox: *mut glib_sys::gboolean,
    out_viewbox: *mut RsvgRectangle,
) {
    rsvg_return_if_fail! {
        rsvg_handle_get_intrinsic_dimensions;

        is_rsvg_handle(handle),
    }

    let rhandle = get_rust_handle(handle);

    let d = rhandle
        .get_intrinsic_dimensions()
        .unwrap_or_else(|_| panic!("API called out of order"));

    let w = d.width;
    let h = d.height;
    let r = d.vbox.map(RsvgRectangle::from);

    set_out_param(out_has_width, out_width, &w.map(Into::into));
    set_out_param(out_has_height, out_height, &h.map(Into::into));
    set_out_param(out_has_viewbox, out_viewbox, &r);
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_intrinsic_size_in_pixels(
    handle: *const RsvgHandle,
    out_width: *mut f64,
    out_height: *mut f64,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_get_intrinsic_size_in_pixels => false.to_glib();

        is_rsvg_handle(handle),
    }

    let rhandle = get_rust_handle(handle);

    let dim = rhandle
        .get_intrinsic_size_in_pixels()
        .unwrap_or_else(|_| panic!("API called out of order"));

    let (w, h) = dim.unwrap_or((0.0, 0.0));

    if !out_width.is_null() {
        *out_width = w;
    }

    if !out_height.is_null() {
        *out_height = h;
    }

    dim.is_some().to_glib()
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_render_document(
    handle: *const RsvgHandle,
    cr: *mut cairo_sys::cairo_t,
    viewport: *const RsvgRectangle,
    error: *mut *mut glib_sys::GError,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_render_document => false.to_glib();

        is_rsvg_handle(handle),
        !cr.is_null(),
        !viewport.is_null(),
        error.is_null() || (*error).is_null(),
    }

    let rhandle = get_rust_handle(handle);

    rhandle
        .render_document(cr, &(*viewport).into())
        .into_gerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_geometry_for_layer(
    handle: *mut RsvgHandle,
    id: *const libc::c_char,
    viewport: *const RsvgRectangle,
    out_ink_rect: *mut RsvgRectangle,
    out_logical_rect: *mut RsvgRectangle,
    error: *mut *mut glib_sys::GError,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_get_geometry_for_layer => false.to_glib();

        is_rsvg_handle(handle),
        !viewport.is_null(),
        error.is_null() || (*error).is_null(),
    }

    let rhandle = get_rust_handle(handle);

    let id: Option<String> = from_glib_none(id);

    rhandle
        .get_geometry_for_layer(id.as_deref(), &(*viewport).into())
        .map(|(ink_rect, logical_rect)| {
            if !out_ink_rect.is_null() {
                *out_ink_rect = ink_rect;
            }

            if !out_logical_rect.is_null() {
                *out_logical_rect = logical_rect;
            }
        })
        .into_gerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_render_layer(
    handle: *const RsvgHandle,
    cr: *mut cairo_sys::cairo_t,
    id: *const libc::c_char,
    viewport: *const RsvgRectangle,
    error: *mut *mut glib_sys::GError,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_render_layer => false.to_glib();

        is_rsvg_handle(handle),
        !cr.is_null(),
        !viewport.is_null(),
        error.is_null() || (*error).is_null(),
    }

    let rhandle = get_rust_handle(handle);
    let id: Option<String> = from_glib_none(id);

    rhandle
        .render_layer(cr, id.as_deref(), &(*viewport).into())
        .into_gerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_geometry_for_element(
    handle: *const RsvgHandle,
    id: *const libc::c_char,
    out_ink_rect: *mut RsvgRectangle,
    out_logical_rect: *mut RsvgRectangle,
    error: *mut *mut glib_sys::GError,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_get_geometry_for_element => false.to_glib();

        is_rsvg_handle(handle),
        error.is_null() || (*error).is_null(),
    }

    let rhandle = get_rust_handle(handle);

    let id: Option<String> = from_glib_none(id);

    rhandle
        .get_geometry_for_element(id.as_deref())
        .map(|(ink_rect, logical_rect)| {
            if !out_ink_rect.is_null() {
                *out_ink_rect = ink_rect;
            }

            if !out_logical_rect.is_null() {
                *out_logical_rect = logical_rect;
            }
        })
        .into_gerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_render_element(
    handle: *const RsvgHandle,
    cr: *mut cairo_sys::cairo_t,
    id: *const libc::c_char,
    element_viewport: *const RsvgRectangle,
    error: *mut *mut glib_sys::GError,
) -> glib_sys::gboolean {
    rsvg_return_val_if_fail! {
        rsvg_handle_render_element => false.to_glib();

        is_rsvg_handle(handle),
        !cr.is_null(),
        !element_viewport.is_null(),
        error.is_null() || (*error).is_null(),
    }

    let rhandle = get_rust_handle(handle);
    let id: Option<String> = from_glib_none(id);

    rhandle
        .render_element(cr, id.as_deref(), &(*element_viewport).into())
        .into_gerror(error)
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_desc(handle: *const RsvgHandle) -> *mut libc::c_char {
    rsvg_return_val_if_fail! {
        rsvg_handle_get_desc => ptr::null_mut();

        is_rsvg_handle(handle),
    }

    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_metadata(handle: *const RsvgHandle) -> *mut libc::c_char {
    rsvg_return_val_if_fail! {
        rsvg_handle_get_metadata => ptr::null_mut();

        is_rsvg_handle(handle),
    }

    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_handle_get_title(handle: *const RsvgHandle) -> *mut libc::c_char {
    rsvg_return_val_if_fail! {
        rsvg_handle_get_title => ptr::null_mut();

        is_rsvg_handle(handle),
    }

    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn rsvg_init() {}

#[no_mangle]
pub unsafe extern "C" fn rsvg_term() {}

#[no_mangle]
pub unsafe extern "C" fn rsvg_cleanup() {}

/// Detects whether a `*const libc::c_char` is a path or a URI
///
/// `rsvg_handle_new_from_file()` takes a `filename` argument, and advertises
/// that it will detect either a file system path, or a proper URI.  It will then use
/// `gio::File::new_for_path()` or `gio::File::new_for_uri()` as appropriate.
///
/// This enum does the magic heuristics to figure this out.
///
/// The `from_os_str` version is for using the same logic on rsvg-convert's command-line
/// arguments: we want `rsvg-convert http://example.com/foo.svg` to go to a URL, not to a
/// local file with that name.
#[derive(Clone, Debug)]
pub enum PathOrUrl {
    Path(PathBuf),
    Url(Url),
}

impl PathOrUrl {
    unsafe fn new(s: *const libc::c_char) -> Result<PathOrUrl, String> {
        let cstr = CStr::from_ptr(s);

        if cstr.to_bytes().is_empty() {
            return Err("invalid empty filename".to_string());
        }

        Ok(cstr
            .to_str()
            .map_err(|_| ())
            .and_then(Self::try_from_str)
            .unwrap_or_else(|_| PathOrUrl::Path(PathBuf::from_glib_none(s))))
    }

    fn try_from_str(s: &str) -> Result<PathOrUrl, ()> {
        assert!(!s.is_empty());

        Url::parse(s).map_err(|_| ()).and_then(|url| {
            if url.origin().is_tuple() || url.scheme() == "file" {
                Ok(PathOrUrl::Url(url))
            } else {
                Ok(PathOrUrl::Path(url.to_file_path()?))
            }
        })
    }

    pub fn from_os_str(osstr: &OsStr) -> Result<PathOrUrl, String> {
        if osstr.is_empty() {
            return Err("invalid empty filename".to_string());
        }

        Ok(osstr
            .to_str()
            .ok_or(())
            .and_then(Self::try_from_str)
            .unwrap_or_else(|_| PathOrUrl::Path(PathBuf::from(osstr.to_os_string()))))
    }

    pub fn get_gfile(&self) -> gio::File {
        match *self {
            PathOrUrl::Path(ref p) => gio::File::new_for_path(p),
            PathOrUrl::Url(ref u) => gio::File::new_for_uri(u.as_str()),
        }
    }
}

impl fmt::Display for PathOrUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PathOrUrl::Path(ref p) => p.display().fmt(f),
            PathOrUrl::Url(ref u) => u.fmt(f),
        }
    }
}

fn check_cairo_context(cr: *mut cairo_sys::cairo_t) -> Result<cairo::Context, RenderingError> {
    let status = unsafe { cairo_sys::cairo_status(cr) };

    if status == cairo_sys::STATUS_SUCCESS {
        Ok(unsafe { from_glib_none(cr) })
    } else {
        let status: cairo::Error = status.into();

        let msg = format!(
            "cannot render on a cairo_t with a failure status (status={:?})",
            status,
        );

        rsvg_g_warning(&msg);

        Err(RenderingError::from(status))
    }
}

pub(crate) fn set_gerror(err: *mut *mut glib_sys::GError, code: u32, msg: &str) {
    unsafe {
        // this is RSVG_ERROR_FAILED, the only error code available in RsvgError
        assert!(code == 0);

        // Log this, in case the calling program passes a NULL GError, so we can at least
        // diagnose things by asking for RSVG_LOG.
        //
        // See https://gitlab.gnome.org/GNOME/gtk/issues/2294 for an example of code that
        // passed a NULL GError and so we had no easy way to see what was wrong.
        rsvg_log!("{}", msg);

        glib_sys::g_set_error_literal(
            err,
            rsvg_error_quark(),
            code as libc::c_int,
            msg.to_glib_none().0,
        );
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, glib::GEnum)]
#[repr(u32)]
#[genum(type_name = "RsvgError")]
enum Error {
    #[genum(name = "RSVG_ERROR_FAILED", nick = "failed")]
    // Keep in sync with rsvg.h:RsvgError
    Failed = 0,
}

/// Used as a generic error to translate to glib::Error
///
/// This type implements `glib::error::ErrorDomain`, so it can be used
/// to obtain the error code while calling `glib::Error::new()`.  Unfortunately
/// the public librsvg API does not have detailed error codes yet, so we use
/// this single value as the only possible error code to return.
#[derive(Copy, Clone)]
struct RsvgError;

impl ErrorDomain for RsvgError {
    fn domain() -> glib::Quark {
        glib::Quark::from_string("rsvg-error-quark")
    }

    fn code(self) -> i32 {
        Error::Failed as i32
    }

    fn from(_code: i32) -> Option<Self> {
        // We don't have enough information from glib error codes
        Some(RsvgError)
    }
}

#[no_mangle]
pub extern "C" fn rsvg_error_quark() -> glib_sys::GQuark {
    RsvgError::domain().to_glib()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_or_url_unix() {
        unsafe {
            match PathOrUrl::new(b"/foo/bar\0" as *const u8 as *const _).unwrap() {
                PathOrUrl::Path(_) => (),
                _ => panic!("unix filename should be a PathOrUrl::Path"),
            }

            match PathOrUrl::new(b"foo/bar\0" as *const u8 as *const _).unwrap() {
                PathOrUrl::Path(_) => (),
                _ => panic!("unix filename should be a PathOrUrl::Path"),
            }
        }
    }

    #[test]
    fn path_or_url_windows() {
        unsafe {
            match PathOrUrl::new(b"c:/foo/bar\0" as *const u8 as *const _).unwrap() {
                PathOrUrl::Path(_) => (),
                _ => panic!("windows filename should be a PathOrUrl::Path"),
            }

            match PathOrUrl::new(b"C:/foo/bar\0" as *const u8 as *const _).unwrap() {
                PathOrUrl::Path(_) => (),
                _ => panic!("windows filename should be a PathOrUrl::Path"),
            }

            match PathOrUrl::new(b"c:\\foo\\bar\0" as *const u8 as *const _).unwrap() {
                PathOrUrl::Path(_) => (),
                _ => panic!("windows filename should be a PathOrUrl::Path"),
            }

            match PathOrUrl::new(b"C:\\foo\\bar\0" as *const u8 as *const _).unwrap() {
                PathOrUrl::Path(_) => (),
                _ => panic!("windows filename should be a PathOrUrl::Path"),
            }
        }
    }

    #[test]
    fn path_or_url_unix_url() {
        unsafe {
            match PathOrUrl::new(b"file:///foo/bar\0" as *const u8 as *const _).unwrap() {
                PathOrUrl::Url(_) => (),
                _ => panic!("file:// unix filename should be a PathOrUrl::Url"),
            }
        }
    }

    #[test]
    fn path_or_url_windows_url() {
        unsafe {
            match PathOrUrl::new(b"file://c:/foo/bar\0" as *const u8 as *const _).unwrap() {
                PathOrUrl::Url(_) => (),
                _ => panic!("file:// windows filename should be a PathOrUrl::Url"),
            }

            match PathOrUrl::new(b"file://C:/foo/bar\0" as *const u8 as *const _).unwrap() {
                PathOrUrl::Url(_) => (),
                _ => panic!("file:// windows filename should be a PathOrUrl::Url"),
            }
        }
    }

    #[test]
    fn path_or_url_empty_str() {
        unsafe {
            assert!(PathOrUrl::new(b"\0" as *const u8 as *const _).is_err());
        }

        assert!(PathOrUrl::from_os_str(OsStr::new("")).is_err());
    }

    #[test]
    fn base_url_works() {
        let mut u = BaseUrl::default();

        assert!(u.get().is_none());
        assert_eq!(u.get_ptr(), ptr::null());

        u.set(Url::parse("file:///example.txt").unwrap());

        assert_eq!(u.get().unwrap().as_str(), "file:///example.txt");

        unsafe {
            let p = u.get_ptr();
            let cstr = CStr::from_ptr(p);
            assert_eq!(cstr.to_str().unwrap(), "file:///example.txt");
        }
    }
}
