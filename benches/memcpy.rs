use std::os::unix::io::AsRawFd;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use libc::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE, c_void, iovec};
use mem_bench::{GB, KB, MB, PageSize, anon_mmap, create_file, create_memfd, fill_fd};

// ─── Sizes ────────────────────────────────────────────────────────────────────

const SIZES: &[usize] = &[
    4 * KB,
    64 * KB,
    MB,
    16 * MB,
    256 * MB,
    GB,
    4 * GB,
    16 * GB,
    30 * GB,
];

/// Parse a human-readable size string: raw bytes or a value with a KB / MB / GB suffix.
fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("GB") {
        n.trim().parse::<usize>().ok().map(|n| n * GB)
    } else if let Some(n) = s.strip_suffix("MB") {
        n.trim().parse::<usize>().ok().map(|n| n * MB)
    } else if let Some(n) = s.strip_suffix("KB") {
        n.trim().parse::<usize>().ok().map(|n| n * KB)
    } else {
        s.parse::<usize>().ok()
    }
}

/// Returns the sizes to benchmark.
/// Set BENCH_SIZE=<n>[KB|MB|GB] to restrict to one or more sizes, comma-separated
/// (e.g. BENCH_SIZE=1GB or BENCH_SIZE=256MB,1GB,4GB).
/// Defaults to all entries in SIZES.
fn bench_sizes() -> Vec<usize> {
    match std::env::var("BENCH_SIZE") {
        Ok(val) => val
            .split(',')
            .map(|s| {
                parse_size(s).unwrap_or_else(|| {
                    panic!("invalid BENCH_SIZE entry {s:?}, expected <n>[KB|MB|GB] or raw bytes")
                })
            })
            .collect(),
        Err(_) => SIZES.to_vec(),
    }
}

// ─── Child process for process_vm_readv ──────────────────────────────────────

/// A forked child that holds an mmap region for the parent to read via
/// process_vm_readv. Killed and reaped on drop.
struct ChildProcess {
    pid: libc::pid_t,
    /// Address of the mmap region *in the child's address space*.
    remote_ptr: *mut c_void,
}

// remote_ptr is only ever passed to process_vm_readv (a syscall), never
// dereferenced in the parent, so Send is safe here.
unsafe impl Send for ChildProcess {}

impl ChildProcess {
    fn spawn(size: usize, page_size: PageSize) -> Self {
        // Compute mmap flags before fork so the child only needs syscalls.
        let mmap_flags = match page_size {
            PageSize::Standard => MAP_ANONYMOUS | MAP_PRIVATE,
            PageSize::Huge => MAP_ANONYMOUS | MAP_PRIVATE | libc::MAP_HUGETLB,
        };

        // Pipe used by the child to send its mmap address back to the parent.
        let mut pipe_fds = [0i32; 2];
        let ret = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "pipe failed: {}", std::io::Error::last_os_error());

        let pid = unsafe { libc::fork() };
        match pid {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),

            0 => {
                // ── Child ──────────────────────────────────────────────────
                // After fork in a multi-threaded process only async-signal-safe
                // functions are safe to call. We use only raw libc syscalls and
                // libc::_exit (which skips atexit handlers).
                unsafe {
                    libc::close(pipe_fds[0]);

                    let ptr = libc::mmap(
                        std::ptr::null_mut(),
                        size,
                        PROT_READ | PROT_WRITE,
                        mmap_flags,
                        -1,
                        0,
                    );
                    if ptr == libc::MAP_FAILED {
                        libc::_exit(1);
                    }

                    // Fill with non-zero data so the parent reads real content.
                    // memset is async-signal-safe and safe to call post-fork.
                    libc::memset(ptr, 0xAB, size);

                    let ptr_val = ptr as usize;
                    libc::write(
                        pipe_fds[1],
                        &ptr_val as *const usize as *const c_void,
                        std::mem::size_of::<usize>(),
                    );
                    libc::close(pipe_fds[1]);

                    loop {
                        libc::pause();
                    }
                }
            }

            child_pid => {
                // ── Parent ─────────────────────────────────────────────────
                assert!(child_pid > 0);
                unsafe { libc::close(pipe_fds[1]) };

                let mut ptr_val: usize = 0;
                let n = unsafe {
                    libc::read(
                        pipe_fds[0],
                        &mut ptr_val as *mut usize as *mut c_void,
                        std::mem::size_of::<usize>(),
                    )
                };
                assert_eq!(
                    n as usize,
                    std::mem::size_of::<usize>(),
                    "reading child mmap address failed: {}",
                    std::io::Error::last_os_error(),
                );
                unsafe { libc::close(pipe_fds[0]) };

                Self {
                    pid: child_pid,
                    remote_ptr: ptr_val as *mut c_void,
                }
            }
        }
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            libc::waitpid(self.pid, std::ptr::null_mut(), 0);
        }
    }
}

// ─── Benchmarks ───────────────────────────────────────────────────────────────

/// All five copy mechanisms in one Criterion group, so Criterion generates
/// per-size comparison plots across methods.
///
/// Group name is the page-size label ("4k" or "2m").
/// Function dimension (first BenchmarkId argument) is the method name.
/// Parameter dimension (second BenchmarkId argument) is the copy size.
///
/// fd_to_fd has no huge-page variant (file page-cache always uses 4 KiB pages)
/// and is therefore only registered in the "4k" group.
fn bench_copy(c: &mut Criterion, page_size: PageSize) {
    let mut group = c.benchmark_group(page_size.label());

    for size in bench_sizes() {
        if !page_size.is_size_compatible(size) {
            continue;
        }

        group.throughput(Throughput::Bytes(size as u64));

        // ── process_vm_readv ──────────────────────────────────────────────────
        group.bench_with_input(
            BenchmarkId::new("process_vm_readv", size),
            &(size, page_size),
            |b, &(size, page_size)| {
                let child = ChildProcess::spawn(size, page_size);
                let dst = unsafe { anon_mmap(size, page_size) };

                b.iter(|| {
                    let mut done = 0usize;
                    while done < size {
                        let local = iovec {
                            iov_base: (dst as *mut u8).wrapping_add(done) as *mut c_void,
                            iov_len: size - done,
                        };
                        let remote = iovec {
                            iov_base: (child.remote_ptr as *mut u8).wrapping_add(done)
                                as *mut c_void,
                            iov_len: size - done,
                        };
                        let n =
                            unsafe { libc::process_vm_readv(child.pid, &local, 1, &remote, 1, 0) };
                        assert!(
                            n > 0,
                            "process_vm_readv failed after {done} bytes: {}",
                            std::io::Error::last_os_error(),
                        );
                        done += n as usize;
                    }
                });

                unsafe { libc::munmap(dst, size) };
                drop(child);
            },
        );

        // ── mmap → file (pwrite) ──────────────────────────────────────────────
        group.bench_with_input(
            BenchmarkId::new("mmap_to_file", size),
            &(size, page_size),
            |b, &(size, page_size)| {
                let src = unsafe { anon_mmap(size, page_size) };
                unsafe { std::ptr::write_bytes(src as *mut u8, 0xAB, size) };
                let dst = create_file(size);
                let dst_fd = dst.as_raw_fd();

                b.iter(|| {
                    let mut written = 0usize;
                    while written < size {
                        let n = unsafe {
                            libc::pwrite(
                                dst_fd,
                                (src as *const u8).add(written) as *const c_void,
                                size - written,
                                written as libc::off_t,
                            )
                        };
                        assert!(n > 0, "pwrite failed: {}", std::io::Error::last_os_error());
                        written += n as usize;
                    }
                });

                drop(dst);
                unsafe { libc::munmap(src, size) };
            },
        );

        // ── mmap → mmap (memcpy) ──────────────────────────────────────────────
        group.bench_with_input(
            BenchmarkId::new("mmap_to_mmap", size),
            &(size, page_size),
            |b, &(size, page_size)| {
                let src = unsafe { anon_mmap(size, page_size) };
                unsafe { std::ptr::write_bytes(src as *mut u8, 0xAB, size) };
                let dst = unsafe { anon_mmap(size, page_size) };

                b.iter(|| unsafe {
                    let mut count = 0;
                    while count < size {
                        let len = (size - count).min(page_size.into());
                        std::ptr::copy_nonoverlapping(
                            (src as *const u8).add(count),
                            (dst as *mut u8).add(count),
                            len,
                        );
                        count += len
                    }
                });

                unsafe {
                    libc::munmap(src, size);
                    libc::munmap(dst, size);
                }
            },
        );

        // ── fd → fd (sendfile) — standard pages only ──────────────────────────
        if page_size == PageSize::Standard {
            group.bench_with_input(BenchmarkId::new("fd_to_fd", size), &size, |b, &size| {
                let src = create_file(size);
                fill_fd(src.as_raw_fd(), size);
                let dst = create_file(size);
                let src_fd = src.as_raw_fd();
                let dst_fd = dst.as_raw_fd();

                b.iter(|| {
                    let mut offset: libc::off_t = 0;
                    let mut remaining = size;
                    while remaining > 0 {
                        let n = unsafe { libc::sendfile(dst_fd, src_fd, &mut offset, remaining) };
                        assert!(
                            n > 0,
                            "sendfile failed: {}",
                            std::io::Error::last_os_error(),
                        );
                        remaining -= n as usize;
                    }
                });

                drop(src);
                drop(dst);
            });
        }

        // ── memfd → mmap (pread) ──────────────────────────────────────────────
        group.bench_with_input(
            BenchmarkId::new("memfd_to_mmap", size),
            &(size, page_size),
            |b, &(size, page_size)| {
                let src_fd = create_memfd(size, page_size);
                fill_fd(src_fd, size);
                let dst = unsafe { anon_mmap(size, page_size) };

                b.iter(|| {
                    let mut done = 0usize;
                    while done < size {
                        let n = unsafe {
                            libc::pread(
                                src_fd,
                                (dst as *mut u8).add(done) as *mut c_void,
                                size - done,
                                done as libc::off_t,
                            )
                        };
                        assert!(n > 0, "pread failed: {}", std::io::Error::last_os_error());
                        done += n as usize;
                    }
                });

                unsafe {
                    libc::close(src_fd);
                    libc::munmap(dst, size);
                }
            },
        );
    }

    group.finish();
}

fn bench_4k(c: &mut Criterion) {
    bench_copy(c, PageSize::Standard);
}

fn bench_2m(c: &mut Criterion) {
    bench_copy(c, PageSize::Huge);
}

// ─── Criterion wiring ─────────────────────────────────────────────────────────

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .measurement_time(Duration::from_secs(30))
        .warm_up_time(Duration::from_secs(3));
    targets = bench_4k, bench_2m
}
criterion_main!(benches);
