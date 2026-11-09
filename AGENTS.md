# Gears development

## What is Gears and Motor OS

Gears (the current repo) is a (coding) agent harness that compiles natively
for Motor OS and Linux (and can be cross-compiled from Linux for Motor OS).

To determine which OS you are running, run `uname`.

Motor OS is NOT Unix, but it has a posixy shell (rush) and a bunch of posixy
commands in $PATH (run `echo $PATH`).

Note that there is no `git`, but `gix` is close enough. There is no `cargo`,
but `lorry` is close enough.

## Lorry workflow

`lorry fetch` - fetch dependencies of the repo
`lorry vendor` (+ 'y') - approve dependencies of the repo
`lorry build [--release]`
`lorry test [--release]`
(etc). 

## Motor OS documentation

/devtools/www/ should contain some high-level documents on Motor OS;
if not there, ask the user where to find them.

Use `curl` to inspect crates.io and github.com.

## Additional notes

* Use /none/tmp/ as a scratch/temporary directory.
* Motor OS does not have '/dev/' or '/dev/null'. Use `> /none/tmp/foo` instead
  of `> /dev/null`
* There's no `cmp` (yet), use `diff`
* There's no `git`, so instead of `git diff` use `diff` or `gix diff`

## How to work on Gears

Prefer secure, correct, simple Rust using std and native Motor OS APIs.
Keep handwritten changes in small patches and add comments sparingly.
Do not commit without explicit instructions. Format with the repository-selected
Rust toolchain and introduce no compiler or Clippy warnings.
