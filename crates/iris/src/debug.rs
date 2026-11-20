//! Debug-only control pipe for automated checks of the spike.
//!
//! Enabled with `IRIS_CONTROL=/path/to/fifo`. Each line is one command:
//!   key <keystroke>      e.g. `key tab`, `key space`, `key f`
//!   move <x> <y>         synthetic mouse move, window points (macOS)
//!   click <x> <y>        synthetic left click, window points (macOS)
//!   snap <file.png>      capture this app's own window (macOS)
//!   resize <w> <h>       resize the window (logical points)
//!   open <path>          open a file or folder
//!   state                print playback and UI state to stderr
//!   get <property>       print an mpv property
//!   mpv <command...>     run an mpv command
//!   quit

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use std::io::BufRead;

pub fn control_channel() -> Option<UnboundedReceiver<String>> {
    let path = std::env::var_os("IRIS_CONTROL")?;
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("iris-control".into())
        .spawn(move || {
            loop {
                // Opening a FIFO blocks until a writer connects; reopen after EOF.
                let Ok(file) = std::fs::File::open(&path) else {
                    return;
                };
                for line in std::io::BufReader::new(file).lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim().to_string();
                    if !line.is_empty() && tx.unbounded_send(line).is_err() {
                        return;
                    }
                }
            }
        })
        .ok()?;
    Some(rx)
}
