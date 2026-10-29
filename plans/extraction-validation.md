# Standalone extraction validation

Validated on 2026-10-28 with the repository-selected toolchain.

- Linux: `sh tests/full-test.sh --release` passed, including batteries workflows.
- Linux and Motor OS: Clippy with warnings denied passed for all targets.
- Motor OS: sources copied into `/devtools/src/gears` on a release developer
  image; `lorry vendor --accept-all` followed by
  `rush tests/full-test.sh --release` passed. Both binaries and the smoke
  examples built natively. Unit/integration tests, real TLS mock-provider
  checks, shipped tools, and batteries memory/skills/compiler workflows all ran
  inside Motor OS and passed.

No tests were added during extraction. Platform coverage exceptions are listed
in README.md. Each gate runs its mock providers and test programs on the same
OS; source transport to the developer VM was a manual acceptance procedure.

The native image includes Rush fixes for descriptor duplication and expansion
I/O/order (Motor OS commits 16ecfa16, 06befeeb, ed10888b).

An earlier native cancellation run found a descendant still active after
cleanup reported success. The final native gate passed without weakening that
assertion or changing cleanup. The earlier finding remains unresolved:
Motor OS `kill_pid` requests asynchronous termination, while Gears' native
`kill_tree` immediately enumerates the next generation and reports success
without confirming termination. A bounded confirmation wait in Gears is
proposed, pending user guidance. No retries or timeout increases were added.
