//! Run a command without a terminal, with a time limit. The thread of the
//! merged pull requests uses it for git and gh: a network that does not
//! answer must not stop this thread.

use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::herdr::Error;

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

/// Run the CLI of a forge without a terminal, and return its standard
/// output. `label` names the command in the errors, for example
/// `gh pr list`. A missing program gives "<program> is not installed".
pub fn run_cli(command: &mut Command, label: &str, limit: Duration) -> Result<String, Error> {
    let output = run_cli_output(command, label, limit)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Like `run_cli`, but return the whole output: the standard error can
/// carry information, for example the HTTP status of `tea api -i`.
pub fn run_cli_output(
    command: &mut Command,
    label: &str,
    limit: Duration,
) -> Result<Output, Error> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = output_with_limit(command, limit).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            Error::Command(format!("{program} is not installed"))
        } else {
            Error::Command(format!("{label}: {error}"))
        }
    })?;
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Command(format!("{label}: {}", details.trim())));
    }
    Ok(output)
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

#[cfg(test)]
mod run_cli_tests {
    use super::*;

    const LIMIT: Duration = Duration::from_secs(5);

    #[test]
    fn run_cli_gives_the_output_or_a_clear_error() {
        assert_eq!(
            run_cli(Command::new("echo").arg("hello"), "echo", LIMIT).unwrap(),
            "hello\n"
        );
        assert_eq!(
            run_cli(&mut Command::new("herdr-worktrunk-no-such-cli"), "x", LIMIT)
                .unwrap_err()
                .to_string(),
            "herdr-worktrunk-no-such-cli is not installed"
        );
        let failed = run_cli(
            Command::new("sh").args(["-c", "echo oops >&2; exit 1"]),
            "sh -c",
            LIMIT,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(failed, "sh -c: oops");
    }
}
