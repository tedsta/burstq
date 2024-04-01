use core::{
    cell::Cell,
    mem::MaybeUninit,
    sync::atomic::Ordering,
};

use async_event::Event;
use crossbeam_utils::{Backoff, CachePadded};

use crate::loom_exports::sync::{
    atomic::AtomicUsize,
    Arc,
};

#[derive(Debug)]
pub enum Error {
    Shutdown,
}

pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

#[derive(Clone)]
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

struct Slinky {
    head: AtomicUsize,
    tail: AtomicUsize,
}

struct Shared<T> {
    prod: CachePadded<Slinky>,
    cons: CachePadded<Slinky>,

    capacity: usize,
    data: *mut MaybeUninit<T>,

    write_event: Event,
    consume_event: Event,
}

pub struct Write<'a, T> {
    front: &'a mut [MaybeUninit<T>],
    back: Option<&'a mut [MaybeUninit<T>]>,
}

pub struct Read<'a, T> {
    front: &'a mut [MaybeUninit<T>],
    back: Option<&'a mut [MaybeUninit<T>]>,
}

pub fn mpmc<T: 'static>(capacity: usize)
    -> (Sender<T>, Receiver<T>)
{
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
    });

    let producer = Sender { shared: shared.clone() };
    let consumer = Receiver { shared };

    (producer, consumer)
}

impl<T> Sender<T> {
    #[inline(always)]
    pub async fn send(
        &self,
        max_burst_len: usize,
        mut write_fn: impl FnMut(Write<T>),
    ) -> Result<usize, Error> {
        if max_burst_len == 0 {
            return Ok(0);
        }

        self.shared.consume_event
            .wait_until(|| {
                match self.try_send(max_burst_len, &mut write_fn) {
                    Ok(0) => None,
                    Ok(n) => Some(Ok(n)),
                    Err(e) => Some(Err(e)),
                }
            })
            .await
    }

    #[inline(always)]
    pub fn try_send(
        &self,
        max_burst_len: usize,
        write_fn: impl FnOnce(Write<T>),
    ) -> Result<usize, Error> {
        if max_burst_len == 0 {
            return Ok(0);
        }

        let mut prod_head = self.shared.prod.head.load(Ordering::Acquire);
        let mut prod_next: usize;
        let mut burst_len: usize;

        let backoff = Backoff::new();
        loop {
            let cons_tail = self.shared.cons.tail.load(Ordering::Relaxed);

            let free_entries =
                if cons_tail > prod_head {
                    cons_tail - prod_head - 1
                } else {
                    self.shared.capacity - 1 - prod_head + cons_tail
                };

            if free_entries == 0 {
                return Ok(0);
            }

            burst_len = core::cmp::min(max_burst_len, free_entries);
            prod_next = (prod_head + burst_len) % self.shared.capacity;

            match self.shared.prod.head.compare_exchange_weak(
                prod_head,
                prod_next,
                // On success, we need to ensure that subsequent producers that observe the new
                // `prod_head` value cannot observe a `cons_tail` value that is older than what we
                // have just observed.
                Ordering::Release,
                // On failure, we need to ensure that the subsequent cons_tail value that will be
                // loaded in the next attempt happens-before the `prod_head` value we get back from
                // `compare_exchange`.
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
        // - In other producers in the spin-loop above, to ensure writes from other producers
        //   happen before this thread allows consumers to progress through its written region.
        // - In consumers at the beginning of dequeue operations, to ensure that all of the writes
        //   to the ring that were done in this thread happen on consumer threads before they are
        //   allowed to read the written region.
        self.shared.prod.tail.store(prod_next, Ordering::Release);

        self.shared.write_event.notify_all();

        Ok(burst_len)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(
        &self,
        max_burst_len: usize,
        read_fn: impl FnOnce(Read<T>),
    ) -> Result<usize, Error> {
        let mut cons_head: usize = self.shared.cons.head.load(Ordering::Acquire);
        let mut cons_next: usize;
        let mut burst_len: usize;

        let backoff = Backoff::new();
        loop {
            let prod_tail = self.shared.prod.tail.load(Ordering::Acquire);

            let entries =
                if prod_tail >= cons_head {
                    prod_tail - cons_head
                } else {
                    self.shared.capacity - cons_head + prod_tail
                };
            if entries == 0 {
                return Ok(0);
            }

            burst_len = core::cmp::min(max_burst_len, entries);
            cons_next = (cons_head + burst_len) % self.shared.capacity;

            match self.shared.cons.head.compare_exchange_weak(
                cons_head,
                cons_next,
                Ordering::Release,
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

        self.shared.consume_event.notify_all();

        Ok(burst_len)
    }

    pub async fn recv(
        &self,
        burst_len: usize,
        mut read_fn: impl FnMut(Read<T>),
    ) -> Result<usize, Error> {
        self.shared.write_event
            .wait_until(|| {
                match self.try_recv(burst_len, &mut read_fn) {
                    Ok(0) => None,
                    Ok(n) => Some(Ok(n)),
                    Err(e) => Some(Err(e)),
                }
            })
            .await
    }
}

impl<'a, T> Write<'a, T> {
    /// Get the length of this write reservation.
    #[inline(always)]
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
    #[inline(always)]
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
    #[inline(always)]
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
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.front.len() + self.back.as_ref().map(|t| t.len()).unwrap_or(0)
    }

    #[inline(always)]
    pub fn get(&self, index: usize) -> &T {
        if index < self.front.len() {
            unsafe { self.front[index].assume_init_ref() }
        } else if let Some(back) = &self.back {
            unsafe { back[index - self.front.len()].assume_init_ref() }
        } else {
            panic!("Read::get index out of bounds");
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

unsafe impl<T> Send for Shared<T> { }
unsafe impl<T> Sync for Shared<T> { }

unsafe impl<T> Send for Sender<T> { }
unsafe impl<T> Send for Receiver<T> { }

impl<'a, T: 'static> IntoIterator for Read<'a, T> {
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

impl<'a, T: 'static> Iterator for ReadIter<'a, T> {
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

#[cfg(test)]
mod test {
    use super::*;

    #[cfg(not(loom))]
    #[test]
    fn test_s20_r10_s20_r25() {
        let (tx, rx) = mpmc(25);

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

    #[cfg(not(loom))]
    #[test]
    fn test_s20_s20_r20_s5_r20() {
        let (tx, rx) = mpmc(25);

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

    #[cfg(loom)]
    #[test]
    fn test_burst_spsc_e2e_loom() {
        let tx_threads_count = 1;
        let rx_threads_count = 1;
        let tx_batches = 4;
        e2e_loom(
            tx_threads_count,
            rx_threads_count,
            tx_batches,
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
        e2e_loom(
            tx_threads_count,
            rx_threads_count,
            tx_batches,
        );
    }*/

    #[cfg(loom)]
    fn e2e_loom(
        tx_threads_count: usize,
        rx_threads_count: usize,
        tx_batches: usize,
    ) {
        use std::rc::Rc;
        use core::cell::RefCell;

        let tx_batch_size = 32;
        let rx_batch_size = tx_batch_size * 3 / 2;
        let total_item_count = tx_batch_size * tx_batches * tx_threads_count;

        assert_eq!(total_item_count % 2, 0);
        assert_eq!(total_item_count % rx_threads_count, 0);

        loom::model(move || {
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
                        let n = match tx.try_send(want_count, |w| {
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

                        let n = match rx.try_recv(want_count, |r| {
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
