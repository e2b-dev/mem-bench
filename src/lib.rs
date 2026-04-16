use std::ffi::CString;
use std::os::unix::io::RawFd;

// ─── Constants ────────────────────────────────────────────────────────────────

pub const KB: usize = 1024;
pub const MB: usize = 1024 * KB;
pub const GB: usize = 1024 * MB;

/// Default huge page size on x86-64. Sizes must be a multiple of this to use
/// MAP_HUGETLB / MFD_HUGETLB.
pub const HUGE_PAGE_SIZE: usize = 2 * MB;

// ─── Page size ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSize {
    /// Regular 4 KiB pages.
    Standard,
    /// 2 MiB huge pages via MAP_HUGETLB / MFD_HUGETLB.
    Huge,
}

impl PageSize {
    pub const ALL: &'static [Self] = &[Self::Standard, Self::Huge];

    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "4k",
            Self::Huge => "2m",
        }
    }

    /// MAP_HUGETLB / MFD_HUGETLB require the allocation size to be a multiple
    /// of the huge page size.
    pub fn is_size_compatible(self, size: usize) -> bool {
        match self {
            Self::Standard => true,
            Self::Huge => size % HUGE_PAGE_SIZE == 0,
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Returns the directory used for real on-disk files.
/// Override with the BENCH_DIR environment variable to target a specific
/// filesystem (e.g. `BENCH_DIR=/mnt/nvme cargo bench`).
pub fn bench_dir() -> std::path::PathBuf {
    std::env::var("BENCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("current_dir"))
}

/// Create an anonymous, private mmap of `size` bytes.
pub unsafe fn anon_mmap(size: usize, page_size: PageSize) -> *mut libc::c_void {
    let flags = match page_size {
        PageSize::Standard => libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
        PageSize::Huge => libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_HUGETLB,
    };
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            flags,
            -1,
            0,
        )
    };
    assert_ne!(
        ptr,
        libc::MAP_FAILED,
        "mmap(size={size}, {page_size:?}) failed: {}",
        std::io::Error::last_os_error(),
    );
    ptr
}

/// Create a memfd pre-sized to `size` bytes.
pub fn create_memfd(size: usize, page_size: PageSize) -> RawFd {
    let name = CString::new("bench").unwrap();
    let flags: libc::c_uint = match page_size {
        PageSize::Standard => 0,
        PageSize::Huge => libc::MFD_HUGETLB,
    };
    let fd = unsafe { libc::memfd_create(name.as_ptr(), flags) };
    assert!(
        fd >= 0,
        "memfd_create({page_size:?}) failed: {}",
        std::io::Error::last_os_error(),
    );
    let ret = unsafe { libc::ftruncate(fd, size as libc::off_t) };
    assert_eq!(ret, 0, "ftruncate failed: {}", std::io::Error::last_os_error());
    fd
}

/// Create a real on-disk temporary file pre-sized to `size` bytes.
/// The file lives in `bench_dir()` and is unlinked automatically on drop.
pub fn create_file(size: usize) -> std::fs::File {
    let dir = bench_dir();
    let file = tempfile::tempfile_in(&dir)
        .unwrap_or_else(|e| panic!("tempfile_in({dir:?}) failed: {e}"));
    file.set_len(size as u64).expect("set_len failed");
    file
}

/// Fill a file descriptor with a non-zero repeating byte pattern.
///
/// Uses a temporary `MAP_SHARED` mmap rather than `pwrite` because hugetlbfs
/// file descriptors (created with `MFD_HUGETLB`) do not support `pwrite` and
/// return `EINVAL`.  A shared mapping works for both regular files and
/// hugetlbfs fds.
pub fn fill_fd(fd: RawFd, size: usize) {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    assert_ne!(
        ptr,
        libc::MAP_FAILED,
        "fill_fd mmap failed: {}",
        std::io::Error::last_os_error(),
    );
    unsafe { std::ptr::write_bytes(ptr as *mut u8, 0xAB, size) };
    unsafe { libc::munmap(ptr, size) };
}
