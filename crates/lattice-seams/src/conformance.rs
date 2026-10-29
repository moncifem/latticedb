//! A conformance suite every [`Io`] implementation, real or simulated, must pass.

use crate::{Io, IoError};

/// Runs the contract checks against `io`. Panics with a clear message on the first violation.
#[allow(clippy::panic, clippy::expect_used)]
pub fn run<I: Io>(io: &mut I) {
    io.create_dir_all("conf").expect("create dir");
    assert!(!io.exists("conf/a"), "fresh file must not exist");
    assert_eq!(io.len("conf/a"), Err(IoError::NotFound("conf/a".into())));

    assert_eq!(
        io.read_prefix("conf/missing", 0),
        Err(IoError::NotFound("conf/missing".into()))
    );
    io.append("conf/a", b"hello").expect("append");
    io.append("conf/a", b" world").expect("append");
    assert_eq!(io.read("conf/a").expect("read"), b"hello world");
    assert_eq!(io.len("conf/a").expect("len"), 11);
    assert_eq!(io.read_prefix("conf/a", 0).expect("zero prefix"), b"");
    let prefix = io.read_prefix("conf/a", 7).expect("pending prefix");
    assert_eq!(prefix, b"hello w");
    assert!(prefix.capacity() <= 7, "prefix allocation exceeds request");
    assert_eq!(
        io.read_prefix("conf/a", 32).expect("short prefix"),
        b"hello world"
    );

    io.sync("conf/a").expect("sync");
    io.sync_dir("conf").expect("sync dir");

    io.truncate("conf/a", 5).expect("truncate");
    assert_eq!(io.read("conf/a").expect("read"), b"hello");
    assert_eq!(
        io.read_prefix("conf/a", 32).expect("truncated prefix"),
        b"hello"
    );

    io.append("conf/a", b"!").expect("append after truncate");
    assert_eq!(
        io.read_prefix("conf/a", 8)
            .expect("durable and pending prefix"),
        b"hello!"
    );

    io.append("conf/b", b"x").expect("append b");
    assert_eq!(
        io.list("conf").expect("list"),
        vec!["a".to_string(), "b".to_string()]
    );

    io.remove("conf/b").expect("remove");
    assert!(!io.exists("conf/b"));
    assert_eq!(io.remove("conf/b"), Err(IoError::NotFound("conf/b".into())));
    assert_eq!(io.list("conf").expect("list"), vec!["a".to_string()]);

    // Locks exclude a second taker until released.
    io.lock("conf/LOCK").expect("first lock");
    assert!(
        io.lock("conf/LOCK").is_err(),
        "a held lock cannot be taken again"
    );
    io.unlock("conf/LOCK");
    io.lock("conf/LOCK").expect("lock after unlock");
    io.unlock("conf/LOCK");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// An `Io` that forgets every append, which the suite must refuse.
    #[derive(Default)]
    struct Forgetful(BTreeMap<String, Vec<u8>>);

    impl Io for Forgetful {
        fn exists(&self, path: &str) -> bool {
            self.0.contains_key(path)
        }
        fn len(&self, path: &str) -> Result<u64, IoError> {
            self.0
                .get(path)
                .map(|v| v.len() as u64)
                .ok_or_else(|| IoError::NotFound(path.into()))
        }
        fn read(&self, path: &str) -> Result<Vec<u8>, IoError> {
            self.0
                .get(path)
                .cloned()
                .ok_or_else(|| IoError::NotFound(path.into()))
        }
        fn read_prefix(&self, path: &str, max: usize) -> Result<Vec<u8>, IoError> {
            self.0
                .get(path)
                .map(|v| v.get(..max.min(v.len())).unwrap_or(&[]).to_vec())
                .ok_or_else(|| IoError::NotFound(path.into()))
        }
        fn append(&mut self, path: &str, _data: &[u8]) -> Result<(), IoError> {
            self.0.entry(path.to_string()).or_default();
            Ok(())
        }
        fn truncate(&mut self, _path: &str, _len: u64) -> Result<(), IoError> {
            Ok(())
        }
        fn sync(&mut self, _path: &str) -> Result<(), IoError> {
            Ok(())
        }
        fn sync_dir(&mut self, _dir: &str) -> Result<(), IoError> {
            Ok(())
        }
        fn list(&self, _dir: &str) -> Result<Vec<String>, IoError> {
            Ok(Vec::new())
        }
        fn remove(&mut self, path: &str) -> Result<(), IoError> {
            self.0
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| IoError::NotFound(path.into()))
        }
        fn create_dir_all(&mut self, _dir: &str) -> Result<(), IoError> {
            Ok(())
        }
        fn lock(&mut self, _path: &str) -> Result<(), IoError> {
            Ok(())
        }
        fn unlock(&mut self, _path: &str) {}
    }

    #[test]
    #[should_panic]
    fn the_suite_rejects_an_io_that_loses_data() {
        run(&mut Forgetful::default());
    }
}
