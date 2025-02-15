# VecBelt

`VecBelt<T>` is a high-performant, concurrent, lock-free data structure suitable for multi-threaded bulk-appending (takes a `&self`) and single-threaded consuming (takes a `&mut self`).

Under the hood, `VecBelt<T>` is implemented as a linked list of dynamically sized fragments that may hold several elements at once, queued by `append(&self, ..)`. New fragments of the current length (hence an exponential size to reduce the amount of allocations necessary for many appends) are allocated as necessary. `clear(&mut self, ..)`-ing the `VecBelt<T>` merges all of these fragments into one large fragment able to contain all items contiguously, which is then passed to a consumer closure. This implies subsequent appends will be faster, as there will be substantially less fragment allocations due to the large size of the merged (and reused) fragment.

## `VecBelt<T>` vs `Mutex<Vec<T>>`

- `VecBelt<T>` operates much faster than `Mutex<Vec<T>>`, except on very low contentions (in my case it was 4 threads times 4 appends per threads). See [benchmarks](#benchmarks) below.
- `VecBelt<T>` is non-blocking, i.e. it doesn't invoke system calls to park or unpark threads in any way (worst case scenario, it yields to the CPU scheduler. Backed by `crossbeam-utils`'s exponential `Backoff`). This works because acquisitions to the sychronization primitive is extremely short-lived.
- `VecBelt<T>` is append-only when accessed immutably from threads, while `Mutex<Vec<T>>` grants every operations.
- `VecBelt<T>` may only append collections that coerce into a slice (`[T]` or `&[T] where T: Copy`), which notably don't include most iterator adapters. This is because `append(&self, ..)` requires all transfer operations to not diverge (e.g. `panic!(..)`), otherwise the fragment slice may end up with uninitialized memory which will lead to **undefined behavior** when used or even dropped.

## Benchmarks

This benchmark was executed on a 13th Gen Intel(R) Core(TM) i5-13420H (12 CPUs), ~2.1 GHz running Windows 11 Pro 64-bit. Visualized results are in `bench_result/report/index.html` on GitHub.

The benchmark tests the append time on 4, 16, and 64 threads, each running 4, 16, 64, and 256 append operations for both `VecBelt<usize>` and `Mutex<Vec<usize>>`. Both data containers are initially backed by a buffer of length 32768, while each appends transfer data of length 64.

Tests are timed exactly when all worker threads are busy-waiting for append operations, and ended exactly after all operations are done (note that the worker threads might live a bit longer). All worker threads are then shut down outside of timings before moving on to the next tests.

### 4 Threads

| Appends     | `VecBelt<T>` (avg.) | `Mutex<Vec<T>>` (avg.) |
|-------------|---------------------|------------------------|
| 4           | 43.305 µs           | 43.938 µs              |
| 16          | 44.387 µs           | 46.197 µs              |
| 64          | 52.276 µs           | 61.159 µs              |
| 128         | 109.80 µs           | 170.24 µs              |

### 16 Threads

| Appends     | `VecBelt<T>` (avg.) | `Mutex<Vec<T>>` (avg.) |
|-------------|---------------------|------------------------|
| 4           | 75.426 µs           | 90.784 µs              |
| 16          | 119.66 µs           | 138.42 µs              |
| 64          | 191.75 µs           | 265.53 µs              |
| 128         | 801.14 µs           | 1.4654 ms              |

### 64 Threads

| Appends     | `VecBelt<T>` (avg.) | `Mutex<Vec<T>>` (avg.) |
|-------------|---------------------|------------------------|
| 4           | 322.48 µs           | 369.00 µs              |
| 16          | 460.20 µs           | 546.95 µs              |
| 64          | 1.0637 ms           | 1.8535 ms              |
| 128         | 3.6124 ms           | 6.9506 ms              |

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
