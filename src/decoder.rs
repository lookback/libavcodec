use std::ffi::c_void;
use std::ptr;

use crate::Packet;
use crate::PaddedData;
use crate::MAX_PLANES;

use super::{
    av_log_set_callback, err_code_to_string, log_callback, set_log_level, sys, Codec, CodecKind,
    Error, Frame, PixelFormat,
};

use tracing::Level;

pub struct Decoder {
    ctx: *mut sys::AVCodecContext,
    /// Maps rotation values to the PTS of the incoming packet.
    pts_map: PtsMap,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DecoderConfig {
    /// Number of decoding threads: 0 for auto (picked by the decoder).
    pub thread_count: u32,
    /// Type of threading.
    pub thread_type: DecodeThreadType,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DecodeThreadType {
    // Decode more than one frame at once
    Frame,
    // Decode more than one part of a single frame at once
    Slice,
    // Whatever is the default of the decoder
    #[default]
    Default,
}

struct PtsMap {
    map: [(i64, usize); 16],
    cur: usize,
}

/// A single frame of video or audio.
struct DecodedFrame(*mut sys::AVFrame);

// SAFETY: AVFrame is fine to send between threads.
unsafe impl Send for DecodedFrame {}
unsafe impl Sync for DecodedFrame {}

impl Decoder {
    /// Create a new decoder
    pub fn new(codec: &Codec, config: &DecoderConfig) -> Result<Self, Error> {
        set_log_level(Level::DEBUG);
        unsafe {
            av_log_set_callback(Some(log_callback));
        }

        if codec.kind() != CodecKind::Decoder {
            return Err(Error::CodecIsNotDecoder(codec.name()));
        }

        let codec = codec.ptr;
        let ctx: *mut sys::AVCodecContext = unsafe { sys::avcodec_alloc_context3(codec) };
        if ctx.is_null() {
            return Err(Error::CreateContextFailed);
        }

        unsafe {
            (*ctx).thread_count = config.thread_count as i32;
            match config.thread_type {
                DecodeThreadType::Frame => {
                    (*ctx).thread_type = sys::FF_THREAD_FRAME as i32;
                }
                DecodeThreadType::Slice => {
                    (*ctx).thread_type = sys::FF_THREAD_SLICE as i32;
                }
                DecodeThreadType::Default => {}
            };
        }

        let dec = Decoder {
            ctx,
            pts_map: PtsMap::default(),
        };

        // TODO: options

        let err = unsafe { sys::avcodec_open2(ctx, codec, ptr::null_mut()) };
        if err < 0 {
            return Err(Error::CodecOpenError(err, err_code_to_string(err)));
        }

        Ok(dec)
    }

    /// Decode some compressed data.
    ///
    /// Returns an iterator over the resulting frames.
    pub fn decode<T: Packet<Data>, Data: PaddedData>(
        &mut self,
        packet: T,
    ) -> Result<impl Iterator<Item = Result<impl Frame, Error>> + '_, Error> {
        let mut pkt = unsafe { sys::av_packet_alloc() };

        if pkt.is_null() {
            return Err(Error::AlllocateFailed("av_malloc for Decoder::decode"));
        }

        let pts = packet.pts();
        self.pts_map.set(pts, packet.rotation());

        let data = packet.data();

        // The buffer used for the packet is required to have
        // `sys::AV_INPUT_BUFFER_PADDING_SIZE` padding bytes, this is guaranteed for us by
        // packet being of type `PaddedData`.
        let len = data.len();
        let data_ptr = data.as_ptr();

        let buf = if let Some(buf) = packet.as_avcodec_buf_ref() {
            buf
        } else {
            let droppable = packet.into_droppable();
            let boxed = Box::new(droppable);
            let opaque = Box::into_raw(boxed);

            unsafe {
                sys::av_buffer_create(
                    data_ptr.cast_mut(),
                    // NB: The type expected here differs based on the underlying version of
                    // libavcoded. For newer version it's `usize`, but on older versions it's `i32`.
                    // Since we never want to create buffers of size 2GiB size we unwrap here, panicing
                    // on too larger buffers.
                    // Silence clippy since this conversion is not actually useless
                    #[allow(clippy::useless_conversion)]
                    len.try_into().unwrap(),
                    Some(free_packet_droppable::<<T as Packet<Data>>::Droppable>),
                    opaque.cast(),
                    0,
                )
            }
        };

        unsafe {
            (*pkt).buf = buf;
            (*pkt).data = data_ptr.cast_mut();
            (*pkt).pts = pts;
            // This should be the size of the data without the padding
            (*pkt).size = (len as i32) - sys::AV_INPUT_BUFFER_PADDING_SIZE as i32;
        }

        let ret = unsafe { sys::avcodec_send_packet(self.ctx, pkt) };

        // Regardless of errors we are done with this packet, parts of the packet might have been
        // retained in the decoder.
        //
        // This frees our `AVPacket` which **might** free the underlying buffer and call `free_boxed_slice`.
        // `avcodec_send_packet` is a allowed to increase the reference count of the buffer and
        // retain it until it has produced a frame. We rely on `libavcodec` to eventually free the
        // bufffer.
        // Relevant documentation snippet from `avcodec_send_packet`:
        //   The input AVPacket. Usually, this will be a single video frame, or several complete audio frames.
        //   Ownership of the packet remains with the caller, and the decoder will not write to the packet.
        //   The decoder may create a reference to the packet data (or copy it if the packet is not reference-counted).
        unsafe {
            sys::av_packet_free(&mut pkt);
        }
        if ret < 0 {
            return Err(Error::DecodePacketFailed(ret, err_code_to_string(ret)));
        }

        Ok(DecoderIterator {
            dec: self,
            ended: false,
        })
    }
}

extern "C" fn free_packet_droppable<T>(opaque: *mut c_void, _data: *mut u8) {
    unsafe {
        let _ = Box::<T>::from_raw(opaque.cast());
    };
}

struct DecoderIterator<'a> {
    dec: &'a mut Decoder,
    ended: bool,
}

impl<'a> Iterator for DecoderIterator<'a> {
    type Item = Result<DecodedFrame, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }

        let frame = DecodedFrame::new();

        let ret = unsafe { sys::avcodec_receive_frame(self.dec.ctx, frame.0) };
        if ret == sys::AVErrorEAgain || ret == sys::AVErrorEof {
            self.ended = true;
            return None;
        } else if ret < 0 {
            self.ended = true;
            return Some(Err(Error::ReceiveFrameFailed(ret, err_code_to_string(ret))));
        }
        unsafe {
            // This is a pointer but it's entirely opaque to libavcodec so we can use it to store
            // some arbitrary pointer sized data.
            (*frame.0).opaque = self.dec.pts_map.get(frame.pts()).unwrap_or(0) as *mut c_void;
        };

        Some(Ok(frame))
    }
}

impl DecodedFrame {
    fn new() -> Self {
        let ptr = unsafe { sys::av_frame_alloc() };
        assert!(!ptr.is_null());

        Self(ptr)
    }

    /// The presentation timestamp for this frame.
    ///
    /// This is an internal value from the Decoder instance. Not a real PTS.
    fn pts(&self) -> i64 {
        // SAFETY: The pointer is valid while self is alive.
        unsafe { (*self.0).pts }
    }
}

impl Frame for DecodedFrame {
    type Droppable = Self;

    fn width(&self) -> usize {
        // SAFETY: The pointer is valid while self is alive.
        unsafe { (*self.0).width as usize }
    }

    fn height(&self) -> usize {
        // SAFETY: The pointer is valid while self is alive.
        unsafe { (*self.0).height as usize }
    }

    fn plane_count(&self) -> usize {
        // SAFETY: The pointer is valid while self is alive.
        unsafe {
            assert_eq!(
                (*self.0).format,
                PixelFormat::AV_PIX_FMT_YUV420P as i32,
                "Only YUV420P is supported"
            );

            3
        }
    }

    fn get_plane(&self, i: usize) -> &[u8] {
        assert!(i < MAX_PLANES);

        // SAFETY:
        // * The pointer is valid while self is alive.
        // * The value calculated for `len` is correct
        unsafe {
            assert_eq!(
                (*self.0).format,
                PixelFormat::AV_PIX_FMT_YUV420P as i32,
                "Only YUV420P is supported"
            );
            let ptr: *mut u8 = (*self.0).data[i];

            let height = self.height();
            let stride = self.get_stride(i);
            let len = if i == 0 {
                // Y
                stride * height
            } else {
                // U & V
                stride * (height / 2)
            };

            std::slice::from_raw_parts(ptr, len)
        }
    }

    fn get_stride(&self, i: usize) -> usize {
        assert!(i < MAX_PLANES);

        // SAFETY: The pointer is valid while self is alive.
        unsafe {
            assert_eq!(
                (*self.0).format,
                PixelFormat::AV_PIX_FMT_YUV420P as i32,
                "Only YUV420P is supported"
            );

            (*self.0).linesize[i]
                .try_into()
                .expect("Non negative linesize")
        }
    }

    fn rotation(&self) -> usize {
        // SAFETY: The pointer is valid while self is alive.
        unsafe { (*self.0).opaque as usize }
    }

    fn pts(&self) -> i64 {
        self.pts()
    }

    fn into_droppable(self) -> Self::Droppable {
        self
    }

    fn as_avcodec_buf_ref(&self) -> Option<[*mut sys::AVBufferRef; MAX_PLANES]>
    where
        Self: Sized,
    {
        // SAFETY: The pointer is valid until we run the Drop trait.
        let buffers = unsafe { (*self.0).buf };
        Some(buffers)
    }
}

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        unsafe {
            sys::av_frame_free(&mut self.0);
        }
    }
}

impl PtsMap {
    fn set(&mut self, pts: i64, value: usize) {
        self.map[self.cur] = (pts, value);
        self.cur = (self.cur + 1) % self.map.len();
    }

    fn get(&self, pts: i64) -> Option<usize> {
        self.map
            .iter()
            .find(|(p, _)| *p == pts)
            .copied()
            .map(|(_, v)| v)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            sys::avcodec_free_context(&mut self.ctx);
        }
        self.ctx = ptr::null_mut();
    }
}

impl Default for PtsMap {
    fn default() -> Self {
        Self {
            map: [(-1, 0); 16],
            cur: 0,
        }
    }
}
