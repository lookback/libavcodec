use std::ffi::c_void;

use crate::sys::AVBufferRef;

use super::sys;

pub(crate) struct Buffer(Option<*mut sys::AVBufferRef>);

impl Buffer {
    /// Creates a buffer from some bufferable data
    pub fn new<B: Bufferable + Send + 'static>(bufferable: B) -> Self {
        let (ptr, len, free) = bufferable.into_raw();

        let opaque = Box::into_raw(Box::new(free));

        let buf = unsafe {
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

    fn into_raw(self) -> (*mut u8, usize, Self::Free) {
        let len = self.len();
        let ptr = Box::into_raw(self);
        (ptr.cast(), len, FreeBoxed(ptr))
    }
}

unsafe impl Bufferable for Vec<u8> {
    type Free = FreeBoxed<[u8]>;

    fn into_raw(self) -> (*mut u8, usize, Self::Free) {
        let len = self.len();
        let boxed = self.into_boxed_slice();
        let ptr = Box::into_raw(boxed);
        (ptr.cast(), len, FreeBoxed(ptr))
    }
}
