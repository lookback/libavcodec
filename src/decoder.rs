use std::ffi::c_void;
use std::ptr;

use super::{
    av_log_set_callback, err_code_to_string, log_callback, set_log_level, sys, Codec, CodecKind,
    Error, FrameRef, PixelFormat,
};

use tracing::Level;

pub struct Decoder {
    ctx: *mut sys::AVCodecContext,
    /// Maps rotation values to the PTS of the incoming packet.
    pts_map: PtsMap,
}

unsafe impl Send for Decoder {}

struct PtsMap {
    map: [(i64, usize); 16],
    cur: usize,
}

pub trait DecoderPacket {
    /// Returns
    fn data(&mut self) -> PacketData;
    fn pts(&self) -> i64;
    fn rotation(&self) -> usize;
}

pub struct PacketData {
    inner: Box<[u8]>,
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
    pub fn decode(
        &mut self,
        packet: &mut dyn DecoderPacket,
    ) -> Result<impl Iterator<Item = Result<DecodedFrame, Error>> + '_, Error> {
        let mut pkt = unsafe {
            let pkt = sys::av_packet_alloc();
            if pkt.is_null() {
                return Err(Error::AlllocateFailed("av_malloc for Decoder::decode"));
            }

            let data = packet.data();
            // The buffer used for the packet is required to have
            // `sys::AV_INPUT_BUFFER_PADDING_SIZE` padding bytes, this is guaranteed for us by
            // `PacketData`.
            let len = data.inner.len();
            // This is a fat pointer i.e. 2 words
            let data_ptr = Box::into_raw(data.inner);
            let buf = sys::av_buffer_create(
                data_ptr.cast(),
                // This might look useless, but depending on the version of libavcodec used it's
                // required.
                #[allow(clippy::useless_conversion)]
                len.try_into().unwrap(),
                Some(free_boxed_slice),
                // We store the length of the slice as the opaque data so we can re-create the fat
                // pointer for freeing in `free_boxed_slice`.
                len as *mut c_void,
                0,
            );
            assert!(!buf.is_null());
            (*pkt).buf = buf;
            (*pkt).data = data_ptr.cast();
            (*pkt).pts = packet.pts();
            // This should be the size of the data without the padding
            (*pkt).size = (len as i32) - sys::AV_INPUT_BUFFER_PADDING_SIZE as i32;

            pkt
        };
        self.pts_map.set(packet.pts(), packet.rotation());
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

    pub fn timebase(&self) -> u32 {
        unsafe {
            let num = (*self.ctx).time_base.num;
            let den = (*self.ctx).time_base.den;
            // Assumption here that numerator is 1
            assert_eq!(num, 1);
            assert!(den.is_positive());
            den as u32
        }
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

impl PacketData {
    pub fn new(mut data: Vec<u8>) -> Self {
        data.extend_from_slice(&[0; sys::AV_INPUT_BUFFER_PADDING_SIZE as usize]);

        Self {
            inner: data.into_boxed_slice(),
        }
    }
}

impl From<&[u8]> for PacketData {
    fn from(value: &[u8]) -> Self {
        let new_size = value.len() + sys::AV_INPUT_BUFFER_PADDING_SIZE as usize;
        let mut vec = Vec::with_capacity(new_size);
        vec.extend_from_slice(value);

        Self::new(vec)
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
    pub fn pts(&self) -> i64 {
        // SAFETY: The pointer is valid while self is alive.
        unsafe { (*self.0).pts }
    }

    /// The rotation of the frame.
    pub fn rotation(&self) -> usize {
        // SAFETY: The pointer is valid while self is alive.
        unsafe { (*self.0).opaque as usize }
    }

    fn new() -> Self {
        let ptr = unsafe { sys::av_frame_alloc() };
        assert!(!ptr.is_null());

        Self(ptr)
    }
}

impl FrameRef for DecodedFrame {
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
        assert!(i < sys::AV_NUM_DATA_POINTERS as usize);

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

extern "C" fn free_boxed_slice(opaque: *mut c_void, data: *mut u8) {
    let len = opaque as usize;
    let ptr = std::ptr::slice_from_raw_parts_mut(data, len);

    // SAFETY: The pointer was originally created from a Box<[u8]> and the length was that from
    // said boxed slice.
    let _ = unsafe { Box::from_raw(ptr) };
}
