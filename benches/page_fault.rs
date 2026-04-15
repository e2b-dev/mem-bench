use std::os::unix::io::{AsRawFd, RawFd};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use libc::{MAP_PRIVATE, PROT_READ, PROT_WRITE, c_void};
use mem_bench::{HUGE_PAGE_SIZE, KB, PageSize, anon_mmap, create_file, create_memfd};

// ─── Constants ────────────────────────────────────────────────────────────────

/// Number of pages touched per iteration.
/// 128 × 4 KiB  = 512 KiB for standard pages.
/// 128 × 2 MiB  = 256 MiB for huge pages (requires 256 MiB of huge-page pool).
const N_FAULT_PAGES: usize = 128;

// ─── Memory backing ───────────────────────────────────────────────────────────

/// How a memory region is backed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backing {
    /// Anonymous private pages, zero-filled on first access.
    Anonymous,
    /// A real on-disk file (governed by BENCH_DIR). Page cache mediates faults.
    File,
    /// An anonymous in-memory file (memfd / tmpfs). Page cache mediates faults.
    Memfd,
}

impl Backing {
    const ALL: &'static [Self] = &[Self::Anonymous, Self::File, Self::Memfd];

    fn label(self) -> &'static str {
        match self {
            Self::Anonymous => "anon",
            Self::File => "file",
            Self::Memfd => "memfd",
        }
    }

    /// File-backed huge-page mappings require a hugetlbfs mount; a plain file
    /// on a regular filesystem cannot be mmap'd with MAP_HUGETLB.
    fn supports_huge_pages(self) -> bool {
        matches!(self, Self::Anonymous | Self::Memfd)
    }
}

// ─── Mapping RAII ─────────────────────────────────────────────────────────────

enum MappingBacking {
    Anonymous,
    File { _file: std::fs::File }, // held only to keep the fd open and for Drop
    Memfd(RawFd),
}

/// An mmap region together with the resource that backs it.
struct Mapping {
    ptr: *mut c_void,
    size: usize,
    _backing: MappingBacking,
}

impl Mapping {
    fn new(size: usize, backing: Backing, page_size: PageSize) -> Self {
        match backing {
            Backing::Anonymous => {
                let ptr = unsafe { anon_mmap(size, page_size) };
                Self { ptr, size, _backing: MappingBacking::Anonymous }
            }

            Backing::File => {
                assert_eq!(
                    page_size,
                    PageSize::Standard,
                    "file-backed huge-page mappings require a hugetlbfs mount",
                );
                let file = create_file(size);
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        size,
                        PROT_READ | PROT_WRITE,
                        MAP_PRIVATE,
                        file.as_raw_fd(),
                        0,
                    )
                };
                assert_ne!(
                    ptr,
                    libc::MAP_FAILED,
                    "mmap(file) failed: {}",
                    std::io::Error::last_os_error(),
                );
                Self { ptr, size, _backing: MappingBacking::File { _file: file } }
            }

            Backing::Memfd => {
                // For MFD_HUGETLB fds the filesystem type (hugetlbfs) already
                // determines the page size, so MAP_HUGETLB is not needed here.
                let fd = create_memfd(size, page_size);
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        size,
                        PROT_READ | PROT_WRITE,
                        MAP_PRIVATE,
                        fd,
                        0,
                    )
                };
                assert_ne!(
                    ptr,
                    libc::MAP_FAILED,
                    "mmap(memfd) failed: {}",
                    std::io::Error::last_os_error(),
                );
                Self { ptr, size, _backing: MappingBacking::Memfd(fd) }
            }
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.size) };
        if let MappingBacking::Memfd(fd) = &self._backing {
            unsafe { libc::close(*fd) };
        }
        // MappingBacking::File is closed automatically via its Drop.
    }
}

// ─── Benchmark ────────────────────────────────────────────────────────────────

/// Page-fault latency: time to fault in N_FAULT_PAGES pages after MADV_DONTNEED
/// strips them from the page table. write_volatile is used so anonymous accesses
/// trigger real page allocation rather than mapping the shared zero-page.
///
/// Dimensions:
///   backing   — anonymous | file (BENCH_DIR filesystem) | memfd (tmpfs)
///   page_size — standard 4 KiB | huge 2 MiB  (file+huge skipped: needs hugetlbfs)
fn bench_page_faults(c: &mut Criterion) {
    let mut group = c.benchmark_group("page_fault");

    for &backing in Backing::ALL {
        for &page_size in PageSize::ALL {
            if page_size == PageSize::Huge && !backing.supports_huge_pages() {
                continue;
            }

            let page_bytes = match page_size {
                PageSize::Standard => 4 * KB,
                PageSize::Huge => HUGE_PAGE_SIZE,
            };
            let total_size = N_FAULT_PAGES * page_bytes;

            group.throughput(Throughput::Elements(N_FAULT_PAGES as u64));
            group.bench_function(
                BenchmarkId::new(backing.label(), page_size.label()),
                |b| {
                    let mapping = Mapping::new(total_size, backing, page_size);
                    let ptr = mapping.ptr;

                    b.iter_batched(
                        // Setup (untimed): strip all pages from the page table.
                        || unsafe { libc::madvise(ptr, total_size, libc::MADV_DONTNEED) },
                        // Routine (timed): one write per page triggers a fault.
                        |_| {
                            for offset in (0..total_size).step_by(page_bytes) {
                                unsafe {
                                    std::ptr::write_volatile(
                                        (ptr as *mut u8).add(offset),
                                        0u8,
                                    );
                                }
                            }
                        },
                        criterion::BatchSize::PerIteration,
                    );

                    drop(mapping);
                },
            );
        }
    }

    group.finish();
}

// ─── Criterion wiring ─────────────────────────────────────────────────────────

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .measurement_time(Duration::from_secs(30))
        .warm_up_time(Duration::from_secs(3));
    targets = bench_page_faults
}
criterion_main!(benches);
