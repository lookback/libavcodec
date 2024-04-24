use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_void;
use std::ffi::CStr;
use std::ptr;

mod sys;
pub use sys::AVPixelFormat as PixelFormat;

mod error;
pub use error::Error;
use tracing::Level;
use tracing::{debug, error, info, trace, warn};

pub struct Encoder {
    codec: *const sys::AVCodec,
    ctx: *mut sys::AVCodecContext,
    frame: RawFrame,
    pts_counter: i64,
}

unsafe impl Send for Encoder {}
unsafe impl Sync for Encoder {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    pub bitrate: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u8,
    pub pix_fmt: PixelFormat,
    pub profile: Option<EncoderProfile>,
    pub thread_count: u32,
    pub max_b_frames: u32,
    pub keyframe_distance: u32,
    pub x264_realtime: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderProfile {
    H264Constrained,
    H264Intra,
    H264Baseline,
    H264ConstrainedBaseline,
    H264Main,
    H264Extended,
    H264High,
}

impl EncoderProfile {
    fn as_avcodec(&self) -> u32 {
        match self {
            Self::H264Constrained => sys::FF_PROFILE_H264_CONSTRAINED,
            Self::H264Intra => sys::FF_PROFILE_H264_INTRA,
            Self::H264Baseline => sys::FF_PROFILE_H264_BASELINE,
            Self::H264ConstrainedBaseline => sys::FF_PROFILE_H264_CONSTRAINED_BASELINE,
            Self::H264Main => sys::FF_PROFILE_H264_MAIN,
            Self::H264Extended => sys::FF_PROFILE_H264_EXTENDED,
            Self::H264High => sys::FF_PROFILE_H264_HIGH,
        }
    }
}

pub trait AvFrame {
    fn plane_count(&self) -> usize;
    fn get_plane(&self, i: usize) -> &[u8];
    fn get_stride(&self, i: usize) -> usize;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codec {
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

impl Encoder {
    pub fn new(codec: &Codec, config: &EncoderConfig) -> Result<Self, Error> {
        unsafe {
            set_log_level(Level::DEBUG);
            sys::av_log_set_callback(Some(log_callback));

            if codec.kind() != CodecKind::Encoder {
                return Err(Error::CodecIsNotEncoder(codec.name));
            }

            let codec = codec.ptr;

            let ctx = sys::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(Error::CreateContextFailed);
            }

            {
                (*ctx).bit_rate = config.bitrate as i64;
                (*ctx).width = config.width as i32;
                (*ctx).height = config.height as i32;
                (*ctx).time_base = sys::AVRational {
                    num: 1,
                    den: config.fps as i32,
                };
                (*ctx).framerate = sys::AVRational {
                    num: config.fps as i32,
                    den: 1,
                };
                (*ctx).pix_fmt = config.pix_fmt;
                if let Some(profile) = config.profile {
                    (*ctx).profile = profile.as_avcodec() as i32;
                }
                (*ctx).thread_count = config.thread_count as i32;
                (*ctx).max_b_frames = config.max_b_frames as i32;
                (*ctx).gop_size = config.keyframe_distance as i32;
            }

            if config.x264_realtime {
                // This sets options directly on libx264
                if (*codec).id == sys::AVCodecID::AV_CODEC_ID_H264 {
                    const OPTS: &[(&CStr, &CStr)] = &[
                        //
                        (c"preset", c"ultrafast"),
                        (c"tune", c"zerolatency"),
                    ];
                    for (k, v) in OPTS {
                        sys::av_opt_set((*ctx).priv_data, k.as_ptr(), v.as_ptr(), 0);
                    }
                }
            }

            let err = sys::avcodec_open2(ctx, codec, ptr::null_mut());
            if err < 0 {
                return Err(Error::CodecOpenError(err, err_code_to_string(err)));
            }

            let frame = sys::av_frame_alloc();
            (*frame).format = config.pix_fmt as i32;
            (*frame).width = config.width as i32;
            (*frame).height = config.width as i32;
            (*frame).time_base = (*ctx).time_base;

            let err = sys::av_frame_get_buffer(frame, 0);
            if err < 0 {
                return Err(Error::AllocateFrameFailed(err, err_code_to_string(err)));
            }

            sys::av_frame_make_writable(frame);

            let frame = RawFrame(frame);

            Ok(Encoder {
                codec,
                ctx,
                frame,
                pts_counter: 0,
            })
        }
    }

    pub fn width(&self) -> usize {
        unsafe { (*self.ctx).width as usize }
    }

    pub fn height(&self) -> usize {
        unsafe { (*self.ctx).height as usize }
    }

    pub fn codec(&self) -> Codec {
        Codec::from_ptr(self.codec)
    }

    pub fn encode(
        &mut self,
        frame: &dyn AvFrame,
    ) -> Result<impl Iterator<Item = Result<Packet, Error>>, Error> {
        let pts = self.pts_counter;
        self.pts_counter += 1;

        self.frame.fill(frame, pts);

        unsafe {
            let ret = sys::avcodec_send_frame(self.ctx, self.frame.0);
            if ret < 0 {
                return Err(Error::EncodeFrameFailed(ret, err_code_to_string(ret)));
            }
        }

        Ok(EncoderIterator {
            enc: self,
            pkt: Some(RawPacket::new()),
        })
    }
}

fn err_code_to_string(code: i32) -> String {
    let mut buf = [0_u8; sys::AV_ERROR_MAX_STRING_SIZE as usize];
    let r = unsafe { sys::av_strerror(code, buf.as_mut_ptr().cast(), buf.len()) };
    if r < 0 {
        eprintln!("av_strerror failed: {}", r);
        return String::new();
    }
    let c = CStr::from_bytes_until_nul(&buf).expect("a valid CStr");
    c.to_string_lossy().to_string()
}

struct EncoderIterator<'a> {
    enc: &'a mut Encoder,
    pkt: Option<RawPacket>,
}

impl<'a> Iterator for EncoderIterator<'a> {
    type Item = Result<Packet<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let pkt = self.pkt.as_mut()?;

        unsafe {
            let ret = sys::avcodec_receive_packet(self.enc.ctx, pkt.0);
            if ret == sys::AVErrorEAgain || ret == sys::AVErrorEof {
                self.pkt = None;
                return None;
            } else if ret < 0 {
                return Some(Err(Error::ReceivePacketFailed(
                    ret,
                    err_code_to_string(ret),
                )));
            }

            let data = std::slice::from_raw_parts((*pkt.0).data, (*pkt.0).size as usize);
            let keyframe = (*pkt.0).flags & sys::AV_PKT_FLAG_KEY as i32 > 0;

            Some(Ok(Packet {
                pkt: pkt.0,
                data,
                keyframe,
            }))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            sys::avcodec_free_context(&mut self.ctx);
            self.ctx = ptr::null_mut();
        }
    }
}

struct RawFrame(*mut sys::AVFrame);

impl RawFrame {
    fn fill(&mut self, frame: &dyn AvFrame, pts: i64) {
        unsafe {
            let width = (*self.0).width as usize;
            let height = (*self.0).height as usize;
            (*self.0).pts = pts;

            let planes = frame.plane_count();

            for i in 0..planes {
                let src_base = frame.get_plane(i);
                let src_stride = frame.get_stride(i);

                let dst_plane = (*self.0).data[i];
                let dst_stride = (*self.0).linesize[i];

                for y in 0..height {
                    let src_offs = y * src_stride;
                    let src_range = src_offs..(src_offs + width);
                    let src = &src_base[src_range];

                    let dst_offs = y * dst_stride as usize;
                    let dst_ptr = dst_plane.add(dst_offs);
                    let dst = std::slice::from_raw_parts_mut(dst_ptr, width);

                    dst.copy_from_slice(src);
                }
            }
        }
    }
}

impl Drop for RawFrame {
    fn drop(&mut self) {
        unsafe {
            sys::av_frame_free(&mut self.0);
            self.0 = ptr::null_mut();
        }
    }
}

struct RawPacket(*mut sys::AVPacket);

impl RawPacket {
    pub fn new() -> Self {
        unsafe {
            let pkt = sys::av_packet_alloc();
            (*pkt).data = ptr::null_mut();
            (*pkt).size = 0;
            sys::av_init_packet(pkt);
            Self(pkt)
        }
    }
}

impl Drop for RawPacket {
    fn drop(&mut self) {
        unsafe {
            sys::av_packet_free(&mut self.0);
            self.0 = ptr::null_mut();
        }
    }
}

pub struct Packet<'a> {
    pkt: *mut sys::AVPacket,
    pub data: &'a [u8],
    pub keyframe: bool,
}

impl Drop for Packet<'_> {
    fn drop(&mut self) {
        unsafe {
            // Clean the packet for reusing.
            sys::av_packet_unref(self.pkt);
        }
    }
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
    fn from_ptr(codec: *const sys::AVCodec) -> Self {
        unsafe {
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
    vargs: sys::va_list,
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

extern "C" {
    pub fn log_to_string(
        fmt: *const ::std::os::raw::c_char,
        vargs: sys::va_list,
    ) -> *mut ::std::os::raw::c_char;
}
extern "C" {
    pub fn log_to_string_free(buffer: *mut ::std::os::raw::c_char);
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

    #[test]
    fn test_instantiate_encoder() {
        let codec = Codec::list(CodecKind::Encoder)
            .find(|c| c.name == "libx264")
            .unwrap();
        let config = EncoderConfig {
            bitrate: 2_000_000,
            width: 1024,
            height: 768,
            fps: 30,
            pix_fmt: PixelFormat::AV_PIX_FMT_YUV420P,
            profile: None,
            thread_count: 4,
            max_b_frames: 0,
            keyframe_distance: 300,
            x264_realtime: true,
        };
        Encoder::new(&codec, &config).unwrap();
    }
}