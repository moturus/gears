# Gears development

Prefer secure, correct, simple Rust using std and native Motor OS APIs.
Keep handwritten changes in small patches and add comments sparingly.
Do not commit without explicit instructions. Format with the repository-selected
Rust toolchain and introduce no compiler or Clippy warnings.

Run `sh tests/full-test.sh --release` on Linux, or
`rush tests/full-test.sh --release` on Motor OS after `lorry vendor --accept-all`.
Tests execute locally and use local provider fixtures; do not add SSH, VM boot,
remote test execution or Internet-provider requests to a test gate.
Diagnose failures rather than adding retries, longer timeouts or ignored errors.
Changes to Motor OS, its runtime, stdlib or external dependency forks must be
discussed explicitly before implementation.
