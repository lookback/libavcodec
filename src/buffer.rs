use std::ffi::c_void;
use std::ptr;

use crate::sys::AVBufferRef;

use super::sys;

pub(crate) struct Buffer(Option<*mut sys::AVBufferRef>);

impl Buffer {
    /// Creates a buffer from some bufferable data
    pub fn new<B: Bufferable + Send + 'static>(bufferable: B) -> Self {
        let buf = if let Some(v) = bufferable.reuse_av_buffer() {
            v
        } else {
            let (ptr, len, free) = bufferable.into_raw();

            let opaque = Box::into_raw(Box::new(free));

            unsafe {
                sys::av_buffer_create(
                    ptr.cast(),
                    // This might look useless, but depending on the version of libavcodec used it's
                    // required.
                    #[allow(clippy::useless_conversion)]
                    len.try_into().unwrap(),
                    Some(free_buffer::<B::Free>),
                    opaque.cast(),
                    0,
                )
            }
        };

        Self(Some(buf))
    }
}

impl Into<*mut AVBufferRef> for Buffer {
    fn into(mut self) -> *mut AVBufferRef {
        // This .take() is the whole reason we have Option<*mut AvBufferRef)
        self.0.take().expect("existing pointer for alive Buffer")
    }
}

impl Clone for Buffer {
    fn clone(&self) -> Self {
        let ptr = self.0.expect("existing pointer for alive Buffer");
        let ptr = unsafe { sys::av_buffer_ref(ptr) };
        Self(Some(ptr))
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // If pointer is already gone, we used .into()
        if let Some(mut ptr) = self.0.take() {
            unsafe { sys::av_buffer_unref(&mut ptr) }
        }
    }
}

unsafe extern "C" fn free_buffer<T>(opaque: *mut c_void, _: *mut u8) {
    let _: Box<T> = unsafe { Box::from_raw(opaque.cast()) };
}

/// Something that can be turned into pointer + length and later freed.
///
/// SAFETY: The implementation of [`Bufferable`] must guarantee the pointer
/// produced by [`Bufferable::into_raw()`] is not shared by the time it is handed
/// over to [`Buffer::new()`].
pub unsafe trait Bufferable {
    /// Whatever this type is, it must free underlying data the pointer/len points to.
    type Free: Drop;

    /// Consume and turn into a pointer/length + the mechanism for freeing.
    fn into_raw(self) -> (*mut u8, usize, Self::Free);

    /// Internal shortcircuit to reuse something that already is an `AVBuffer`.
    #[doc(hidden)]
    fn reuse_av_buffer(&self) -> Option<*mut sys::AVBufferRef> {
        None
    }
}

/// Free some boxed data, whatever it might be.
#[doc(hidden)]
pub struct FreeBoxed<T: ?Sized>(*mut T);

unsafe impl<T: ?Sized> Send for FreeBoxed<T> {}

impl<T: ?Sized> Drop for FreeBoxed<T> {
    fn drop(&mut self) {
        let _ = unsafe { Box::from_raw(self.0) };
    }
}

unsafe impl Bufferable for Box<[u8]> {
    type Free = FreeBoxed<[u8]>;

    fn into_raw(mut self) -> (*mut u8, usize, Self::Free) {
        let len = self.len();
        let data_ptr = self.as_mut_ptr();
        let ptr = Box::into_raw(self);
        (data_ptr, len, FreeBoxed(ptr))
    }
}

unsafe impl Bufferable for Vec<u8> {
    type Free = FreeBoxed<[u8]>;

    fn into_raw(mut self) -> (*mut u8, usize, Self::Free) {
        let len = self.len();
        let data_ptr = self.as_mut_ptr();
        let boxed = self.into_boxed_slice();
        let ptr = Box::into_raw(boxed);
        (data_ptr, len, FreeBoxed(ptr))
    }
}

unsafe impl Bufferable for () {
    type Free = EmptyDrop;

    fn into_raw(self) -> (*mut u8, usize, Self::Free) {
        (ptr::null_mut(), 0, EmptyDrop)
    }
}

#[doc(hidden)]
pub struct EmptyDrop;

impl Drop for EmptyDrop {
    fn drop(&mut self) {}
}

#[doc(hidden)]
pub struct BufferableAvBuffer(*mut sys::AVBufferRef);

impl BufferableAvBuffer {
    pub fn new(p: *mut sys::AVBufferRef) -> Self {
        unsafe { sys::av_buffer_ref(p) };
        BufferableAvBuffer(p)
    }
}

unsafe impl Send for BufferableAvBuffer {}

unsafe impl Bufferable for BufferableAvBuffer {
    type Free = EmptyDrop;

    fn into_raw(self) -> (*mut u8, usize, Self::Free) {
        // This is unused, because we implement reuse_av_buffer()
        unreachable!()
    }

    fn reuse_av_buffer(&self) -> Option<*mut sys::AVBufferRef> {
        Some(self.0)
    }
}

impl From<*mut sys::AVFrame> for BufferableAvBuffer {
    fn from(v: *mut sys::AVFrame) -> Self {
        let bufs = unsafe { (*v).buf };

        let first_null = bufs[0].is_null();
        let rest_null = bufs[1..].iter().all(|b| b.is_null());

        assert!(!first_null, "Expected first AVFrame buffer to be not null");
        assert!(rest_null, "Expected the rest of AVFrame buffers to be null");

        BufferableAvBuffer::new(bufs[0])
    }
}

impl From<*mut sys::AVPacket> for BufferableAvBuffer {
    fn from(v: *mut sys::AVPacket) -> Self {
        let buf = unsafe { (*v).buf };
        BufferableAvBuffer::new(buf)
    }
}
