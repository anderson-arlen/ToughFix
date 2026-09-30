# Local DOP853 stage-time correction

This is ode_solvers 0.6.2, Apache-2.0 licensed (see LICENSE), copied from
the Cargo registry. Original source and attribution are preserved.

In `src/butcher_tableau.rs`, the DOP853 `C` array's indices 11 and
12 are corrected from `0.0` to `1.0`. The stage-12 row of `A` sums to one,
and the corresponding SciPy DOP853 coefficient is one. Stage 13 is the
endpoint derivative, also at one. Stage 12 participates in the integration
result; evaluating a nonautonomous force at the start of the step introduces
drift. A time-independent two-body orbit does not expose this bug.

In `src/dop853.rs`, dense endpoint output also requires the integrator's current
time to be at the endpoint. Originally, as soon as the next requested output
was the endpoint, it extrapolated that value from the previous step even when
the integrator had not reached it. The added condition prevents extrapolation
outside the accepted dense-output interval.

ToughFix includes an analytic time-dependent integration regression and an
independent SciPy comparison using the same fitted orbital initial conditions.
The dependency is pinned through Cargo's local `[patch.crates-io]` override;
no changes to the user's Cargo cache are required.
