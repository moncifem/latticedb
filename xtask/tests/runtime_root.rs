//! One executable must select the caller's checkout, never its compile-time source directory.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "lattice-xtask-root-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn checkout(&self, name: &str) -> PathBuf {
        let root = self.0.join(name);
        std::fs::create_dir_all(root.join("xtask")).unwrap();
        std::fs::create_dir_all(root.join("docs/adr/nested")).unwrap();
        for marker in ["Cargo.toml", "layers.toml", "xtask/Cargo.toml"] {
            std::fs::write(root.join(marker), "").unwrap();
        }
        std::fs::write(
            root.join("docs/adr/0001-choice.md"),
            format!("# {name}\n\nStatus: proposed\n"),
        )
        .unwrap();
        root
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn run(binary: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(binary)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn copied_binary_indexes_only_each_calling_worktree_and_checks_its_own_files() {
    let fixture = Fixture::new();
    let a = fixture.checkout("first");
    let b = fixture.checkout("second");
    let binary = fixture.0.join("copied-xtask");
    std::fs::copy(env!("CARGO_BIN_EXE_xtask"), &binary).unwrap();
    for (selected, other, title) in [(&a, &b, "first"), (&b, &a, "second")] {
        let previous_other = std::fs::read(other.join("docs/adr/README.md")).ok();
        let result = run(
            &binary,
            &selected.join("docs/adr/nested"),
            &["adr", "index"],
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let index = std::fs::read_to_string(selected.join("docs/adr/README.md")).unwrap();
        assert!(index.contains(title));
        assert_eq!(
            std::fs::read(other.join("docs/adr/README.md")).ok(),
            previous_other
        );
    }
    std::fs::create_dir_all(a.join("formats/corpus")).unwrap();
    std::fs::create_dir_all(b.join("formats/corpus")).unwrap();
    std::fs::write(a.join("formats/corpus/broken.golden"), b"missing metadata").unwrap();
    assert!(!run(&binary, &a, &["check-format-corpus"]).status.success());
    assert!(run(&binary, &b, &["check-format-corpus"]).status.success());
}

#[test]
fn unrelated_directory_is_refused_before_a_mutating_command() {
    let fixture = Fixture::new();
    let output = run(
        Path::new(env!("CARGO_BIN_EXE_xtask")),
        &fixture.0,
        &["adr", "index"],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("run xtask from inside a Lattice checkout")
    );
    assert!(!fixture.0.join("docs").exists());
}
