# Gears development

## What is Gears

Gears (the current repo) is a (coding) agent harness that compiles natively
for Motor OS.

To determine which OS you are running, run `uname`.

## Lorry workflow

`lorry fetch` - fetch dependencies of the repo
`lorry vendor` (+ 'y') - approve dependencies of the repo
`lorry build [--release]`
`lorry test [--release]`
(etc). 

Run `lorry test` with `GEARS_LOCK_OWNER` unset: a Gears session exports it,
and the `run_lock` tests then fail with "inherited Gears lock owner is not a
live ancestor" (or a lock count of 0) because they inherit this session's
owner. Use `unset GEARS_LOCK_OWNER; lorry test` in one command, since the
shell restores the export for each new command.
