#[cfg(any(not(windows), test))]
use std::fs::File;
use std::fs::TryLockError;
use std::io;
use std::ops::{Bound, RangeBounds};
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
use std::os::unix::io::AsRawFd;

#[cfg(any(
    all(target_os = "linux", not(target_env = "gnu")),
    target_vendor = "apple"
))]
use libc::flock as Flock;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use libc::flock64 as Flock;

/// A byte-range lock attempt in the convention of [`crate::StorageBackend`]: `Ok(false)` means a
/// conflicting lock is held elsewhere.
pub(crate) fn acquired(result: Result<(), TryLockError>) -> io::Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

/// Refuses the ranges that `FileBackend` refuses: an empty range, a bound past `u64::MAX`, and on
/// Linux and the Apple platforms a range whose offset or length `fcntl`'s signed arguments cannot
/// hold. std accepts some of these, reading a range past the last lockable offset as unbounded.
pub(crate) fn check_range(range: impl RangeBounds<u64>) -> io::Result<(u64, Option<u64>)> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty or unrepresentable lock range",
        )
    };
    let start = match range.start_bound() {
        Bound::Unbounded => 0,
        Bound::Included(start) => *start,
        Bound::Excluded(start) => start.checked_add(1).ok_or_else(invalid)?,
    };
    let end = match range.end_bound() {
        Bound::Unbounded => None,
        Bound::Included(end) => Some(end.checked_add(1).ok_or_else(invalid)?),
        Bound::Excluded(end) => Some(*end),
    };
    if end.is_some_and(|end| start >= end) {
        return Err(invalid());
    }
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    if start > i64::MAX as u64
        || end.is_some_and(|end| end - 1 > i64::MAX as u64 || end - start > i64::MAX as u64)
    {
        return Err(invalid());
    }
    Ok((start, end))
}

/// Retries a lock operation that a signal handler interrupted, which is not a failure to take the
/// lock: while a blocking lock waited, or before a non-blocking one checked for a conflict, which
/// Linux documents as most likely over NFS
pub(crate) fn retrying<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match op() {
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

/// Byte-range locks themselves are [`File::lock_range`] and its relatives. std has no query for
/// a conflicting lock, because Windows has none, so this keeps one.
#[cfg(any(not(windows), test))]
pub(crate) trait QueryLock {
    /// Whether an exclusive lock over the range would conflict with one already held.
    /// [`File::lock`] is included wherever it would in fact block a range lock
    fn query_lock(&self, _range: impl RangeBounds<u64>) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "byte-range locks are not supported on this platform",
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
impl QueryLock for File {}

// Windows has no query. FileBackend emulates one under its query mutex; test observers, which
// bypass that mutex to inspect a backend's locks, probe directly.
#[cfg(all(windows, test))]
impl QueryLock for File {
    fn query_lock(&self, range: impl RangeBounds<u64>) -> io::Result<bool> {
        let range = (range.start_bound().cloned(), range.end_bound().cloned());
        if acquired(self.try_lock_range(range))? {
            self.unlock_range(range)?;
            Ok(false)
        } else {
            Ok(true)
        }
    }
}

// The lock-type constants are c_int on Linux and already c_short on the Apple platforms
#[cfg(target_os = "linux")]
fn lock_type(kind: libc::c_int) -> libc::c_short {
    kind.try_into().unwrap()
}

#[cfg(target_vendor = "apple")]
fn lock_type(kind: libc::c_short) -> libc::c_short {
    kind
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn flock_struct(kind: libc::c_short, range: impl RangeBounds<u64>) -> io::Result<Flock> {
    let (start, end) = check_range(range)?;
    // Zeroed rather than written field by field: struct flock's layout differs between Linux
    // and the Apple platforms. A zero length covers all future growth.
    let mut lock: Flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind;
    lock.l_whence = libc::SEEK_SET.try_into().unwrap();
    // check_range bounds both by i64::MAX
    lock.l_start = start.try_into().unwrap();
    lock.l_len = end.map_or(0, |end| end - start).try_into().unwrap();
    Ok(lock)
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn fcntl_lock(file: &File, command: libc::c_int, lock: &mut Flock) -> libc::c_long {
    // GNU's off_t can be 32 bits. Use the kernel's large-file ABI directly: glibc's fcntl
    // translates through off_t, and its fcntl64 symbol requires glibc 2.28 or later.
    // x32 and RISC-V already use 64-bit offsets with SYS_fcntl.
    #[cfg(any(
        target_pointer_width = "64",
        target_arch = "x86_64",
        target_arch = "riscv32"
    ))]
    use libc::SYS_fcntl as SYSCALL;
    #[cfg(not(any(
        target_pointer_width = "64",
        target_arch = "x86_64",
        target_arch = "riscv32"
    )))]
    use libc::SYS_fcntl64 as SYSCALL;

    unsafe { libc::syscall(SYSCALL, file.as_raw_fd(), command, &raw mut *lock) }
}

#[cfg(any(
    all(target_os = "linux", not(target_env = "gnu")),
    target_vendor = "apple"
))]
fn fcntl_lock(file: &File, command: libc::c_int, lock: &mut Flock) -> libc::c_int {
    unsafe { libc::fcntl(file.as_raw_fd(), command, &raw mut *lock) }
}

/// The last lock failure, as `Unsupported` where the filesystem has no byte-range locks: EINVAL
/// is how the Apple platforms report one, and Linux a kernel without open file description
/// locks; ENOSYS, ENOTSUP and EOPNOTSUPP are `Unsupported` as std classifies them. Not ENOLCK,
/// which is a lock refused (the lock table is full, or a remote locking protocol failed)
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn lock_error() -> io::Error {
    let err = io::Error::last_os_error();
    if matches!(err.raw_os_error(), Some(code) if code == libc::EINVAL
        || code == libc::ENOTSUP
        || code == libc::EOPNOTSUPP)
    {
        io::Error::new(io::ErrorKind::Unsupported, err)
    } else {
        err
    }
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
impl QueryLock for File {
    fn query_lock(&self, range: impl RangeBounds<u64>) -> io::Result<bool> {
        let mut lock = flock_struct(lock_type(libc::F_WRLCK), range)?;
        retrying(|| {
            if fcntl_lock(self, libc::F_OFD_GETLK, &mut lock) == 0 {
                Ok(())
            } else {
                Err(lock_error())
            }
        })?;
        // The lock-type constants are c_int on Linux and c_short on the Apple platforms, so
        // both sides are widened rather than compared directly
        Ok(i32::from(lock.l_type) != i32::from(lock_type(libc::F_UNLCK)))
    }
}
