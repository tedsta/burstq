#[cfg(loom)]
#[allow(unused_imports)]
pub(crate) mod sync {
    pub(crate) use loom::sync::Arc;

    pub(crate) mod atomic {
        pub(crate) use loom::sync::atomic::AtomicUsize;
    }
}
#[cfg(not(loom))]
#[allow(unused_imports)]
pub(crate) mod sync {
    pub(crate) use alloc::sync::Arc;

    pub(crate) mod atomic {
        pub(crate) use core::sync::atomic::AtomicUsize;
    }
}

