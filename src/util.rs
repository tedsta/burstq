use core::mem::MaybeUninit;

// Adapted from std
// https://github.com/rust-lang/rust/issues/79995
pub fn maybe_uninit_write_slice<'a, T>(this: &'a mut [MaybeUninit<T>], src: &[T]) -> &'a mut [T]
where
    T: Copy,
{
    // SAFETY: &[T] and &[MaybeUninit<T>] have the same layout
    let uninit_src: &[MaybeUninit<T>] = unsafe { core::mem::transmute(src) };

    this.copy_from_slice(uninit_src);

    // SAFETY: Valid elements have just been copied into `this` so it is initialized
    unsafe { &mut *(this as *mut [MaybeUninit<T>] as *mut [T]) }
}
