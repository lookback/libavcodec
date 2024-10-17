use std::ffi::c_void;
use std::ffi::CStr;
use std::ptr;

use tracing::Level;

use crate::Packet;
use crate::MAX_PLANES;

use super::sys::AVPixelFormat as PixelFormat;
use super::{av_log_set_callback, err_code_to_string, log_callback, set_log_level};
use super::{sys, Codec, CodecKind, Error, Frame};

pub struct Encoder {
    codec: *const sys::AVCodec,
    ctx: *mut sys::AVCodecContext,
    /// We don't take an external PTS in the encode() call, instead we use the FPS
    /// as time base and increase this counter by 1 for each frame.
    pts_counter: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    pub bitrate: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u8,
    pub thread_count: u32,
    pub max_b_frames: u32,
    pub keyframe_distance: u32,
}

impl Encoder {
    pub fn new(codec: &Codec, config: &EncoderConfig) -> Result<Self, Error> {
        unsafe {
            set_log_level(Level::TRACE);
            av_log_set_callback(Some(log_callback));

            if codec.kind() != CodecKind::Encoder {
                return Err(Error::CodecIsNotEncoder(codec.name()));
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
                (*ctx).thread_count = config.thread_count as i32;
                (*ctx).max_b_frames = config.max_b_frames as i32;
                (*ctx).gop_size = config.keyframe_distance as i32;
                (*ctx).flags = sys::AV_CODEC_FLAG_LOW_DELAY as i32;
                (*ctx).flags2 = sys::AV_CODEC_FLAG2_FAST as i32;
            }

            let is_nvidia = (*codec).name == c"h264_nvenc".as_ptr();
            let is_x264 = (*codec).id == sys::AVCodecID::AV_CODEC_ID_H264;
            let is_vpx =
                (*codec).name == c"libvpx".as_ptr() || (*codec).name == c"libvpx-vp9".as_ptr();

            if is_nvidia {
                const OPTS: &[(&CStr, &CStr)] = &[
                    (c"preset", c"llhp"),
                    (c"rc", c"vbr"),
                    (c"profile", c"baseline"),
                ];
                for (k, v) in OPTS {
                    // This sets options directly on nvidia
                    sys::av_opt_set((*ctx).priv_data, k.as_ptr(), v.as_ptr(), 0);
                }
            } else if is_x264 {
                // To be WebRTC compatible
                (*ctx).profile = sys::FF_PROFILE_H264_CONSTRAINED_BASELINE as i32;

                const OPTS: &[(&CStr, &CStr)] = &[
                    //
                    (c"preset", c"ultrafast"),
                    (c"tune", c"zerolatency"),
                ];
                for (k, v) in OPTS {
                    // This sets options directly on libx264
                    sys::av_opt_set((*ctx).priv_data, k.as_ptr(), v.as_ptr(), 0);
                }
            } else if is_vpx {
                // This sets options directly on libvpx
                sys::av_opt_set((*ctx).priv_data, c"lag_in_frames".as_ptr(), &0, 0);
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

    pub fn encode<T: Frame>(
        &mut self,
        frame: T,
        force_keyframe: bool,
    ) -> Result<impl Iterator<Item = Result<impl Packet<[u8]>, Error>> + '_, Error> {
        let pts = self.pts_counter;
        self.pts_counter += 1;

        let mut fr = unsafe { sys::av_frame_alloc() };

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
        let pic_type = if force_keyframe {
            sys::AVPictureType::AV_PICTURE_TYPE_I
        } else {
            sys::AVPictureType::AV_PICTURE_TYPE_NONE
        };
        unsafe { (*fr).pict_type = pic_type };

        let buffers = if let Some(buffers) = frame.as_avcodec_buf_ref() {
            buffers
        } else {
            let droppable = frame.into_droppable();
            let boxed = Box::new(droppable);
            let opaque = Box::into_raw(boxed);

            let buf = unsafe {
                sys::av_buffer_create(
                    ptr::null_mut(),
                    0,
                    Some(free_frame_droppable::<<T as Frame>::Droppable>),
                    opaque.cast(),
                    0,
                )
            };
            let mut buffers = [ptr::null_mut(); MAX_PLANES];
            buffers[0] = buf;
            buffers
        };

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

extern "C" fn free_frame_droppable<T>(opaque: *mut c_void, _data: *mut u8) {
    unsafe {
        let _ = Box::<T>::from_raw(opaque.cast());
    };
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
    type Droppable = Self;

    fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts((*self.pkt).data, (*self.pkt).size as usize) }
    }

    fn rotation(&self) -> usize {
        self.rotation
    }

    fn keyframe(&self) -> bool {
        unsafe { (*self.pkt).flags & sys::AV_PKT_FLAG_KEY as i32 > 0 }
    }

    fn pts(&self) -> i64 {
        unsafe { (*self.pkt).pts }
    }

    fn into_droppable(self) -> Self::Droppable {
        self
    }

    fn as_avcodec_buf_ref(&self) -> Option<*mut sys::AVBufferRef>
    where
        Self: Sized,
    {
        // SAFETY: The pointer is valid until we run the Drop trait.
        let buf = unsafe { (*self.pkt).buf };
        Some(buf)
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
            .find(|c| c.name() == "libx264")
            .unwrap();
        let config = EncoderConfig {
            bitrate: 2_000_000,
            width: 1024,
            height: 768,
            fps: 30,
            thread_count: 4,
            max_b_frames: 0,
            keyframe_distance: 300,
        };
        Encoder::new(&codec, &config).unwrap();
    }
}
