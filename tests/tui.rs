#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use gears::mock::{MockServer, sse_response};

struct Terminal {
    child: Child,
    master: File,
    output: Vec<u8>,
    root: PathBuf,
}

impl Terminal {
    fn start(server: &MockServer, tmux: bool) -> Self {
        let root = std::env::temp_dir().join(format!("gears-tui-{tmux}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("gears.toml");
        std::fs::write(
            &config,
            format!(
                "version = 1\n[net]\negress_allowlist = ['127.0.0.1']\n\
                 plain_http_allowlist = ['127.0.0.1']\n[provider]\n\
                 base_url = '{}'\nmodel = 'test/model'\n",
                server.url("/v1"),
            ),
        )
        .unwrap();
        let size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes both descriptors; the other pointers are
        // either null or point to a valid window size for the duration of the call.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        // SAFETY: each newly opened descriptor is owned exactly once.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        let mut command = if tmux {
            let tmux_config = root.join("tmux.conf");
            std::fs::write(
                &tmux_config,
                "set -s extended-keys on\nset -as terminal-features 'xterm*:extkeys'\n\
                 set -sq extended-keys-format csi-u\n",
            )
            .unwrap();
            let mut command = Command::new("tmux");
            command
                .arg("-S")
                .arg(root.join("tmux.sock"))
                .arg("-f")
                .arg(tmux_config)
                .args(["new-session", "--", env!("CARGO_BIN_EXE_gears")]);
            command
        } else {
            Command::new(env!("CARGO_BIN_EXE_gears"))
        };
        command
            .args(["--ui", "tui", "--ephemeral", "--config"])
            .arg(config)
            .arg("--workspace")
            .arg(&root)
            .current_dir(&root)
            .env(gears::directory::ENV, root.join(".gears"))
            .env_remove("GEARS_LOCK_OWNER")
            .env("OPENROUTER_API_KEY", "test-key")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env(
                "TERM",
                if tmux {
                    "xterm-256color"
                } else {
                    "xterm-kitty"
                },
            )
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: the child only calls async-signal-safe functions before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            child: command.spawn().unwrap(),
            master,
            output: Vec::new(),
            root,
        }
    }

    fn wait_for(&mut self, marker: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self
            .output
            .windows(marker.len())
            .any(|bytes| bytes == marker)
        {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "terminal output: {:?}",
                String::from_utf8_lossy(&self.output)
            );
            let mut fd = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: fd is a valid pollfd for the duration of the call.
            let ready = unsafe { libc::poll(&mut fd, 1, left.as_millis().max(1) as i32) };
            if ready < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            assert!(ready >= 0);
            if ready == 0 {
                continue;
            }
            let mut buffer = [0; 4096];
            let count = self.master.read(&mut buffer).unwrap_or_else(|error| {
                panic!(
                    "terminal closed before {marker:?}: {error}; output: {:?}",
                    String::from_utf8_lossy(&self.output)
                )
            });
            assert_ne!(count, 0, "terminal closed before marker");
            self.output.extend_from_slice(&buffer[..count]);
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let socket = self.root.join("tmux.sock");
        if socket.exists() {
            let _ = Command::new("tmux")
                .arg("-S")
                .arg(socket)
                .arg("kill-server")
                .output();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn newline_keys_reach_the_model_as_one_multiline_prompt() {
    check_newline_keys(false);
}

#[test]
fn newline_keys_survive_tmux_extended_key_reporting() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is not installed; skipping its terminal integration test");
        return;
    }
    check_newline_keys(true);
}

fn check_newline_keys(tmux: bool) {
    let server = MockServer::start_one(sse_response(&[
        "{\"choices\":[{\"delta\":{\"content\":\"newline-test-reply\"}}]}",
    ]))
    .unwrap();
    let mut terminal = Terminal::start(&server, tmux);
    if tmux {
        // Act as an outer terminal only once tmux has requested extended keys.
        terminal.wait_for(b"\x1b[>4;1m");
        terminal.wait_for(b"test/model");
    } else {
        terminal.wait_for(b"\x1b[>1u");
        terminal.wait_for(b"\x1b[?25h");
    }
    terminal.output.clear();
    // Ctrl+J arrives as LF or CSI-u; modified Enter uses CSI-u too. A
    // backslash before Enter needs no key the terminal might not report.
    terminal
        .master
        .write_all(
            b"one\nkitty\x1b[106;5utwo\x1b[13;2uthree\x1b[13;5ufour\x1b[13;6ufive\x1b\rsix\\\rseven\r",
        )
        .unwrap();
    terminal.wait_for(b"newline-test-reply");
    terminal.wait_for(b"idle");
    assert!(
        !String::from_utf8_lossy(&terminal.output).contains("error>"),
        "{:?}",
        String::from_utf8_lossy(&terminal.output)
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let prompt = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap();
    assert_eq!(
        prompt["content"],
        "one\nkitty\ntwo\nthree\nfour\nfive\nsix\nseven"
    );
    terminal.master.write_all(b"/quit\r").unwrap();
    terminal.wait_for(if tmux { b"\x1b[>4m" } else { b"\x1b[<1u" });
    terminal.wait_for(b"exited.");
    assert!(terminal.child.wait().unwrap().success());
}
