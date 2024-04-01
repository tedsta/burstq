//! A lock-free, `async`, multi-producer multi-consumer queue that natively supports batched enqueue
//! and dequeue operations. It supports both single and multi producer and consumer configurations.
//! It is based on the algorithm used by DPDK's `rte_ring` utility library.
//!
//! https://doc.dpdk.org/guides/prog_guide/ring_lib.html
//!
//! If you want to send and receive items in relatively large batches (say 32 items per batch) but
//! allow dynamic and uneven batch sizes, this is the crate for you.
//!
//! If you are sending and receiving one item at a time, or, more generally, have an exact batch
//! size that is the same at both senders and receivers, you are better off using one of the many
//! other channel crates (e.g. flume) - they will likely be faster.

pub use queue::{
    mpmc,
    Sender,
    Receiver,
    Write,
    Read,
};

mod queue;
mod loom_exports;
mod util;
