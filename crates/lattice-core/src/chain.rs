//! Tamper-evident hash chain, the primitive behind the audit log (invariant I13).

/// A link: the entry bytes and the hash that commits to them and to everything before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Entry bytes.
    pub entry: Vec<u8>,
    /// `blake3(prev_hash || len(entry) || entry)`.
    pub hash: [u8; 32],
}

fn link_hash(prev: &[u8; 32], entry: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(prev);
    h.update(&(entry.len() as u64).to_le_bytes());
    h.update(entry);
    *h.finalize().as_bytes()
}

/// Builds a chain.
#[derive(Debug, Clone, Default)]
pub struct Chain {
    head: [u8; 32],
}

impl Chain {
    /// An empty chain (head is all zeroes).
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an entry and returns its link.
    pub fn append(&mut self, entry: &[u8]) -> Link {
        let hash = link_hash(&self.head, entry);
        self.head = hash;
        Link {
            entry: entry.to_vec(),
            hash,
        }
    }

    /// The current head hash.
    pub fn head(&self) -> [u8; 32] {
        self.head
    }
}

/// Verifies the chain **and** that it ends at `expected_head`. Internal consistency alone cannot
/// detect deleted trailing links; only an independently stored head can.
pub fn verify_with_head(links: &[Link], expected_head: [u8; 32]) -> Verdict {
    match verify(links) {
        Verdict::Valid => {
            let head = links.last().map_or([0u8; 32], |l| l.hash);
            if head == expected_head {
                Verdict::Valid
            } else {
                Verdict::BrokenAt(links.len())
            }
        }
        broken => broken,
    }
}

/// Where verification failed, if it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every link checks out.
    Valid,
    /// The link at this index does not match: the entry or an earlier one was altered, removed,
    /// inserted or reordered.
    BrokenAt(usize),
}

/// Recomputes the chain from the start and compares every hash.
pub fn verify(links: &[Link]) -> Verdict {
    let mut prev = [0u8; 32];
    for (i, l) in links.iter().enumerate() {
        if link_hash(&prev, &l.entry) != l.hash {
            return Verdict::BrokenAt(i);
        }
        prev = l.hash;
    }
    Verdict::Valid
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn build(entries: &[Vec<u8>]) -> Vec<Link> {
        let mut c = Chain::new();
        entries.iter().map(|e| c.append(e)).collect()
    }

    #[test]
    fn inv_i13_suffix_deletion_is_only_caught_with_a_trusted_head() {
        let mut c = Chain::new();
        let links: Vec<Link> = [b"a".as_slice(), b"b", b"c"]
            .iter()
            .map(|e| c.append(e))
            .collect();
        let head = c.head();
        assert_eq!(verify_with_head(&links, head), Verdict::Valid);
        assert_eq!(
            verify(&links[..2]),
            Verdict::Valid,
            "internal consistency cannot see the loss"
        );
        assert_ne!(verify_with_head(&links[..2], head), Verdict::Valid);
        assert_ne!(verify_with_head(&[], head), Verdict::Valid);
    }

    proptest! {
        #[test]
        fn untouched_chains_verify(entries in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..20), 0..10)) {
            prop_assert_eq!(verify(&build(&entries)), Verdict::Valid);
        }

        #[test]
        fn altering_any_entry_is_detected_at_or_before_that_entry(entries in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 1..20), 1..10), which in any::<usize>()) {
            let mut links = build(&entries);
            let i = which % links.len();
            links[i].entry[0] ^= 1;
            prop_assert_eq!(verify(&links), Verdict::BrokenAt(i));
        }

        #[test]
        fn removing_or_swapping_links_is_detected(entries in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..20), 3..10)) {
            let links = build(&entries);
            let mut removed = links.clone();
            removed.remove(1);
            prop_assert_ne!(verify(&removed), Verdict::Valid);
            let mut swapped = links.clone();
            if swapped[0] != swapped[1] {
                swapped.swap(0, 1);
                prop_assert_ne!(verify(&swapped), Verdict::Valid);
            }
        }
    }
}
