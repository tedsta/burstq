use core_affinity::CoreId;
use criterion::{criterion_group, criterion_main, Criterion};
use std::future::Future;
use std::sync::{Arc, Barrier};

// x10000 everywhere to amortize the startup cost of each benchmark (e.g. creating executor).

fn block_on<F: Future>(f: F) -> F::Output {
    // Helper function to make it easy to try out different executors.

    futures_executor::block_on(f)
}

fn burst_mpmc_x10000(c: &mut Criterion) {
    let tx_batch_size = 32;
    let rx_batch_size = 32;
    let tx_threads = 4;

    assert_eq!(10000 % tx_threads, 0);

    let (tx, rx) = burstq::mpmc::<usize>(4000);
    let barrier = Arc::new(Barrier::new(tx_threads + 1));

    for thread_id in 0..tx_threads {
        let tx = tx.clone();
        let barrier = barrier.clone();

        std::thread::spawn(move || {
            core_affinity::set_for_current(CoreId { id: thread_id + 1 });

            let write_payload = &vec![42usize; tx_batch_size];

            block_on(async {
                loop {
                    barrier.wait();

                    let mut progress = 0;
                    while progress < 10000 / tx_threads {
                        let want_burst_size = std::cmp::min(10000 / tx_threads - progress, tx_batch_size);
                        progress += tx.send(want_burst_size, |w| {
                            let len = w.len();
                            w.write_slice(&write_payload[..len]);
                        })
                        .await
                        .unwrap();
                    }
                }
            });
        });
    }

    core_affinity::set_for_current(CoreId { id: 0 });

    let expected_payload = &vec![42usize; tx_batch_size];

    c.bench_function("burst mpmc x10000", |b| {
        b.iter(|| {
            block_on(async {
                barrier.wait();

                let mut progress = 0;
                while progress < 10000 {
                    progress += rx.recv(rx_batch_size, |r| {
                        let len = r.len();
                        assert!(r.len() <= rx_batch_size);
                        assert!(r.into_iter().eq((&expected_payload[..len]).iter().cloned()));
                    })
                    .await
                    .unwrap();
                }
            })
        })
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = burst_mpmc_x10000
}
criterion_main!(benches);
