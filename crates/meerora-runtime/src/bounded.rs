//! Reading answers of servers meerora does not control with limits, so a
//! hostile or broken server ends a run with a readable error instead of
//! growing the daemon's memory.

use std::io::{self, BufRead, Read};

/// One line of `reader` without its line break (and a `\r` before it), or
/// None at the end. A line longer than `max` bytes is an
/// [`io::ErrorKind::InvalidData`] error, and at most `max` bytes of it are
/// ever held. Invalid UTF-8 is replaced.
pub(crate) fn read_line(reader: &mut impl BufRead, max: usize, buf: &mut Vec<u8>) -> io::Result<Option<String>> {
    buf.clear();
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok((!buf.is_empty()).then(|| text(buf)));
        }
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (i, true),
            None => (available.len(), false),
        };
        if buf.len() + take > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("a line is longer than {}", size(max))));
        }
        buf.extend_from_slice(&available[..take]);
        reader.consume(if done { take + 1 } else { take });
        if done {
            return Ok(Some(text(buf)));
        }
    }
}

fn text(buf: &[u8]) -> String {
    let line = buf.strip_suffix(b"\r").unwrap_or(buf);
    String::from_utf8_lossy(line).into_owned()
}

/// At most `max` bytes of `reader` as text, for error bodies that are only
/// shown in part anyway.
pub(crate) fn read_prefix(reader: impl Read, max: u64) -> String {
    let mut bytes = Vec::new();
    let _ = reader.take(max).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `4 MiB`, `64 KiB` or `100 bytes`, for messages.
pub(crate) fn size(bytes: usize) -> String {
    match bytes {
        b if b >= 1024 * 1024 && b % (1024 * 1024) == 0 => format!("{} MiB", b / 1024 / 1024),
        b if b >= 1024 && b % 1024 == 0 => format!("{} KiB", b / 1024),
        b => format!("{b} bytes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_have_a_limit() {
        let mut buf = Vec::new();
        let mut reader = io::BufReader::with_capacity(4, &b"one\r\ntwo\nthree"[..]);
        assert_eq!(read_line(&mut reader, 10, &mut buf).unwrap().as_deref(), Some("one"));
        assert_eq!(read_line(&mut reader, 10, &mut buf).unwrap().as_deref(), Some("two"));
        assert_eq!(read_line(&mut reader, 10, &mut buf).unwrap().as_deref(), Some("three"));
        assert_eq!(read_line(&mut reader, 10, &mut buf).unwrap(), None);

        let long = [b'x'; 100];
        let mut reader = io::BufReader::with_capacity(8, &long[..]);
        let err = read_line(&mut reader, 64, &mut buf).unwrap_err();
        assert!(err.to_string().contains("longer than 64 bytes"), "{err}");
        assert!(buf.len() <= 64, "never more than the limit is held");
        assert_eq!(read_prefix(&long[..], 10).len(), 10);
        assert_eq!(size(4 * 1024 * 1024), "4 MiB");
    }
}
