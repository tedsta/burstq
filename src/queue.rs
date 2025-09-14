use core::{
    cell::Cell,
    mem::{ManuallyDrop, MaybeUninit},
    sync::atomic::Ordering,
};

#[cfg(not(feature = "std"))]
use alloc::boxed::Box;

use async_event::Event;
use crossbeam_utils::{Backoff, CachePadded};

use crate::loom_exports::sync::{
    atomic::AtomicUsize,
    Arc,
};

/// An error that may be returned when the queue is shutting down - when all senders or all
/// receivers have been dropped.
#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    Shutdown,
}

/// A sending end of a channel.
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

/// A receiving end of a channel.
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

struct Slinky {
    head: AtomicUsize,
    tail: AtomicUsize,
}

#[repr(C)]
struct Shared<T> {
    capacity: usize,

    write_event: Event,
    consume_event: Event,

    sender_count: AtomicUsize,
    receiver_count: AtomicUsize,

    prod: CachePadded<Slinky>,
    cons: CachePadded<Slinky>,

    data: *mut MaybeUninit<T>,
}

/// A handle to write to some reserved region of the queue. The reserved region must be written in
/// its entirety before this handle is dropped.
pub struct Write<'a, T> {
    front: &'a mut [MaybeUninit<T>],
    back: Option<&'a mut [MaybeUninit<T>]>,
}

/// A handle to read to some reserved region of the queue.
pub struct Read<'a, T> {
    front: &'a mut [MaybeUninit<T>],
    back: Option<&'a mut [MaybeUninit<T>]>,
}

/// Create a new bounded mpmc queue.
pub fn mpmc<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    // We add 1 to the user-specified capacity since one slot in the queue will always be unusable
    // so that we can discern between the queue being completely full and completely empty.
    let capacity = capacity + 1;

    let write_event = Event::new();
    let consume_event = Event::new();

    let data: *mut MaybeUninit<T> =
        core::mem::ManuallyDrop::new(
            (0..capacity)
                .map(|_| MaybeUninit::<T>::uninit())
                .collect::<Box<[_]>>()
        )
        .as_mut_ptr();

    let shared = Arc::new(Shared {
        prod: CachePadded::new(Slinky {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }),
        cons: CachePadded::new(Slinky {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }),
        capacity,
        data,
        write_event,
        consume_event,
        sender_count: AtomicUsize::new(1),
        receiver_count: AtomicUsize::new(1),
    });

    let producer = Sender { shared: shared.clone() };
    let consumer = Receiver { shared };

    (producer, consumer)
}

const SHUTDOWN_FLAG: usize = 1 << (usize::BITS - 1);
const SEQ_MASK: usize = 0xFFFF << (usize::BITS - 1 - 16);
const SEQ_SHIFT: u32 = usize::BITS - 1 - 16;

impl<T> Sender<T> {
    /// Asynchronously send up to `max_burst_len` values. If the queue is completely full, the
    /// returned Future will yield to the async runtime until at least one value can be sent.
    ///
    /// The provided closure will be called with a `Write` reservation handle. The closure must
    /// write the reservation in its entirety. This is guaranteed if using `Write::write_slice`.
    /// But if the unsafe `Write::write_at` method is used, you **must** ensure that `write_at` is
    /// called exactly once at **every** index of the `Write` - failing to do so will result in
    /// undefined behavior when receivers attempt to read the uninitialized values.
    pub async fn send(
        &self,
        max_burst_len: usize,
        write_fn: impl FnOnce(Write<T>),
    ) -> Result<usize, Error> {
        if max_burst_len == 0 {
            return Ok(0);
        }

        let write_fn = &mut ManuallyDrop::new(write_fn);

        let result = self.shared.consume_event
            .wait_until(|| {
                match self.try_send_inner(max_burst_len, write_fn) {
                    Ok(0) => None,
                    Ok(n) => {
                        Some(Ok(n))
                    }
                    Err(e) => {
                        // SAFETY: write_fn is guaranteed to not have been taken yet if
                        // try_send_inner returns Ok(0) or Err(_).
                        unsafe { let _ = ManuallyDrop::take(write_fn); }
                        Some(Err(e))
                    }
                }
            })
            .await;
        if result.is_ok() {
            self.shared.write_event.notify_all();
        }
        result
    }

    /// Attempt to send up to `max_burst_len` values. If the queue is completely full, this
    /// method will return immediately.
    ///
    /// See `Sender::send` for details about the `write_fn` parameter.
    pub fn try_send(
        &self,
        max_burst_len: usize,
        write_fn: impl FnOnce(Write<T>),
    ) -> Result<usize, Error> {
        let n = self.try_send_quiet(max_burst_len, write_fn)?;
        if n > 0 {
            self.shared.write_event.notify_all();
        }
        Ok(n)
    }

    /// Attempt to send up to `max_burst_len` values. If the queue is completely full, this
    /// method will return immediately.
    ///
    /// This method doesn't notify async Receivers that new values are available. It should be used
    /// if and only if Receivers only ever receive with `try_recv` or `try_recv_quiet`.
    ///
    /// See `Sender::send` for details about the `write_fn` parameter.
    pub fn try_send_quiet(
        &self,
        max_burst_len: usize,
        write_fn: impl FnOnce(Write<T>),
    ) -> Result<usize, Error> {
        let write_fn = &mut ManuallyDrop::new(write_fn);
        self.try_send_inner(max_burst_len, write_fn)
            .map_err(|e| {
                // SAFETY: write_fn is guaranteed to not have been taken yet if try_send_inner
                // returns Err(_).
                unsafe { let _ = ManuallyDrop::take(write_fn); }
                e
            })
    }

    #[inline]
    pub fn try_send_inner(
        &self,
        max_burst_len: usize,
        write_fn: &mut ManuallyDrop<impl FnOnce(Write<T>)>,
    ) -> Result<usize, Error> {
        if max_burst_len == 0 {
            return Ok(0);
        }

        // See the ordering used on failure in the compare exchange operation below - this uses
        // Acquire ordering for the same reason.
        let mut prod_head = self.shared.prod.head.load(Ordering::Acquire);
        let mut prod_next: usize;
        let mut burst_len: usize;

        let backoff = Backoff::new();
        loop {
            if prod_head & SHUTDOWN_FLAG != 0 {
                return Err(Error::Shutdown);
            }
            prod_head &= !SHUTDOWN_FLAG;

            let seq = prod_head & SEQ_MASK;
            prod_head &= !SEQ_MASK;

            let cons_tail = self.shared.cons.tail.load(Ordering::Relaxed);

            let free_entries =
                if cons_tail > prod_head {
                    cons_tail - prod_head - 1
                } else {
                    self.shared.capacity - 1 - prod_head + cons_tail
                };

            if free_entries == 0 {
                // Compare exchange to be sure that we have the latest values and there really is
                // no space.
                //
                // This improves performance (measured +25% spsc throughput) in non-poll-mode
                // settings where this code path leads to registering this task in a wait queue,
                // which is expensive.
                if let Err(new_prod_head) = self.shared.prod.head.compare_exchange(
                    prod_head | seq as usize,
                    prod_head | seq as usize,
                    Ordering::Release,
                    Ordering::Acquire,
                ) {
                    prod_head = new_prod_head;
                    continue;
                }
                return Ok(0);
            }

            burst_len = core::cmp::min(max_burst_len, free_entries);
            prod_next = (prod_head + burst_len) % self.shared.capacity;

            match self.shared.prod.head.compare_exchange_weak(
                prod_head | seq as usize,
                prod_next | ((seq + (burst_len << SEQ_SHIFT)) & SEQ_MASK),
                // On success, we need to ensure that subsequent producers that observe the new
                // `prod_head` value cannot observe a `cons_tail` value that is older than what we
                // have just observed.
                Ordering::Release,
                // On failure, we need to ensure that the subsequent `cons_tail` value that will be
                // loaded in the next attempt is at least as recent as what was observed by the
                // thread that wrote the `prod_head` value we get back from `compare_exchange`.
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    break;
                }
                Err(new_prod_head) => {
                    prod_head = new_prod_head;
                }
            }

            backoff.spin();
            #[cfg(loom)]
            loom::hint::spin_loop();
        }

        let write_fn = unsafe { ManuallyDrop::take(write_fn) };
        if prod_next > prod_head || prod_next == 0 {
            write_fn(Write {
                front: unsafe {
                    core::slice::from_raw_parts_mut(
                        self.shared.data.add(prod_head as usize),
                        burst_len as usize,
                    )
                },
                back: None,
            });
        } else {
            let head_len = self.shared.capacity - prod_head;
            write_fn(Write {
                front: unsafe {
                    core::slice::from_raw_parts_mut(
                        self.shared.data.add(prod_head as usize),
                        head_len as usize,
                    )
                },
                back: Some(unsafe {
                    core::slice::from_raw_parts_mut(
                        self.shared.data,
                        (burst_len - head_len) as usize,
                    )
                }),
            });
        }

        // Wait for previous enqueues to finish.
        let backoff = Backoff::new();
        while self.shared.prod.tail.load(Ordering::Acquire) != prod_head {
            backoff.snooze();
            #[cfg(loom)]
            loom::hint::spin_loop();
        }
        // Mark this enqueue as finished. Synchronizes both with Acquire loads:
        //
        // - In consumers at the beginning of dequeue operations, to ensure that the write
        //   to the queue's buffer that was performed by this thread is visible to consumer threads
        //   before they are allowed to read the written region.
        //
        // - In other producers in the spin-loop above, to ensure that writes from other producers
        //   are observed by consumers who observe our store of `prod_next`.
        self.shared.prod.tail.store(prod_next, Ordering::Release);

        Ok(burst_len)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.sender_count.fetch_add(1, Ordering::Relaxed);

        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let sender_count = self.shared.sender_count.fetch_sub(1, Ordering::Relaxed);
        if sender_count == 1 {
            // Dropping the last sender - notify receivers that we are shutting down.
            // Use Release ordering to ensure that receivers see any items that have already been
            // written before observing the shutdown flag.
            self.shared.cons.head.fetch_or(SHUTDOWN_FLAG, Ordering::Release);
        }
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.shared.receiver_count.fetch_add(1, Ordering::Relaxed);

        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let receiver_count = self.shared.receiver_count.fetch_sub(1, Ordering::Relaxed);
        if receiver_count == 1 {
            // Dropping the last receiver - notify senders that we are shutting down.
            self.shared.prod.head.fetch_or(SHUTDOWN_FLAG, Ordering::Relaxed);
        }
    }
}

impl<T> Receiver<T> {
    /// Asynchronously receive up to `max_burst_len` values. If the queue is completely empty, the
    /// returned Future will yield to the async runtime until at least one value can be received.
    ///
    /// The provided `read_fn` closure will be called with a `Read` reservation handle that can be
    /// used to read the received values. If `T` is `!Copy` (it has a destructor), failing to take
    /// all items from the `Read` will result in leaked values. Currently the only way to take items
    /// from a `Read` instance is via `Read::into_iter`.
    pub async fn recv(
        &self,
        burst_len: usize,
        mut read_fn: impl FnMut(Read<T>),
    ) -> Result<usize, Error> {
        let result = self.shared.write_event
            .wait_until(|| {
                match self.try_recv_quiet(burst_len, &mut read_fn) {
                    Ok(0) => None,
                    Ok(n) => Some(Ok(n)),
                    Err(e) => Some(Err(e)),
                }
            })
            .await;
        if result.is_ok() {
            self.shared.consume_event.notify_all();
        }
        result
    }

    /// Attempt to receive up to `max_burst_len` values. If the queue is completely empty, this
    /// method will return `Ok(0)` immediately.
    ///
    /// See `Receiver::recv` for details about the `read_fn` parameter.
    #[inline]
    pub fn try_recv(
        &self,
        max_burst_len: usize,
        read_fn: impl FnOnce(Read<T>),
    ) -> Result<usize, Error> {
        let n = self.try_recv_quiet(max_burst_len, read_fn)?;
        if n > 0 {
            self.shared.consume_event.notify_all();
        }
        Ok(n)
    }

    /// Attempt to receive up to `max_burst_len` values. If the queue is completely empty, this
    /// method will return `Ok(0)` immediately.
    ///
    /// This method doesn't notify async Senders that new values can be sent. It should be used
    /// if and only if Senders only ever send with `try_send` or `try_send_quiet`.
    ///
    /// See `Receiver::recv` for details about the `read_fn` parameter.
    pub fn try_recv_quiet(
        &self,
        max_burst_len: usize,
        read_fn: impl FnOnce(Read<T>),
    ) -> Result<usize, Error> {
        // See the ordering used on failure in the compare exchange operation below - this uses
        // Acquire ordering for the same reason.
        let mut cons_head: usize = self.shared.cons.head.load(Ordering::Acquire);
        let mut cons_next: usize;
        let mut burst_len: usize;

        let backoff = Backoff::new();
        loop {
            let all_senders_dropped = (cons_head & SHUTDOWN_FLAG) != 0;
            cons_head &= !SHUTDOWN_FLAG;

            let seq = cons_head & SEQ_MASK;
            cons_head &= !SEQ_MASK;

            // Acquire ordering to ensure all producers' writes up to the tail are visible by this
            // thread.
            let prod_tail = self.shared.prod.tail.load(Ordering::Acquire);

            let entries =
                if prod_tail >= cons_head {
                    prod_tail - cons_head
                } else {
                    self.shared.capacity - cons_head + prod_tail
                };
            if entries == 0 {
                if all_senders_dropped {
                    return Err(Error::Shutdown);
                }

                // Compare exchange to be sure that we have the latest values and there really are
                // no free entries.
                //
                // This improves performance (measured +25% spsc throughput) in non-poll-mode
                // settings where this code path leads to registering this task in a wait queue,
                // which is expensive.
                if let Err(new_cons_head) = self.shared.cons.head.compare_exchange(
                    cons_head | seq as usize,
                    cons_head | seq as usize,
                    Ordering::Release,
                    Ordering::Acquire,
                ) {
                    cons_head = new_cons_head;
                    continue;
                }
                return Ok(0);
            }

            burst_len = core::cmp::min(max_burst_len, entries);
            cons_next = (cons_head + burst_len) % self.shared.capacity;

            let maybe_shutdown_flag = if all_senders_dropped { SHUTDOWN_FLAG } else { 0 };
            match self.shared.cons.head.compare_exchange_weak(
                cons_head | maybe_shutdown_flag | seq as usize,
                cons_next | maybe_shutdown_flag | ((seq + (burst_len << SEQ_SHIFT)) & SEQ_MASK),
                // On success, we need to ensure that subsequent consumers that observe the new
                // `cons_head` value cannot observe a `prod_tail` value that is older than what we
                // have just observed.
                Ordering::Release,
                // On failure, we need to ensure that the subsequent `prod_tail` value that will be
                // loaded in the next attempt is at least as recent as what was observed by the
                // thread that wrote the `cons_head` value we get back from `compare_exchange`.
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    break;
                }
                Err(new_cons_head) => {
                    cons_head = new_cons_head;
                }
            }

            backoff.spin();
            #[cfg(loom)]
            loom::hint::spin_loop();
        }

        if cons_next > cons_head || cons_next == 0 {
            read_fn(Read {
                front: unsafe {
                    core::slice::from_raw_parts_mut(
                        self.shared.data.add(cons_head as usize),
                        burst_len as usize,
                    )
                },
                back: None,
            });
        } else {
            let head_len = self.shared.capacity - cons_head;
            read_fn(Read {
                front: unsafe {
                    core::slice::from_raw_parts_mut(
                        self.shared.data.add(cons_head as usize),
                        head_len as usize,
                    )
                },
                back: Some(unsafe {
                    core::slice::from_raw_parts_mut(
                        self.shared.data,
                        (burst_len - head_len) as usize,
                    )
                }),
            });
        }

        // Wait for earlier consumes to finish.
        let backoff = Backoff::new();
        while self.shared.cons.tail.load(Ordering::Relaxed) != cons_head {
            backoff.snooze();
            #[cfg(loom)]
            loom::hint::spin_loop();
        }
        // Mark this consume as finished.
        self.shared.cons.tail.store(cons_next, Ordering::Relaxed);

        Ok(burst_len)
    }
}

impl<'a, T> Write<'a, T> {
    /// Get the length of this write reservation.
    #[inline]
    pub fn len(&self) -> usize {
        self.front.len() + self.back.as_ref().map(|t| t.len()).unwrap_or(0)
    }

    /// Write the provided value at the provided position relative to the start of this write
    /// reservation. When using this method, no other `Write::write*` methods can be used
    ///
    /// SAFETY: When using this function, the caller must be sure write every position exactly
    /// once, and no other `write*` methods can be used on the object. If `write_at` is called at
    /// the same position twice, drop will not be called on the first value. If some position never
    /// has `write_at` called for it, it will remain uninitialized and trigger undefined behavior
    /// when the receiver tries to read it.
    #[inline]
    pub unsafe fn write_at(&mut self, index: usize, value: T) {
        if index < self.front.len() {
            self.front[index].write(value);
        } else if let Some(back) = &mut self.back {
            back[index - self.front.len()].write(value);
        }
    }
}

impl<'a, T: Copy> Write<'a, T> {
    /// Fill this write reservation by copying values from the provided slice. The slice's length
    /// must be equal to the reservation's length.
    pub fn write_slice(mut self, items: &[T]) {
        use crate::util::maybe_uninit_write_slice;

        // The provided slice must be exactly the write size.
        assert_eq!(self.len(), items.len());

        let write_front_len = core::cmp::min(self.front.len(), items.len());
        maybe_uninit_write_slice(
            &mut self.front[..write_front_len],
            &items[..write_front_len],
        );

        if let Some(back) = &mut self.back {
            let write_back_len = core::cmp::min(back.len(), items.len() - write_front_len);
            maybe_uninit_write_slice(
                &mut back[..write_back_len],
                &items[write_front_len..write_front_len + write_back_len],
            );
        }
    }
}

impl<'a, T> Read<'a, T> {
    /// Get the length of this read reservation.
    #[inline]
    pub fn len(&self) -> usize {
        self.front.len() + self.back.as_ref().map(|t| t.len()).unwrap_or(0)
    }

    #[inline]
    pub fn get(&self, index: usize) -> &T {
        if index < self.front.len() {
            unsafe { self.front[index].assume_init_ref() }
        } else if let Some(back) = &self.back {
            unsafe { back[index - self.front.len()].assume_init_ref() }
        } else {
            panic!("Read::get index out of bounds");
        }
    }

    /// # Safety
    /// The provided pointer must point to an array of T with length >= `self.len()`.
    pub unsafe fn read_to_ptr(self, dst: *mut T) {
        unsafe {
            let front_src = self.front.as_mut_ptr() as *mut T;
            core::ptr::copy_nonoverlapping(front_src, dst, self.front.len());
            if let Some(back) = self.back {
                let back_src = back.as_mut_ptr() as *mut T;
                core::ptr::copy_nonoverlapping(
                    back_src,
                    dst.offset(self.front.len() as isize),
                    back.len(),
                );
            }
        }
    }
}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        unsafe {
            let data_slice: *mut [MaybeUninit<T>] =
                core::slice::from_raw_parts_mut(self.data, self.capacity as usize) as *mut _;
            drop(Box::from_raw(data_slice));
        }
    }
}

unsafe impl<T: Send> Send for Shared<T> { }
unsafe impl<T: Send> Sync for Shared<T> { }

unsafe impl<T: Send> Send for Sender<T> { }
unsafe impl<T: Send> Send for Receiver<T> { }

impl<'a, T> IntoIterator for Read<'a, T> {
    type Item = T;
    type IntoIter = ReadIter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        ReadIter {
            read: self,
            index: Cell::new(0),
        }
    }
}

pub struct ReadIter<'a, T> {
    read: Read<'a, T>,
    index: Cell<usize>,
}

impl<'a, T> Iterator for ReadIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let index = self.index.replace(self.index.get() + 1);
        if index < self.read.front.len() {
            Some(unsafe {
                core::mem::replace(
                    &mut self.read.front[index],
                    MaybeUninit::uninit(),
                )
                .assume_init()
            })
        } else if let Some(back) = &mut self.read.back {
            let back_index = index - self.read.front.len();
            if back_index < back.len() {
                Some(unsafe {
                    core::mem::replace(
                        &mut back[back_index],
                        MaybeUninit::uninit(),
                    )
                    .assume_init()
                })
            } else {
                None
            }
        } else {
            None
        }
    }
}

#[cfg(all(test, not(loom)))]
mod test {
    use super::*;

    #[cfg(not(feature = "std"))]
    use alloc::vec::Vec;

    #[cfg(feature = "std")]
    #[test]
    fn test_simple_example() {
        let (tx, rx) = mpmc::<u32>(5);

        let thread = std::thread::spawn(move || {
            pollster::block_on(async move {
                let mut next = 0;
                let payload: Vec<_> = (0..10).collect();

                while next < 10 {
                    let n = tx.send(10 - next, |w| {
                        let len = w.len();
                        w.write_slice(&payload[next..next + len]);
                    })
                    .await
                    .unwrap();

                    next += n;
                }
            });
        });

        let received = pollster::block_on(async move {
            let mut received = Vec::new();

            while received.len() < 10 {
                rx.recv(10 - received.len(), |r| {
                    received.extend(r);
                })
                .await
                .unwrap();
            }

            received
        });

        thread.join().unwrap();

        assert_eq!((0..10).sum::<u32>(), received.iter().sum());
    }

    #[test]
    fn test_s20_r10_s20_r25() {
        let (tx, rx) = mpmc(24);

        let write_payload: Vec<u32> = (0..20).collect();

        let n = tx.try_send(20, |w| { w.write_slice(&write_payload); }).unwrap();
        assert_eq!(n, 20);

        rx.try_recv(10, |r| {
            assert_eq!(r.into_iter().collect::<Vec<_>>(), &[
                0, 1, 2, 3, 4, 5, 6, 7, 8, 9,
            ]);
        })
        .unwrap();

        let n = tx.try_send(20, |w| { w.write_slice(&write_payload[..14]); }).unwrap();
        assert_eq!(n, 14);

        let n = rx.try_recv(25, |r| {
            assert_eq!(r.into_iter().collect::<Vec<_>>(), &[
                10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
                0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13,
            ]);
        })
        .unwrap();
        assert_eq!(n, 24);
    }

    #[test]
    fn test_s20_s20_r20_s5_r20() {
        let (tx, rx) = mpmc(24);

        let write_payload: Vec<u32> = (0..20).collect();

        let n = tx.try_send(20, |w| { w.write_slice(&write_payload); }).unwrap();
        assert_eq!(n, 20);
        let n = tx.try_send(20, |w| { w.write_slice(&write_payload[..4]); }).unwrap();
        assert_eq!(n, 4);

        let n = rx.try_recv(20, |r| {
            assert_eq!(r.into_iter().collect::<Vec<_>>(), (0..20).collect::<Vec<_>>());
        })
        .unwrap();
        assert_eq!(n, 20);

        let n = tx.try_send(5, |w| { w.write_slice(&write_payload[..5]); }).unwrap();
        assert_eq!(n, 5);

        let n = rx.try_recv(20, |r| {
            assert_eq!(r.into_iter().collect::<Vec<_>>(), &[0, 1, 2, 3, 0, 1, 2, 3, 4]);
        })
        .unwrap();
        assert_eq!(n, 9);
    }

    #[test]
    fn test_sender_shutdown() {
        let (tx, rx) = mpmc::<usize>(24);

        drop(rx);

        let r = tx.try_send(1, |_| { });
        assert_eq!(r, Err(Error::Shutdown));
    }

    #[test]
    fn test_sender_clone_then_shutdown() {
        let (tx, rx) = mpmc::<usize>(24);
        let rx2 = rx.clone();

        drop(rx);

        let r = tx.try_send(1, |w| w.write_slice(&[123]));
        assert_eq!(r, Ok(1));

        let mut did_receive = false;
        let r = rx2.try_recv(1, |r| {
            did_receive = true;
            assert_eq!(*r.get(0), 123);
        });
        assert!(did_receive);
        assert_eq!(r, Ok(1));

        drop(rx2);

        let r = tx.try_send(1, |_| { });
        assert_eq!(r, Err(Error::Shutdown));
    }

    #[test]
    fn test_receiver_shutdown() {
        let (tx, rx) = mpmc::<usize>(24);

        drop(tx);

        let r = rx.try_recv(1, |_| { });
        assert_eq!(r, Err(Error::Shutdown));
    }

    #[test]
    fn test_receive_clone_then_shutdown() {
        let (tx, rx) = mpmc::<usize>(24);
        let tx2 = tx.clone();

        drop(tx);

        let r = tx2.try_send(1, |w| w.write_slice(&[123]));
        assert_eq!(r, Ok(1));

        let mut did_receive = false;
        let r = rx.try_recv(1, |r| {
            did_receive = true;
            assert_eq!(*r.get(0), 123);
        });
        assert!(did_receive);
        assert_eq!(r, Ok(1));

        drop(tx2);

        let r = rx.try_recv(1, |_| { });
        assert_eq!(r, Err(Error::Shutdown));
    }
}

#[cfg(all(test, loom))]
mod test {
    use super::*;

    #[cfg(loom)]
    #[test]
    fn test_burst_spsc_e2e_loom() {
        let tx_threads_count = 1;
        let rx_threads_count = 1;
        let tx_batches = 4;
        let default_preemption_bound = Some(3);
        e2e_loom(
            tx_threads_count,
            rx_threads_count,
            tx_batches,
            default_preemption_bound,
        );
    }

    // Takes too long currently, but if you break the memory orderings you'll see this fail in a
    // reasonable amount of time (few minutes to a few hours).
    /*#[cfg(loom)]
    #[test]
    fn test_burst_mpmc_e2e_loom() {
        let tx_threads_count = 2;
        let rx_threads_count = 2;
        let tx_batches = 3;
        let default_preemption_bound = Some(3);
        e2e_loom(
            tx_threads_count,
            rx_threads_count,
            tx_batches,
            default_preemption_bound,
        );
    }*/

    #[cfg(loom)]
    fn e2e_loom(
        tx_threads_count: usize,
        rx_threads_count: usize,
        tx_batches: usize,
        default_preemption_bound: Option<usize>,
    ) {
        use core::cell::RefCell;

        #[cfg(feature = "std")]
        use std::rc::Rc;
        #[cfg(not(feature = "std"))]
        use alloc::rc::Rc;

        let tx_batch_size = 32;
        let rx_batch_size = tx_batch_size * 3 / 2;
        let total_item_count = tx_batch_size * tx_batches * tx_threads_count;

        assert_eq!(total_item_count % 2, 0);
        assert_eq!(total_item_count % rx_threads_count, 0);

        let mut builder = loom::model::Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = default_preemption_bound;
        }

        builder.check(move || {
            let (tx, rx) = mpmc(total_item_count / 2 + 1);

            let sent_items = Rc::new(RefCell::new(Vec::<usize>::new()));
            let recv_items = Rc::new(RefCell::new(Vec::<usize>::new()));

            let build_tx_work_fn = |thread_id| {
                let tx = tx.clone();
                let sent_items = sent_items.clone();
                move || {
                    let mut start = (total_item_count / tx_threads_count) * thread_id;

                    let mut send_count = 0;
                    loop {
                        let remaining = total_item_count / tx_threads_count - send_count;
                        let want_count = core::cmp::min(remaining, tx_batch_size);
                        if want_count == 0 {
                            break;
                        }

                        let write_payload: Vec<_> = (start .. start + tx_batch_size).collect();
                        let n = match tx.try_send_quiet(want_count, |w| {
                            let len = w.len();
                            w.write_slice(&write_payload[..len]);
                            sent_items.borrow_mut().extend(&write_payload[..len]);
                        }) {
                            Ok(n) => n,
                            Err(Error::Shutdown) => panic!("unexpected shutdown"),
                        };
                        assert!(n <= want_count);
                        start += n;
                        send_count += n;

                        if n == 0 {
                            loom::hint::spin_loop();
                        }
                    }
                }
            };

            let tx_threads: Vec<_> = (0..tx_threads_count - 1).map(|thread_id| {
                loom::thread::spawn(build_tx_work_fn(thread_id))
            })
            .collect();

            let rx_threads: Vec<_> = (0..rx_threads_count).map(|_| {
                let rx = rx.clone();
                let recv_items = recv_items.clone();
                loom::thread::spawn(move || {
                    let mut recv_count = 0;
                    loop {
                        let remaining = total_item_count / rx_threads_count - recv_count;
                        let want_count = core::cmp::min(remaining, rx_batch_size);
                        if want_count == 0 {
                            break;
                        }

                        let n = match rx.try_recv_quiet(want_count, |r| {
                            assert!(r.len() <= want_count);
                            recv_items.borrow_mut().extend(r.into_iter());
                        }) {
                            Ok(n) => n,
                            Err(Error::Shutdown) => panic!("unexpected shutdown"),
                        };
                        assert!(n <= want_count);
                        recv_count += n;

                        if n == 0 {
                            loom::hint::spin_loop();
                        }
                    }
                })
            })
            .collect();

            build_tx_work_fn(tx_threads_count - 1)();

            for tx_thread in tx_threads {
                tx_thread.join().unwrap();
            }
            for rx_thread in rx_threads {
                rx_thread.join().unwrap();
            }

            assert_eq!(sent_items.borrow().len(), total_item_count);
            assert_eq!(recv_items.borrow().len(), total_item_count);
            // Because we modify sent_items and recv_items in the write_fn / read_fn, under loom
            // there is no preemption between writing / reading the queue and writing to
            // sent_items / recv_items, so they are guaranteed to be in the same order.
            assert_eq!(sent_items, recv_items);
        });
    }
}
