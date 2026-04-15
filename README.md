# mem-bench

Linux memory benchmark suite measuring the throughput of various memory-copy
mechanisms and the latency of page faults across different memory backings.

All benchmarks are built on [Criterion](https://github.com/bheisler/criterion.rs)
and produce HTML reports with throughput plots and cross-run regression detection.

---

## Benchmark suites

### `memcpy` — copy throughput

Measures how fast data can be moved between memory regions using five different
kernel/userspace mechanisms. Each benchmark is parameterised over **copy size**
and, where applicable, **page size** (standard 4 KiB vs huge 2 MiB).

| Benchmark | Source | Destination | Mechanism |
|---|---|---|---|
| `process_vm_readv` | child process mmap | local mmap | `process_vm_readv(2)` |
| `mmap_to_file` | anonymous mmap | regular file | `pwrite(2)` |
| `mmap_to_mmap` | anonymous mmap | anonymous mmap | `memcpy` |
| `fd_to_fd` | regular file | regular file | `sendfile(2)` |
| `memfd_to_mmap` | memfd | anonymous mmap | `pread(2)` |

**Sizes tested** (default): 4 KiB, 64 KiB, 1 MiB, 16 MiB, 256 MiB, 1 GiB,
4 GiB, 16 GiB, 30 GiB.

**Huge page variants** are available for all benchmarks except `fd_to_fd`
(file page-cache always uses regular pages). Huge page benchmarks skip sizes
below 16 MiB (the first multiple of the 2 MiB huge-page size in the list).

**`process_vm_readv`** forks a child process that allocates the source region
and then sleeps; the parent reads from it across the process boundary. The child
is killed and reaped at the end of each benchmark case.

**`mmap_to_file` / `fd_to_fd`** create files on the filesystem pointed to by
`BENCH_DIR` (see [Configuration](#configuration)) so they reflect real
filesystem behaviour rather than tmpfs.

---

### `page_fault` — page-fault latency

Measures the time to fault in a fixed number of pages (`N_FAULT_PAGES = 128`)
after `MADV_DONTNEED` strips them from the process's page table. The
`MADV_DONTNEED` call is outside the timed region; only the subsequent page
accesses are measured.

`write_volatile` is used (rather than a read) so that:
- anonymous accesses allocate a real page instead of mapping the shared zero-page;
- file/memfd accesses trigger a copy-on-write fault from the page cache.

Parameterised over **backing** × **page size**:

| Backing | Standard (4 KiB) | Huge (2 MiB) |
|---|---|---|
| `anon` — anonymous private pages | ✓ | ✓ |
| `file` — regular on-disk file (`BENCH_DIR`) | ✓ | — |
| `memfd` — `memfd_create` / tmpfs | ✓ | ✓ |

`file + huge` is not supported: mapping a plain file with `MAP_HUGETLB`
requires the file to reside on a hugetlbfs mount.

**Throughput** is reported as page faults per second (criterion label:
`elements/s`). Invert to get average latency per fault.

Region sizes:
- Standard: 128 × 4 KiB = 512 KiB
- Huge: 128 × 2 MiB = 256 MiB (requires 256 MiB of pre-allocated huge pages)

---

## Running

```sh
# Run both suites
cargo bench

# Run a single suite
cargo bench --bench memcpy
cargo bench --bench page_fault

# Filter by benchmark group or case (criterion name filter)
cargo bench --bench memcpy -- mmap_to_mmap
cargo bench -- "page_fault/memfd/2m"
```

HTML reports are written to `target/criterion/report/index.html` and updated
automatically on every run, including cross-run regression comparisons.

```sh
xdg-open target/criterion/report/index.html
```

---

## Configuration

All options are set via environment variables.

### `BENCH_DIR`

Directory used for **real on-disk files** (`mmap_to_file`, `fd_to_fd`,
`page_fault/file`). Defaults to the current working directory.

Set this to a mount point to benchmark a specific filesystem:

```sh
BENCH_DIR=/mnt/nvme  cargo bench --bench memcpy -- mmap_to_file
BENCH_DIR=/mnt/hdd   cargo bench --bench memcpy -- fd_to_fd
```

Files are created as anonymous temporaries (unlinked immediately) and cleaned
up automatically.

### `BENCH_SIZE` *(memcpy suite only)*

Restrict the `memcpy` suite to one or more copy sizes instead of sweeping all
sizes. Accepts a comma-separated list of values; each value is either a raw byte
count or a number with a `KB`, `MB`, or `GB` suffix.

```sh
BENCH_SIZE=1GB             cargo bench --bench memcpy
BENCH_SIZE=256MB,1GB,4GB   cargo bench --bench memcpy
BENCH_SIZE=256MB           cargo bench --bench memcpy -- mmap_to_mmap
BENCH_SIZE=4096            cargo bench --bench memcpy   # raw bytes
```

### Huge pages

Huge-page benchmarks require 2 MiB huge pages to be pre-allocated in the
kernel's hugetlb pool. Check the current pool:

```sh
cat /proc/sys/vm/nr_hugepages          # number of pre-allocated pages
cat /proc/meminfo | grep HugePages
```

Allocate enough pages (example: 512 pages = 1 GiB):

```sh
echo 512 | sudo tee /proc/sys/vm/nr_hugepages
```

For the largest `page_fault/huge` case (256 MiB) at least **128** pages are
needed. For the largest `memcpy` huge-page case (30 GiB source + destination)
at least **30720** pages are needed per mapping.

Huge-page benchmarks will `assert!`-fail at startup if the pool is exhausted.
