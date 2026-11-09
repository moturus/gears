# Gears

Gears is a small extensible agent harness that runs on Motor OS. Its
core owns provider streaming, one agent loop, cancellation, sessions, terminal
UIs, and five built-in tools. It does not prescribe a coding workflow.

## Run

Start `gears`. The first time, it asks which provider to use (the shipped
`gears.toml` offers OpenRouter, OpenAI and a local server), the local server's
address, a model from the provider's list or any other id, and the API key,
which is typed hidden and saved in the Gears directory. Later starts ask nothing; `/setup` asks again. A `-p` run never
asks. Nothing needs editing in `gears.toml`.

Each provider names its key file (`api_key_file`, kept in the Gears directory);
`OPENROUTER_API_KEY` overrides it for one run, and a provider without a key
file is sent no key. A model can also be given directly:

    gears --model provider/model
    gears -p "answer once" --model provider/model
    gears ask --model provider/model "provider diagnostic"

Gears chooses the full-screen UI when both input and output are terminals.
Use --ui line or --ui tui to choose explicitly.

Without `--config`, Gears reads `gears.toml` from the Gears directory. Run from
its own source tree (`lorry run` in the Gears source directory), it uses
the shipped `config/gears.toml` when the Gears directory has none, and asks
which to use when both exist. Keys, settings and sessions always stay in the
Gears directory. A provider address saved in `provider.toml` replaces the
configured one, and its host is allowed for egress.

Gears always provides five tools: `sh`, `read_file`, `write_file`, `patch` and
`session_search`. `sh` receives one string command and runs it in the selected
workspace through `/system/bin/rush -c` on Motor OS. It
is not a sandbox: a command has the same filesystem authority as Gears and may
leave the workspace. Stdout, stderr, status, timeout, cancellation, and
truncation remain distinct. The working directory and exports it saves live
beside a saved session, or in `sessions/ephemeral/` in the Gears directory for
an ephemeral one; Gears removes the ephemeral state at exit, and what a killed
Gears left there at its next start.

`read_file` reads numbered UTF-8 pages, and `write_file` and `patch` refuse to
change a file Gears has not read completely in this session. `session_search`
finds text in every saved session, across branches and workspaces.

The built-in tools are the only tools: Gears does not infer safe shell
commands. The provider credential is removed from the environment inherited by
tool processes.

## Configuration

The default configuration is `/user/cfg/gears/gears.toml`. Set
`GEARS_DIRECTORY` to an absolute path to select another Gears directory.
A minimal file is:

    version = 1

    [provider]
    base_url = "https://openrouter.ai/api/v1"
    model = "provider/model"

    [models]
    used = ["provider/model", "another/model"]

[config/gears.toml](config/gears.toml) is the complete starting point: copy it
into the Gears directory.

Important optional fields are:

    [net]
    egress_allowlist = ["openrouter.ai"]

    [provider]
    api_key_file = "/absolute/path/to/key"
    ca_cert = "/absolute/path/to/ca.pem"

    [runtime]
    max_tool_rounds = 256

    [context]
    window_tokens = 128000
    output_reserve_tokens = 16384
    recent_tail_tokens = 20000

An unknown key anywhere in the file is an error, so a typo is reported at
startup instead of being ignored.

`max_tool_rounds` bounds the model round trips in one turn of an unattended
run, where nothing can interrupt a model that keeps calling tools: the default
is 256 and the ceiling 4096. An interactive session has no such cap; a turn
runs until the model stops or Ctrl+C cancels it.

## Sessions

Saved sessions are versioned append-only JSONL trees grouped by canonical
workspace:

- /user/cfg/gears/sessions/

Each entry has an id and parent_id. Selecting an earlier entry changes only
the conversation branch; it does not undo commands or filesystem changes.
Only one process may write a saved session. A partial final record is ignored
after a crash, while earlier corruption is reported. A session holding an
entry of a kind this Gears does not write, or an unknown field, is refused.

Interactive UIs display the saved active branch when a session is opened or
selected, including messages from before compaction. The full-screen UI
keeps the latest 1 MiB of display text; Page Up and Page Down scroll it.
Displaying history does not rerun tools or request a model response.
One-shot text and JSONL output contain only the new run's output.

In the full-screen prompt, bare Enter sends; Ctrl+J, Shift+Enter, Ctrl+Enter,
and Alt+Enter insert a newline, and so does Enter typed right after a
backslash, which it replaces. The backslash form works in every terminal and
through every multiplexer, since it uses no key they might fail to report.
Shift+Enter and Ctrl+Enter require a terminal that reports those modifiers. Gears requests the Kitty keyboard protocol;
terminals that send the same bytes for modified and bare Enter cannot be
distinguished. Inside rmux, use a build that preserves Ctrl+J and modified
Enter and requests this reporting from the outer terminal.

Inside tmux, Gears also requests extended keys using `modifyOtherKeys`.
For an outer terminal that supports extended keys, add this to `~/.tmux.conf`
and reload it with `tmux source-file ~/.tmux.conf`:

    set -s extended-keys on
    set -as terminal-features 'xterm*:extkeys'
    set -sq extended-keys-format csi-u

The last setting selects the encoding Gears reads on tmux 3.5 and later;
`-q` leaves older tmux versions, which already use CSI-u, unchanged.
These settings cannot recover modifiers discarded by the outer terminal.
For example, VTE 0.84 sends the same CR for bare, Shift-, and Ctrl-Enter;
use Ctrl+J or Alt+Enter there, or a terminal that reports modified Enter.

Gears also locks its user directory for the lifetime of the run: it holds an
exclusive sys-io file lock on `gears.lock`, which the system releases when
Gears exits or is killed. A second independent Gears exits with
"Another gears session PID is running.
Exiting."; nested runs share their checked ancestor's lock. The file stays in
the Gears directory; the PID in it only names the current owner.

Library callers can use `session::read_snapshot(path)` or `Store::read(id)`
while the owning process writes. The immutable snapshot includes the usable
complete prefix, omits unfinished records and tool rounds, and leaves the
source file and writer lock untouched. `Store::clone_active_with_options`
can copy that snapshot into a saved or ephemeral child session.
The CLI accepts `--fork ID --ephemeral -p 'summarize this work'` to read a live
parent's usable history and run a child without saving its conversation.
Add `--fork-entry ENTRY` to end the copy at an exact saved entry, including
one from before a later compaction. An unknown entry or unfinished tool round
is refused; the source session remains unchanged and writable.

For machine-readable one-shot output, use `-p PROMPT --output jsonl`.
Version 1 starts with a `session` record containing `session_id`, `path`
(null for an ephemeral session), and `session_format_version`. Streaming
records have types `text`, `reasoning`, `tool_start`, `tool_output`,
`tool_result`, `notice`, `usage`, and `turn_end`. Tool results
include the call id, error flag, and three-state outcome. Every record is
one flushed JSON line; embedded newlines are escaped.

The last `final` record contains `success`, `answer`, `error`, and numeric
`usage` for this run, including compaction requests. `session_usage` includes
earlier history too. Unknown cost is null; `cost_is_partial` identifies a
partly priced total. A failed turn produces a final error and a nonzero
exit status. Errors before a session is ready use stderr.

Useful startup options are --continue, --resume ID, --session ID,
--fork ID, --ephemeral, and --name NAME. Interactive commands include /new,
/resume, /name, /session, /tree, /label, /fork, /clone, /compact, /tools,
/status, /setup, /reset, /help, and /quit. Use
/label ENTRY [TEXT] to set or clear an entry label. Fork selects an earlier
user prompt; clone copies the current active branch into a new file.

`gears reset` (or `/reset` in a running Gears) asks once, then removes
everything the user and Gears added: key files, saved settings and model
choices, and sessions in the Gears directory, and `.gears/` in the current
directory. It keeps what an installation provides: `gears.toml`. `/reset`
continues in a new session; the provider, model and key already loaded stay in
use until Gears restarts. `gears reset` refuses to run inside another Gears.

Compaction summarizes older active-branch context and retains a recent tail.
It appends a compaction entry but never deletes history. /compact accepts
optional focus text. Automatic compaction uses the configured context limit,
output reserve, and recent-tail size.

Prompt resources are loaded from the current installation when a session is
opened. Their hashes are appended as a runtime identity; a change produces one
notice. The five built-in tools are compiled in, so no tool directory or
manifest is scanned or installed.

A workspace's `AGENTS.md` is workspace instructions, and Gears sends it to the
model with the system prompt on every request: it is read when a session is
opened, at the start of every turn, and again right after a compaction, so an
edit takes effect at the next turn and a compaction cannot cost the model the
instructions it works under. A missing or empty file adds nothing; a file that
cannot be read is reported. Gears never writes or removes `AGENTS.md`.

## A llama.cpp server on the VM host

Build and boot the development image from the repository root after setting
up the Motor toolchain:

    make -j"$(nproc)" dev.img BUILD=release
    vm_images/release/run-dev.sh

With the standard TAP setup, the host address is 192.168.4.1 and the guest is
192.168.4.2. Restore the TAP after a host reboot with
`vm_images/release/create-tap.sh` if needed. On the host, start llama.cpp with
your model, using a context size the model supports:

    llama-server -m /absolute/path/to/model.gguf --alias local \
      --host 192.168.4.1 --port 8080 --ctx-size 32768 --parallel 1 --jinja

Tool use requires a model and chat template supporting tool calls. An
unauthenticated local server needs no key file. Save this as `/user/cfg/gears/gears.toml`:

```toml
version = 1

[provider]
base_url = "http://192.168.4.1:8080/v1"
model = "local"

[net]
egress_allowlist = ["192.168.4.1"]
plain_http_allowlist = ["192.168.4.1"]

[context]
window_tokens = 32768
output_reserve_tokens = 4096
recent_tail_tokens = 8192
```

The HTTP list must be a subset of the egress list. Both use exact host
matching: an entry permits any port on that host, not just port 8080. HTTP
sends prompts and any bearer key without encryption. HTTPS remains the
default; a TLS failure never falls back to HTTP. A plaintext `base_url`
whose host is missing from either list is refused when the config loads.
OpenRouter receives earlier assistant reasoning on subsequent turns. Other
hosts omit it by default; set `reasoning_echo = true` or `false` under
`[provider]` or a named `[providers.NAME]` to override the host default.

A provider without `api_key_file` is sent no `Authorization` header, and
`OPENROUTER_API_KEY` is ignored for it, so a cloud key never travels over HTTP
to a local server. If the server uses authentication, give the provider an
`api_key_file` holding its key.

Check the host connection and start Gears from the desired guest workspace:

    /system/bin/curl --proto =http http://192.168.4.1:8080/health
    /devtools/bin/gears ask "Reply with a short hello."
    /devtools/bin/gears

Use `--ui line` for the line interface or `-vv` for transport diagnostics.
Gears appends `/chat/completions` to the configured API root. Match the Gears
context settings to the server's context capacity.

## Build and test

Gears supports Motor OS only. Copy this repository into a writable directory such as
`/devtools/src/gears` on a developer image. Provision dependencies, then build
and test entirely inside Motor OS:

    cd /devtools/src/gears
    lorry vendor --accept-all
    rush tests/full-test.sh --release

The gate builds gears and the existing smoke examples, runs unit and
integration tests plus mock-provider tests, then exercises the built-in tools
with the local compiler. Omit `--release` for debug builds. The component gate
is `tests/test-gears-http.sh`, invoked with rush.

All automated provider traffic stays on local loopback. Tests do not boot VMs,
use SSH, or require a Motor OS checkout. Dependency provisioning may access
the network; automated tests never contact a real Internet provider.

Native process-tree cleanup, file access preservation, line UI and session
behavior are covered by the local suites. TLS fixtures are in `tests/tls`.
Search tests use the installed `rg`.

For a manual mock-provider session, build and run the example locally on the
same OS as Gears. A real-provider check is separate and must be authorized.
