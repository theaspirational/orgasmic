# TASK-QRTT8 — credential mode survives boot-candidate parsing

The durable `Lifecycle::RunMeta` parser must bind `credential_mode`; matching
the literal `None` silently discards every run that recorded a resolved mode.

This verifier covers both the parser shape and its blast radius: each supported
field value yields a boot candidate, while a non-reattachable stdio runtime is
declined cleanly without leaking its supervisor lease.

Run `orgasmic verify TASK-QRTT8` from the repository root.
