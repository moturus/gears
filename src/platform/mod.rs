mod motor;
pub use motor::{
    TerminalInput, cancellation_text, cleanup_after_exit, install_interrupt_handler, kill_tree,
    process_alive, process_is_descendant_of, process_is_gears, raw_console, spawn, status_text,
};

use std::sync::atomic::{AtomicBool, Ordering};

fn debug_name_may_be_gears(name: &str, limit: usize) -> bool {
    // Native names contain the path plus unquoted arguments. A truncated or
    // unreadable name cannot prove that a live lock belongs to another program.
    if name.is_empty() || name.ends_with('…') || name.len() >= limit {
        return true;
    }
    // Any space can end the executable path, including one containing spaces.
    name.match_indices(' ')
        .map(|(end, _)| end)
        .chain(std::iter::once(name.len()))
        .any(|end| {
            std::path::Path::new(&name[..end])
                .file_name()
                .is_some_and(|file| file == "gears")
        })
}

#[cfg(test)]
#[test]
fn native_command_names_preserve_gears_and_unknown_live_owners() {
    for name in [
        "gears",
        "/devtools/bin/gears",
        "/devtools/bin/gears --ui line",
        "/user/my tools/gears -p hi",
        "/user/long-executable-path/gea…",
        "",
        "/12345678901234567890123456789012",
    ] {
        assert!(debug_name_may_be_gears(name, 32), "{name}");
    }
    for name in [
        "/system/bin/sysbox sleep 30",
        "/system/bin/rush -c hi",
        "/devtools/bin/gears-other",
        "not-gears",
    ] {
        assert!(!debug_name_may_be_gears(name, 32), "{name}");
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Record an interrupt. Async-signal-safe: an atomic store only.
pub fn note_interrupt() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub fn interrupt_pending() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Take the pending interrupt, clearing it; true if one was pending.
pub fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

// Nothing that sets or takes this flag can be tested beside the agent tests:
// they take it too, at every safe point in a turn, and whichever got there
// first made the other fail.
