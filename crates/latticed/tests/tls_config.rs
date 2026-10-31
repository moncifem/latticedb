//! TLS configuration import: exact size and chain-length bounds, key count, and redacted debug text.
//! Mutation testing showed none of these edges was pinned.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]

use latticed::TlsConfig;

fn material() -> (String, String) {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    (generated.cert.pem(), generated.signing_key.serialize_pem())
}

fn padded(pem: &str, total: usize) -> Vec<u8> {
    assert!(pem.len() <= total);
    let mut bytes = pem.as_bytes().to_vec();
    bytes.resize(total, b'\n');
    bytes
}

fn message(result: std::io::Result<TlsConfig>) -> String {
    result
        .map(|_| "accepted".to_string())
        .unwrap_or_else(|e| e.to_string())
}

#[test]
fn the_debug_text_never_shows_certificate_or_key_material() {
    let (cert, key) = material();
    let config = TlsConfig::from_pem(cert.as_bytes(), key.as_bytes()).unwrap();
    assert_eq!(
        format!("{config:?}"),
        "TlsConfig { certificate_and_key: [redacted] }"
    );
}

#[test]
fn certificate_and_key_sizes_are_bounded_exactly() {
    let (cert, key) = material();
    assert_eq!(
        message(TlsConfig::from_pem(&padded(&cert, 65_536), key.as_bytes())),
        "accepted"
    );
    assert_eq!(
        message(TlsConfig::from_pem(&padded(&cert, 65_537), key.as_bytes())),
        "TLS configuration exceeds bounds"
    );
    assert_eq!(
        message(TlsConfig::from_pem(cert.as_bytes(), &padded(&key, 16_384))),
        "accepted"
    );
    assert_eq!(
        message(TlsConfig::from_pem(cert.as_bytes(), &padded(&key, 16_385))),
        "TLS configuration exceeds bounds"
    );
    // Either input being too large is enough, with the other one fine.
    assert_eq!(
        message(TlsConfig::from_pem(
            &padded(&cert, 65_537),
            &padded(&key, 100 + key.len())
        )),
        "TLS configuration exceeds bounds"
    );
}

#[test]
fn a_chain_has_between_one_and_eight_certificates() {
    let (cert, key) = material();
    let chain = |n: usize| cert.repeat(n);
    assert_eq!(
        message(TlsConfig::from_pem(chain(1).as_bytes(), key.as_bytes())),
        "accepted"
    );
    assert_eq!(
        message(TlsConfig::from_pem(chain(8).as_bytes(), key.as_bytes())),
        "accepted"
    );
    for n in [0, 9] {
        assert_eq!(
            message(TlsConfig::from_pem(chain(n).as_bytes(), key.as_bytes())),
            "invalid TLS certificate chain",
            "{n} certificates"
        );
    }
}

#[test]
fn exactly_one_private_key_is_required() {
    let (cert, key) = material();
    let (_, other_key) = material();
    assert_eq!(
        message(TlsConfig::from_pem(cert.as_bytes(), b"")),
        "missing TLS private key"
    );
    assert_eq!(
        message(TlsConfig::from_pem(
            cert.as_bytes(),
            format!("{key}{other_key}").as_bytes()
        )),
        "multiple TLS private keys"
    );
}

/// The TLS key is read through `files::read_private`: an owner-only regular file in a directory that
/// group and others cannot write to. Each refusal below is derived from the mode, not from running
/// the reader on the same input twice.
#[cfg(unix)]
mod key_file {
    use latticed::files::read_private;
    use std::io::ErrorKind;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("latticed-tls-key-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn write(path: &Path, bytes: &[u8], mode: u32) {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_private_key_is_read_in_full_and_a_longer_one_is_refused() {
        let dir = scratch("bound");
        let key = dir.join("key.pem");
        write(&key, b"0123456789", 0o600);
        assert_eq!(&**read_private(&key, 10).unwrap(), b"0123456789");
        let refusal = read_private(&key, 9).unwrap_err();
        assert_eq!(refusal.kind(), ErrorKind::InvalidData);
        assert_eq!(refusal.to_string(), "credential file is too large");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_that_group_or_others_can_read_is_refused() {
        let dir = scratch("modes");
        let key = dir.join("key.pem");
        for mode in [0o640, 0o604, 0o644, 0o666] {
            write(&key, b"key", mode);
            let refusal = read_private(&key, 16).unwrap_err();
            assert!(
                refusal.to_string().contains("group or others"),
                "{mode:o}: {refusal}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_in_a_directory_that_group_or_others_can_write_is_refused() {
        let dir = scratch("dirs");
        let key = dir.join("key.pem");
        write(&key, b"key", 0o600);
        for mode in [0o777, 0o702, 0o720] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            let refusal = read_private(&key, 16).unwrap_err();
            assert!(
                refusal.to_string().contains("writable by group or others"),
                "{mode:o}: {refusal}"
            );
        }
        // Owner-only and read-only for others (0755) are both accepted.
        for mode in [0o700, 0o750, 0o755] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(&**read_private(&key, 16).unwrap(), b"key", "{mode:o}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_symlink_to_a_private_key_is_refused_and_its_target_is_not_read() {
        let dir = scratch("link");
        let target = dir.join("target.pem");
        let link = dir.join("link.pem");
        write(&target, b"key material", 0o600);
        symlink(&target, &link).unwrap();
        let refusal = read_private(&link, 64).unwrap_err();
        assert!(!refusal.to_string().contains("key material"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_fifo_is_refused_as_not_a_regular_file_without_blocking() {
        let dir = scratch("fifo");
        let fifo = dir.join("key.pem");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        std::fs::set_permissions(&fifo, std::fs::Permissions::from_mode(0o600)).unwrap();
        let refusal = read_private(&fifo, 64).unwrap_err();
        assert_eq!(refusal.to_string(), "secret path is not a regular file");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
