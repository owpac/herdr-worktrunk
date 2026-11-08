//! Run a command without a terminal, with a time limit. The thread of the
//! merged pull requests uses it for git and gh: a network that does not
//! answer must not stop this thread.

use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The time between 2 checks of the end of the command.
const WAIT_STEP: Duration = Duration::from_millis(50);

/// Run `command` without a terminal, and return its output. If the command
/// does not end before `limit`, kill it and return an error.
pub fn output_with_limit(command: &mut Command, limit: Duration) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Read the pipes in 2 threads: a full pipe must not block the command.
    let stdout = read_in_thread(child.stdout.take());
    let stderr = read_in_thread(child.stderr.take());
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= limit {
            // A kill fails only if the command just ended.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no end after {} s", limit.as_secs()),
            ));
        }
        thread::sleep(WAIT_STEP);
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Read a pipe to its end in a new thread.
fn read_in_thread(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            // A read error gives the bytes read before it.
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_command_gives_its_output() {
        let output = output_with_limit(
            Command::new("sh").args(["-c", "echo out; echo err >&2"]),
            Duration::from_secs(5),
        )
        .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }

    #[test]
    fn a_command_that_does_not_end_is_killed() {
        let start = Instant::now();

        let result = output_with_limit(Command::new("sleep").arg("10"), Duration::from_millis(200));

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
