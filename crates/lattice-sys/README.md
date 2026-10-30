# lattice-sys

Layer L0.

**Responsibility.** The only crate allowed to contain unsafe code (io_uring, mmap wrappers).

**Must not.** Contain business logic. Used by lattice-seams (I/O) and directly by latticed (stop signals).

Allowed workspace dependencies: none.
