//! What the hub's commands print (decision D96). `nypi tasks`, `nypi goals`, and the others print
//! through `outln!`, `out!`, and `errln!`: to the terminal when they run on the hub, and into a
//! buffer when the hub runs one for a personal node, whose text then goes back over the link.
//! The buffer belongs to the task that runs the command, so the daemon's own log lines and
//! other commands running beside it never mix in.

use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex};

tokio::task_local! {
    static CAPTURE: Arc<Mutex<Vec<u8>>>;
}

/// Print `text` (with a newline when `line`) where this command's output goes: the buffer of a
/// command run for a node, or the terminal (stderr when `error`).
pub fn write(text: &str, line: bool, error: bool) {
    let captured = CAPTURE.try_with(|buffer| {
        let mut buffer = buffer.lock().unwrap();
        buffer.extend_from_slice(text.as_bytes());
        if line {
            buffer.push(b'\n');
        }
    });
    if captured.is_ok() {
        return;
    }
    let newline = if line { "\n" } else { "" };
    if error {
        let _ = write!(std::io::stderr(), "{text}{newline}");
    } else {
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "{text}{newline}");
        if !line {
            let _ = stdout.flush();
        }
    }
}

/// Whether this command runs for a node: nothing it prints reaches a terminal, and it must not
/// ask one anything.
pub fn capturing() -> bool {
    CAPTURE.try_with(|_| ()).is_ok()
}

/// Run `command`, keeping what it prints: its result and its text.
pub async fn capture<F: Future>(command: F) -> (F::Output, String) {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let result = CAPTURE.scope(buffer.clone(), command).await;
    let text = String::from_utf8_lossy(&buffer.lock().unwrap()).into_owned();
    (result, text)
}

/// A line of a command's output.
#[macro_export]
macro_rules! outln {
    () => { $crate::output::write("", true, false) };
    ($($arg:tt)*) => { $crate::output::write(&format!($($arg)*), true, false) };
}

/// Part of a line of a command's output.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => { $crate::output::write(&format!($($arg)*), false, false) };
}

/// A line of a command's errors.
#[macro_export]
macro_rules! errln {
    () => { $crate::output::write("", true, true) };
    ($($arg:tt)*) => { $crate::output::write(&format!($($arg)*), true, true) };
}

/// Part of a line of a command's errors.
#[macro_export]
macro_rules! errout {
    ($($arg:tt)*) => { $crate::output::write(&format!($($arg)*), false, true) };
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn a_command_run_for_a_node_keeps_its_own_text() {
        let ((), text) = super::capture(async {
            crate::outln!("one {}", 1);
            crate::out!("two");
            crate::errln!(" and an error");
        })
        .await;
        assert_eq!(text, "one 1\ntwo and an error\n");
        assert!(!super::capturing());
    }
}
