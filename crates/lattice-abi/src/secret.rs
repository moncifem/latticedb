//! A wrapper that keeps secrets out of logs, traces, error messages and memory dumps.

use std::fmt;
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// A secret value. It never prints its contents (`Debug` and `Display` both show a placeholder),
/// is wiped from memory when dropped, and cannot be compared with `==`; use [`Secret::ct_eq`] for a
/// constant-time comparison. Reading the value takes an explicit, greppable call: [`Secret::expose`].
pub struct Secret<T: Zeroize>(T);

impl<T: Zeroize> Secret<T> {
    /// Wraps a value.
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// The only way to see the value. Callers must not log it.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl Secret<Vec<u8>> {
    /// Constant-time equality, so comparison time does not reveal where two secrets differ.
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<T: Zeroize> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl<T: Zeroize> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    const NEEDLE: &str = "hunter2-very-secret";

    #[test]
    fn formatting_never_reveals_the_value() {
        let s = Secret::new(NEEDLE.to_string());
        for text in [format!("{s:?}"), format!("{s}"), format!("{s:#?}")] {
            assert!(!text.contains(NEEDLE) && !text.contains("hunter"), "{text}");
        }
        let nested = vec![Secret::new(NEEDLE.as_bytes().to_vec())];
        assert!(
            !format!("{nested:?}").contains("104"),
            "no byte values either"
        );
        assert_eq!(s.expose(), NEEDLE);
    }

    #[test]
    fn constant_time_comparison_is_correct() {
        let a = Secret::new(b"abc".to_vec());
        assert!(a.ct_eq(&Secret::new(b"abc".to_vec())));
        assert!(!a.ct_eq(&Secret::new(b"abd".to_vec())));
        assert!(!a.ct_eq(&Secret::new(b"ab".to_vec())));
    }

    /// Collects everything a subscriber would be handed, as text.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<String>>);

    impl Visit for Capture {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            if let Ok(mut s) = self.0.lock() {
                s.push_str(&format!("{}={value:?};", field.name()));
            }
        }
    }

    impl Subscriber for Capture {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, a: &Attributes<'_>) -> Id {
            a.record(&mut self.clone());
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, v: &Record<'_>) {
            v.record(&mut self.clone());
        }
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, e: &Event<'_>) {
            e.record(&mut self.clone());
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    #[test]
    fn tracing_output_contains_no_secret_values() {
        let cap = Capture::default();
        tracing::subscriber::with_default(cap.clone(), || {
            let s = Secret::new(NEEDLE.to_string());
            let b = Secret::new(NEEDLE.as_bytes().to_vec());
            let span = tracing::info_span!("login", token = ?s, key = %b);
            let _g = span.enter();
            tracing::info!(secret = ?s, shown = %s, bytes = ?b, "user logged in");
            tracing::error!(?s, "failed with {s} and {s:?}");
        });
        let out = cap.0.lock().map(|g| g.clone()).unwrap_or_default();
        assert!(out.contains("redacted"), "something was logged: {out}");
        assert!(
            !out.contains(NEEDLE) && !out.contains("hunter"),
            "secret leaked into tracing output: {out}"
        );
    }

    #[test]
    fn the_placeholders_are_exact() {
        let s = Secret::new(vec![1u8, 2, 3]);
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{s}"), "<redacted>");
    }

    #[test]
    fn dropping_wipes_the_value() {
        use std::cell::Cell;
        use std::rc::Rc;
        struct Probe(Rc<Cell<u32>>);
        impl Zeroize for Probe {
            fn zeroize(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let wiped = Rc::new(Cell::new(0));
        drop(Secret::new(Probe(wiped.clone())));
        assert_eq!(wiped.get(), 1);
    }
}
