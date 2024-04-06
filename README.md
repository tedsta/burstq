# Burstq

A multi-producer, multi-consumer channel that supports sending or receiving multiple items in a single operation.

Currently, only async and busy-waiting modes are supported. If you want to block the current thread, you'll need to use a minimal async executor such as [pollster](docs.rs/pollster).

```rust
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
```

The core lock-free enqueue / dequeue algorithm is based on [DPDK's `rte_ring`](https://doc.dpdk.org/guides/prog_guide/ring_lib.html). In particular, it implements the "burst" (as opposed to "bulk") behavior of `rte_ring` where if not all requested items can be enqueued/dequeued, as many as is currently possible will be.

The async-ness of burstq is achieved using the [async-event](https://docs.rs/async-event) crate.

# License

MIT
