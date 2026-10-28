//! The small pointer-lifetime boundary of the opt-in io_uring experiment.
//!
//! Exactly one request is in flight. A method returns only after consuming its completion, so
//! the caller keeps its borrowed file and buffer alive throughout the operation. Unexpected ring
//! submission failure or unwinding with an outstanding request aborts rather than allowing a buffer
//! to be freed while the kernel may still use it. Ordinary completed I/O errors are returned.

use io_uring::{IoUring, opcode, squeue, types};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

/// One synchronous io_uring queue. It does not create a runtime or background polling thread.
pub struct Ring {
    ring: IoUring,
}

// No request may outlive the safe method's borrowed file/buffer, even during unwinding.
struct InFlight(bool);
impl Drop for InFlight {
    fn drop(&mut self) {
        if self.0 {
            eprintln!(
                "fatal: io_uring request outstanding; refusing to release borrowed I/O memory"
            );
            std::process::abort();
        }
    }
}

impl Ring {
    /// Creates a queue, reporting kernel/sandbox restrictions instead of falling back to std I/O.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            ring: IoUring::new(8)?,
        })
    }

    // The entry is built only by this module, and every public method retains its borrowed file
    // and buffer until this function consumes the request's completion.
    fn execute(&mut self, entry: squeue::Entry) -> io::Result<usize> {
        let mut guard = InFlight(false);
        // SAFETY: entries only originate in read/write/sync below. The public method retains the
        // file and buffer while this function waits. Only one operation can be queued at a time.
        unsafe { self.ring.submission().push(&entry) }
            .map_err(|_| io::Error::other("io_uring submission queue unexpectedly full"))?;
        guard.0 = true;
        loop {
            match self.ring.submit_and_wait(1) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // A submit failure can leave a queued/in-flight pointer. The guard aborts before
                // any borrowed storage can be released; an error is not a proof of cancellation.
                Err(_) => {
                    return Err(io::Error::other(
                        "io_uring submit failed with request outstanding",
                    ));
                }
            }
            if let Some(cqe) = self.ring.completion().next() {
                guard.0 = false;
                let result = cqe.result();
                return if result < 0 {
                    Err(io::Error::from_raw_os_error(-result))
                } else {
                    usize::try_from(result)
                        .map_err(|_| io::Error::other("invalid completion length"))
                };
            }
        }
    }

    /// Reads at a fixed offset into a borrowed buffer, waiting until that buffer is no longer in use.
    pub fn read(&mut self, file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
        let len = u32::try_from(bytes.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "read exceeds u32 request limit",
            )
        })?;
        self.execute(
            opcode::Read::new(types::Fd(file.as_raw_fd()), bytes.as_mut_ptr(), len)
                .offset(offset)
                .build(),
        )
    }

    /// Writes a borrowed buffer at a fixed offset and waits for completion. Partial writes are visible
    /// to the caller and must be retried by the adapter before it acknowledges the append.
    pub fn write(&mut self, file: &File, bytes: &[u8], offset: u64) -> io::Result<usize> {
        let len = u32::try_from(bytes.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "write exceeds u32 request limit",
            )
        })?;
        self.execute(
            opcode::Write::new(types::Fd(file.as_raw_fd()), bytes.as_ptr(), len)
                .offset(offset)
                .build(),
        )
    }

    /// Issues a full fsync (not data-only), consuming its completion before returning.
    pub fn sync(&mut self, file: &File) -> io::Result<()> {
        self.execute(opcode::Fsync::new(types::Fd(file.as_raw_fd())).build())
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::io::Write;

    #[test]
    fn completed_errors_do_not_poison_later_requests() {
        let root = std::env::temp_dir().join(format!("lattice-ring-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let mut ring = Ring::new().expect("Linux runner must allow io_uring for this opt-in test");
        let dir = File::open(&root).unwrap();
        assert!(ring.write(&dir, b"bad directory write", 0).is_err());
        let path = root.join("data");
        let mut file = File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.write_all(b"abc").unwrap();
        let mut out = [0u8; 3];
        assert_eq!(ring.read(&file, &mut out, 0).unwrap(), 3);
        assert_eq!(&out, b"abc");
        assert_eq!(ring.write(&file, b"xyz", 0).unwrap(), 3);
        ring.sync(&file).unwrap();
        assert_eq!(ring.read(&file, &mut out, 0).unwrap(), 3);
        assert_eq!(&out, b"xyz");
        assert_eq!(ring.read(&file, &mut out, 3).unwrap(), 0);
        drop(file);
        drop(dir);
        drop(ring);
        std::fs::remove_dir_all(root).unwrap();
    }
}
