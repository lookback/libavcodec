use std::ffi::CStr;
use std::ptr;

use tracing::Level;

use super::sys::AVPixelFormat as PixelFormat;
use super::{av_log_set_callback, err_code_to_string, log_callback, set_log_level};
use super::{sys, Codec, CodecKind, Error, FrameRef, Packet};

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

impl Encoder {
    pub fn new(codec: &Codec, config: &EncoderConfig) -> Result<Self, Error> {
        unsafe {
            set_log_level(Level::DEBUG);
            av_log_set_callback(Some(log_callback));

            if codec.kind() != CodecKind::Encoder {
                return Err(Error::CodecIsNotEncoder(codec.name));
            }

            let frame =
                RawFrame::new(PixelFormat::AV_PIX_FMT_YUV420P, config.width, config.height)?;

            let codec = codec.ptr;

            let ctx: *mut sys::AVCodecContext = sys::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(Error::CreateContextFailed);
            }

            let enc = Encoder {
                codec,
                ctx,
                frame,
                pts_counter: 0,
            };

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
                (*ctx).pix_fmt = PixelFormat::AV_PIX_FMT_YUV420P;
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

            Ok(enc)
        }
    }

    pub fn width(&self) -> usize {
        unsafe { (*self.ctx).width as usize }
    }

    pub fn height(&self) -> usize {
        unsafe { (*self.ctx).height as usize }
    }

    pub fn codec(&self) -> Codec {
        unsafe { Codec::from_ptr(self.codec) }
    }

    pub fn encode(&mut self, frame: &dyn FrameRef) -> Result<PacketProducer, Error> {
        let pts = self.pts_counter;
        self.pts_counter += 1;

        self.frame.fill(frame, pts);

        unsafe {
            let ret = sys::avcodec_send_frame(self.ctx, self.frame.0);
            if ret < 0 {
                return Err(Error::EncodeFrameFailed(ret, err_code_to_string(ret)));
            }
        }

        Ok(PacketProducer {
            enc: self,
            pkt: Some(RawPacket::new()),
        })
    }
}

pub struct PacketProducer<'a> {
    enc: &'a mut Encoder,
    pkt: Option<RawPacket>,
}

impl<'a> PacketProducer<'a> {
    pub fn next<'b>(&'b mut self) -> Option<Result<Packet<'b>, Error>> {
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
    fn new(pix_fmt: PixelFormat, width: u32, height: u32) -> Result<Self, Error> {
        unsafe {
            let frame = sys::av_frame_alloc();
            (*frame).format = pix_fmt as i32;
            (*frame).width = width as i32;
            (*frame).height = height as i32;

            // let err = sys::av_frame_get_buffer(frame, 0);
            // if err < 0 {
            //     return Err(Error::AllocateFrameFailed(err, err_code_to_string(err)));
            // }

            // sys::av_frame_make_writable(frame);

            Ok(Self(frame))
        }
    }

    fn fill(&mut self, frame: &dyn FrameRef, pts: i64) {
        unsafe {
            let width = (*self.0).width as usize;
            let height = (*self.0).height as usize;

            assert_eq!(width, frame.width());
            assert_eq!(height, frame.height());

            (*self.0).pts = pts;

            let plane_count = frame.plane_count();

            let mut planes = [ptr::null_mut(); 8];
            let mut strides = [0; 8];

            for i in 0..plane_count {
                planes[i] = frame.get_plane(i).as_ptr().cast_mut();
                strides[i] = frame.get_stride(i) as i32;
            }

            (*self.0).data = planes;
            (*self.0).linesize = strides;
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

#[cfg(test)]
mod test {
    use super::*;

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
            profile: None,
            thread_count: 4,
            max_b_frames: 0,
            keyframe_distance: 300,
            x264_realtime: true,
        };
        Encoder::new(&codec, &config).unwrap();
    }
}
