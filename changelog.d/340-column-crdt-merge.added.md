Column-level CRDT merge (WP42, #244, part 1; RFC 0011 draft): `lattice-engines::merge` merges remote row changes per column with a
last-writer-wins register, an observed-remove set or whole-row last-writer-wins for plain columns, and a row delete that
hides writes stamped at or before it. It is an in-memory function with no byte encoding, not yet wired to the row store or sync.
