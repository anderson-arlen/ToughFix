# ToughFix dependency patch

This is Simba 0.9.1 from crates.io, published from upstream commit
`e7fe5c23339f1f3be2ddf76241dbb245b4899859`. Its source files are unchanged.

The only modification is in `Cargo.toml`: the `paste` dependency is aliased to
the maintained `pastey` 0.2.3 crate. The existing `paste::item!` calls therefore
use the successor macro implementation. This removes the archived `paste`
dependency (RUSTSEC-2024-0436) without changing numerical algorithms or upgrading
the types shared by nalgebra, Levenberg–Marquardt, and ode_solvers.

Upstream: <https://github.com/dimforge/simba/tree/e7fe5c23339f1f3be2ddf76241dbb245b4899859>

Successor macro crate: <https://github.com/AS1100K/pastey>

The original Apache-2.0 license and source notices are preserved. ToughFix's
numerical regression tests cover the patched dependency through its consumers.
