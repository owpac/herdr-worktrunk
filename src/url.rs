//! The parts of the URLs of the forges: the encoding of a value, its
//! decoding, and the name of a host.

/// Encode a value for a path segment or a query of a URL: each byte
/// outside the unreserved characters of RFC 3986 becomes `%XX`. The braces
/// get encoded too: tea replaces `{owner}`, `{repo}` and `{branch}`.
pub fn encode_part(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Decode the `%XX` of an URL. A `%` without 2 hexadecimal digits stays.
pub fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[index], hex) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                index += 3;
            }
            (byte, _) => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// The name of a host, without its port and in lowercase.
pub fn host_name(host: &str) -> String {
    host.split(':').next().unwrap_or(host).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_part_keeps_only_the_unreserved_characters() {
        assert_eq!(encode_part("group/sub/app"), "group%2Fsub%2Fapp");
        assert_eq!(encode_part("feat/forge-layers"), "feat%2Fforge-layers");
        assert_eq!(encode_part("v1.2_x~y"), "v1.2_x~y");
        assert_eq!(encode_part("a+b c&d=e#f"), "a%2Bb%20c%26d%3De%23f");
        assert_eq!(encode_part("é"), "%C3%A9");
        assert_eq!(encode_part("feat/{branch}"), "feat%2F%7Bbranch%7D");
    }

    #[test]
    fn decode_keeps_an_invalid_sequence() {
        assert_eq!(decode("My%20Project"), "My Project");
        assert_eq!(decode("%C3%A9t%C3%A9"), "été");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("%+A"), "%+A");
    }

    #[test]
    fn host_name_drops_the_port_and_the_case() {
        assert_eq!(host_name("Dev.Azure.com:443"), "dev.azure.com");
        assert_eq!(host_name("codeberg.org"), "codeberg.org");
    }
}
