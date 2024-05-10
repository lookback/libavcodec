use std::ffi::c_void;
use std::ptr;

use crate::buffer::Buffer;
use crate::buffer::BufferableAvBuffer;
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
    /// Counter for each packet to decode. Has no relation to "real" PTS and increases with 1
    /// for each call to decode()
    pts_counter: i64,
    /// Maps rotation values to the PTS of the incoming packet.
    pts_map: PtsMap,
}

unsafe impl Send for Decoder {}

struct PtsMap {
    map: [(i64, usize); 16],
    cur: usize,
}

/// A single frame of video or audio.
pub struct DecodedFrame(*mut sys::AVFrame);

impl Decoder {
    /// Create a new decoder
    pub fn new(codec: &Codec) -> Result<Self, Error> {
        set_log_level(Level::DEBUG);
        unsafe {
            av_log_set_callback(Some(log_callback));
        }

        if codec.kind() != CodecKind::Decoder {
            return Err(Error::CodecIsNotDecoder(codec.name));
        }

        let codec = codec.ptr;
        let ctx: *mut sys::AVCodecContext = unsafe { sys::avcodec_alloc_context3(codec) };
        if ctx.is_null() {
            return Err(Error::CreateContextFailed);
        }

        let dec = Decoder {
            ctx,
            pts_counter: 0,
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
    pub fn decode<Data: PaddedData>(
        &mut self,
        packet: impl Packet<Data>,
    ) -> Result<impl Iterator<Item = Result<DecodedFrame, Error>> + '_, Error> {
        let mut pkt = unsafe { sys::av_packet_alloc() };

        if pkt.is_null() {
            return Err(Error::AlllocateFailed("av_malloc for Decoder::decode"));
        }

        let pts = self.pts_counter;
        self.pts_counter += 1;

        self.pts_map.set(pts, packet.rotation());

        let data = packet.data();

        // The buffer used for the packet is required to have
        // `sys::AV_INPUT_BUFFER_PADDING_SIZE` padding bytes, this is guaranteed for us by
        // packet being of type `PaddedPacket`.
        let len = data.len();
        let data_ptr = data.as_ptr();

        let buf = Buffer::new(packet.into_bufferable());

        unsafe {
            (*pkt).buf = buf.into();
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
    /// Return the inner ptr for the frame.
    ///
    /// ## Safety
    /// This pointer **MUST** eventually be passed back to [`Frame::from_raw`] to avoid leaking
    /// memory.
    pub unsafe fn into_raw(mut self) -> *mut c_void {
        let ptr = self.0;
        self.0 = ptr::null_mut();

        ptr as *mut c_void
    }

    /// Create a [`Frame`] from a raw pointer obtained from [`Frame::into_raw`].
    ///
    /// ## Safety
    /// `ptr` **MUST** have been originally obtained from [`Frame::into_raw`]
    pub unsafe fn from_raw(ptr: *mut c_void) -> Self {
        assert!(!ptr.is_null());

        Self(ptr.cast())
    }

    /// The presentation timestamp for this frame.
    ///
    /// This is an internal value from the Decoder instance. Not a real PTS.
    fn pts(&self) -> i64 {
        // SAFETY: The pointer is valid while self is alive.
        unsafe { (*self.0).pts }
    }

    fn new() -> Self {
        let ptr = unsafe { sys::av_frame_alloc() };
        assert!(!ptr.is_null());

        Self(ptr)
    }
}

impl Frame for DecodedFrame {
    type AsBufferable = BufferableAvBuffer;

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

            std::slice::from_raw_parts(ptr, len as usize)
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

    fn into_bufferable(self) -> Self::AsBufferable {
        self.0.into()
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

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }

        unsafe {
            sys::av_frame_free(&mut self.0);
        }
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
