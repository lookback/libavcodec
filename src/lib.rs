use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_void;
use std::ffi::CStr;
use std::ptr;

mod sys;
use buffer::Bufferable;
use sys::AVPixelFormat as PixelFormat;

mod encoder;
pub use encoder::{Encoder, EncoderConfig, EncoderProfile, PacketIterator};
mod decoder;
pub use decoder::{DecodedFrame, Decoder, DecoderPacket, PacketData};
mod error;
pub use error::Error;
mod buffer;

use tracing::Level;
use tracing::{debug, error, info, trace, warn};

pub trait Frame {
    type AsBufferable: Bufferable + Send + 'static;

    fn width(&self) -> usize;
    fn height(&self) -> usize;
    fn plane_count(&self) -> usize;
    fn get_plane(&self, i: usize) -> &[u8];
    fn get_stride(&self, i: usize) -> usize;

    fn into_bufferable(self) -> Self::AsBufferable;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codec {
    /// SAFETY: These values are allocated and initialised at link time and then valid until
    /// process exit.
    ptr: *const sys::AVCodec,
    pub name: &'static str,
    pub long_name: &'static str,
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
        unsafe fn str_of(ptr: *const c_char) -> &'static str {
            let name = CStr::from_ptr(ptr);
            name.to_str().expect("a utf-8 string")
        }

        Codec {
            ptr: codec,
            name: str_of((*codec).name),
            long_name: str_of((*codec).long_name),
        }
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
    pub fn av_log_set_callback(
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
    pub fn log_to_string(fmt: *const c_char, vargs: *const c_void) -> *mut c_char;
}
extern "C" {
    pub fn log_to_string_free(buffer: *mut c_char);
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_list_codecs() {
        println!(
            "{:#?}",
            Codec::list(CodecKind::Encoder)
                .map(|c| c.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_err_to_string() {
        println!("{:#?}", err_code_to_string(-22));
    }
}
