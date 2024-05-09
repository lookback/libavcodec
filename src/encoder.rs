use std::ffi::CStr;
use std::ptr;

use tracing::Level;

use crate::buffer::Buffer;
use crate::buffer::BufferableAvPacket;
use crate::Packet;

use super::sys::AVPixelFormat as PixelFormat;
use super::{av_log_set_callback, err_code_to_string, log_callback, set_log_level};
use super::{sys, Codec, CodecKind, Error, Frame};

pub struct Encoder {
    codec: *const sys::AVCodec,
    ctx: *mut sys::AVCodecContext,
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

            let codec = codec.ptr;

            let ctx: *mut sys::AVCodecContext = sys::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(Error::CreateContextFailed);
            }

            let enc = Encoder {
                codec,
                ctx,
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

    pub fn encode(
        &mut self,
        frame: impl Frame,
    ) -> Result<impl Iterator<Item = Result<impl Packet<[u8]>, Error>> + '_, Error> {
        let pts = self.pts_counter;
        self.pts_counter += 1;

        let mut fr = unsafe { sys::av_frame_alloc() };

        const MAX_PLANES: usize = sys::AV_NUM_DATA_POINTERS as usize;

        let mut planes = [ptr::null_mut(); MAX_PLANES];
        let mut strides = [0; MAX_PLANES];

        let plane_count = frame.plane_count();
        for i in 0..plane_count {
            planes[i] = frame.get_plane(i).as_ptr().cast_mut();
            strides[i] = frame.get_stride(i) as i32;
        }

        let width = frame.width() as i32;
        let height = frame.height() as i32;

        let rotation = frame.rotation();

        let buf = Buffer::new(frame.into_bufferable());
        let mut buffers = [ptr::null_mut(); MAX_PLANES];
        buffers[0] = buf.into();

        unsafe {
            (*fr).format = PixelFormat::AV_PIX_FMT_YUV420P as i32;
            (*fr).width = width;
            (*fr).height = height;
            (*fr).pts = pts;
            (*fr).data = planes;
            (*fr).linesize = strides;
            (*fr).buf = buffers;
        }

        let ret = unsafe { sys::avcodec_send_frame(self.ctx, fr) };

        unsafe {
            sys::av_frame_free(&mut fr);
        }

        if ret < 0 {
            return Err(Error::EncodeFrameFailed(ret, err_code_to_string(ret)));
        }

        Ok(PacketIterator {
            enc: Some(self),
            rotation,
        })
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

struct PacketIterator<'a> {
    enc: Option<&'a mut Encoder>,
    rotation: usize,
}

impl<'a> Iterator for PacketIterator<'a> {
    type Item = Result<EncodedPacket, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let enc = self.enc.as_ref()?;

        unsafe {
            let pkt = sys::av_packet_alloc();

            let ret = sys::avcodec_receive_packet(enc.ctx, pkt);
            if ret == sys::AVErrorEAgain || ret == sys::AVErrorEof {
                // Remove enc to stop producing packets.
                self.enc = None;
                return None;
            } else if ret < 0 {
                return Some(Err(Error::ReceivePacketFailed(
                    ret,
                    err_code_to_string(ret),
                )));
            }

            Some(Ok(EncodedPacket {
                pkt,
                rotation: self.rotation,
            }))
        }
    }
}

struct EncodedPacket {
    pkt: *mut sys::AVPacket,
    rotation: usize,
}

impl Packet<[u8]> for EncodedPacket {
    type AsBufferable = BufferableAvPacket;

    fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts((*self.pkt).data, (*self.pkt).size as usize) }
    }

    fn rotation(&self) -> usize {
        self.rotation
    }

    fn keyframe(&self) -> bool {
        unsafe { (*self.pkt).flags & sys::AV_PKT_FLAG_KEY as i32 > 0 }
    }

    fn into_bufferable(self) -> Self::AsBufferable {
        BufferableAvPacket(self.pkt)
    }
}

impl Drop for EncodedPacket {
    fn drop(&mut self) {
        unsafe {
            sys::av_packet_unref(self.pkt);
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
