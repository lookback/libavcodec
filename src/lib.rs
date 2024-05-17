use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_void;
use std::ffi::CStr;
use std::ptr;

mod sys;
use buffer::FreeBoxed;
use sys::AVPixelFormat as PixelFormat;

mod encoder;
pub use encoder::{Encoder, EncoderConfig};

mod decoder;
pub use decoder::Decoder;

mod error;
pub use error::Error;

mod buffer;
pub use buffer::Bufferable;

use tracing::Level;
use tracing::{debug, error, info, trace, warn};

const MAX_PLANES: usize = sys::AV_NUM_DATA_POINTERS as usize;

pub trait Frame {
    type AsBufferable: Bufferable + Send + 'static;

    fn width(&self) -> usize;
    fn height(&self) -> usize;
    fn plane_count(&self) -> usize;
    fn get_plane(&self, i: usize) -> &[u8];
    fn get_stride(&self, i: usize) -> usize;

    fn rotation(&self) -> usize;

    fn into_bufferable(self) -> Self::AsBufferable;

    /// Consume self and turn into a pointer/length + the mechanism for freeing.
    fn into_raw(
        self,
    ) -> (
        *mut u8,
        usize,
        <<Self as Frame>::AsBufferable as Bufferable>::Free,
    )
    where
        Self: Sized,
    {
        let bufferable = self.into_bufferable();
        bufferable.into_raw()
    }
}

pub trait Packet<Data>
where
    Data: ?Sized,
{
    type AsBufferable: Bufferable + Send + 'static;

    fn data(&self) -> &Data;
    fn rotation(&self) -> usize;
    fn keyframe(&self) -> bool;

    fn into_bufferable(self) -> Self::AsBufferable;

    /// Consume self and turn into a pointer/length + the mechanism for freeing.
    fn into_raw(
        self,
    ) -> (
        *mut u8,
        usize,
        <<Self as Packet<Data>>::AsBufferable as Bufferable>::Free,
    )
    where
        Self: Sized,
    {
        let bufferable = self.into_bufferable();
        bufferable.into_raw()
    }
}

#[allow(clippy::len_without_is_empty)]
pub trait PaddedData {
    fn len(&self) -> usize;
    fn as_ptr(&self) -> *const u8;
}

pub struct PaddedDataImpl(Box<[u8]>);

impl From<Vec<u8>> for PaddedDataImpl {
    fn from(mut value: Vec<u8>) -> Self {
        let new_len = value.len() + sys::AV_INPUT_BUFFER_PADDING_SIZE as usize;
        value.resize(new_len, 0);

        PaddedDataImpl(value.into_boxed_slice())
    }
}

impl From<&[u8]> for PaddedDataImpl {
    fn from(value: &[u8]) -> Self {
        let new_len = value.len() + sys::AV_INPUT_BUFFER_PADDING_SIZE as usize;
        let mut vec = Vec::with_capacity(new_len);
        vec.extend_from_slice(value);
        vec.extend_from_slice(&[0; sys::AV_INPUT_BUFFER_PADDING_SIZE as usize]);
        PaddedDataImpl(vec.into_boxed_slice())
    }
}

impl PaddedData for PaddedDataImpl {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn as_ptr(&self) -> *const u8 {
        self.0.as_ptr()
    }
}

/// SAFETY: We have no pointers to the inner Box<[u8]> and will relinquish the memory in `into_raw`
/// below.
unsafe impl Bufferable for PaddedDataImpl {
    type Free = FreeBoxed<[u8]>;

    fn into_raw(self) -> (*mut u8, usize, Self::Free) {
        let len = self.0.len();
        let ptr = Box::into_raw(self.0);
        // SAFETY: We have exclusive ownership of `self` and have the only pointer to the memory, which
        // we are now relinquishing.
        let free = unsafe { FreeBoxed::new(ptr) };

        (ptr.cast(), len, free)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codec {
    /// SAFETY: These values are allocated and initialised at link time and then valid until
    /// process exit.
    ptr: *const sys::AVCodec,
}

unsafe impl Send for Codec {}
unsafe impl Sync for Codec {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecKind {
    Encoder,
    Decoder,
}

impl Codec {
    pub fn list(kind: CodecKind) -> impl Iterator<Item = Codec> {
        CodecIterator(Some(ptr::null_mut()), kind)
    }
}

fn err_code_to_string(code: i32) -> String {
    let mut buf = [0_u8; sys::AV_ERROR_MAX_STRING_SIZE as usize];
    let r = unsafe { sys::av_strerror(code, buf.as_mut_ptr().cast(), buf.len()) };
    if r < 0 {
        return String::new();
    }
    let c = CStr::from_bytes_until_nul(&buf).expect("a valid CStr");
    c.to_string_lossy().to_string()
}

struct CodecIterator(Option<*mut c_void>, CodecKind);

unsafe fn str_of(ptr: *const c_char) -> &'static str {
    let name = CStr::from_ptr(ptr);
    name.to_str().expect("a utf-8 string")
}

impl Iterator for CodecIterator {
    type Item = Codec;

    fn next(&mut self) -> Option<Self::Item> {
        let opaque = self.0.as_mut()?;

        let want_encoder = self.1 == CodecKind::Encoder;

        unsafe {
            let codec = loop {
                let codec = sys::av_codec_iterate(opaque);

                if codec.is_null() {
                    self.0 = None;
                    return None;
                }

                let is_encoder = sys::av_codec_is_encoder(codec) != 0;

                if want_encoder != is_encoder {
                    continue;
                }

                if (*codec).type_ == sys::AVMediaType::AVMEDIA_TYPE_VIDEO {
                    break codec;
                }
            };

            Some(Codec::from_ptr(codec))
        }
    }
}

impl Codec {
    /// Create a [`Codec`] from a pointer.
    ///
    /// **SAFETY:** The caller must guarantee that the pointer is valid until the process ends.
    /// This is the case for pointers returned by functions like `av_codec_iterate`.
    unsafe fn from_ptr(codec: *const sys::AVCodec) -> Self {
        Codec { ptr: codec }
    }

    pub fn name(&self) -> &'static str {
        unsafe { str_of((*self.ptr).name) }
    }

    pub fn long_name(&self) -> &'static str {
        unsafe { str_of((*self.ptr).long_name) }
    }

    pub fn is_hw(&self) -> bool {
        unsafe { ((*self.ptr).capabilities & sys::AV_CODEC_CAP_HARDWARE as i32) > 0 }
    }

    pub fn kind(&self) -> CodecKind {
        unsafe {
            if sys::av_codec_is_encoder(self.ptr) != 0 {
                CodecKind::Encoder
            } else {
                CodecKind::Decoder
            }
        }
    }
}

fn set_log_level(level: Level) {
    let l = match level {
        Level::TRACE => sys::AV_LOG_TRACE,
        Level::DEBUG => sys::AV_LOG_DEBUG,
        Level::INFO => sys::AV_LOG_INFO,
        Level::WARN => sys::AV_LOG_WARNING,
        Level::ERROR => sys::AV_LOG_ERROR,
    };
    unsafe {
        sys::av_log_set_level(l as i32);
    }
}

unsafe extern "C" fn log_callback(
    _ptr: *mut c_void,
    level: c_int,
    fmt: *const c_char,
    vargs: *const c_void,
) {
    let buffer = log_to_string(fmt, vargs);
    if buffer.is_null() {
        error!("Failed to convert log_callback to string");
    }
    let cs = CStr::from_ptr(buffer);
    let s = cs.to_string_lossy();

    // The c-side fmt has a \n we don't want.
    let s = s.trim();

    let level = level as u32;
    if level <= sys::AV_LOG_ERROR {
        error!("{}", s);
    } else if level <= sys::AV_LOG_WARNING {
        warn!("{}", s);
    } else if level <= sys::AV_LOG_INFO {
        info!("{}", s);
    } else if level <= sys::AV_LOG_DEBUG {
        debug!("{}", s);
    } else if level <= sys::AV_LOG_TRACE {
        trace!("{}", s);
    }

    log_to_string_free(buffer);
}

// This is here because macOS bindgen makes a different type to Linux. Ultimately
// arg4 is "just a pointer".
// expected fn pointer `unsafe extern "C" fn(_, _, _, *mut __va_list_tag)`
// found       fn item `unsafe extern "C" fn(_, _, _, [__va_list_tag; 1]) {log_callback}`
extern "C" {
    pub(crate) fn av_log_set_callback(
        callback: ::std::option::Option<
            unsafe extern "C" fn(
                arg1: *mut c_void,
                arg2: c_int,
                arg3: *const c_char,
                arg4: *const c_void,
            ),
        >,
    );
}

extern "C" {
    pub(crate) fn log_to_string(fmt: *const c_char, vargs: *const c_void) -> *mut c_char;
}
extern "C" {
    pub(crate) fn log_to_string_free(buffer: *mut c_char);
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_list_codecs() {
        println!(
            "{:#?}",
            Codec::list(CodecKind::Encoder)
                .map(|c| c.name())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_err_to_string() {
        println!("{:#?}", err_code_to_string(-22));
    }
}
