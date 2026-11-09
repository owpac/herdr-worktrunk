//! The server of fzf (`--listen`): the plugin sends actions to a running
//! fzf, for example a reload when a check in the background ends.

use std::env;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The longest path of a Unix socket: macOS accepts 104 bytes with the
/// final 0.
const MAX_SOCKET_PATH: usize = 100;
/// The time limit of a request to fzf.
const TIME_LIMIT: Duration = Duration::from_secs(2);

/// A path for the socket of fzf, in the temporary folder. fzf needs the
/// extension `.sock`. A long temporary folder gives a path in `/tmp`.
pub fn socket_path(name: &str) -> PathBuf {
    let file = format!("{name}-{}.sock", std::process::id());
    let path = env::temp_dir().join(&file);
    if path.as_os_str().len() <= MAX_SOCKET_PATH {
        path
    } else {
        Path::new("/tmp").join(file)
    }
}

/// The HTTP request that sends `action` to fzf.
fn request(action: &str) -> String {
    format!(
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{action}",
        action.len()
    )
}

/// Send `action` to the fzf that listens on `socket`. An error tells that
/// fzf does not listen yet, or that it refused the action.
pub fn post(socket: &Path, action: &str) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(TIME_LIMIT))?;
    stream.set_write_timeout(Some(TIME_LIMIT))?;
    stream.write_all(request(action).as_bytes())?;
    let mut reply = [0_u8; 64];
    let count = stream.read(&mut reply)?;
    let status = String::from_utf8_lossy(&reply[..count]);
    if status.starts_with("HTTP/1.1 200") || status.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        Err(io::Error::other(
            status.lines().next().unwrap_or("no reply").to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_path_ends_with_sock_and_fits_the_limit() {
        let path = socket_path("herdr-worktrunk-cleaner");

        assert!(path.to_string_lossy().ends_with(".sock"));
        assert!(path.as_os_str().len() <= MAX_SOCKET_PATH);
    }

    #[test]
    fn the_request_gives_the_length_of_the_action() {
        let text = request("reload(echo é)");

        assert!(text.starts_with("POST / HTTP/1.1\r\n"));
        assert!(text.contains("Content-Length: 15\r\n"));
        assert!(text.ends_with("\r\n\r\nreload(echo é)"));
    }
}
