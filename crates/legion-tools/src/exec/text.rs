//! Output events as text, without breaking characters.
//!
//! A program's output arrives in reads of whatever size the pipe hands over, which often ends in
//! the middle of a multi-byte character. Decoding each read on its own would turn both halves into
//! replacement characters, so the incomplete tail is held back until the next read completes it.

/// Decodes one stream's chunks into text.
#[derive(Debug, Default)]
pub(super) struct Decoder {
    pending: Vec<u8>,
}

impl Decoder {
    /// The text `bytes` complete, holding back a character cut off at the end. Bytes that are not
    /// UTF-8 become U+FFFD, as they do in the captured output.
    pub(super) fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let keep = incomplete_tail(&self.pending);
        let complete = self.pending.len() - keep;
        let text = String::from_utf8_lossy(&self.pending[..complete]).into_owned();
        self.pending.drain(..complete);
        text
    }
}

/// How many bytes at the end of `bytes` begin a character that is not finished yet.
fn incomplete_tail(bytes: &[u8]) -> usize {
    // A UTF-8 character is at most four bytes, so only the last three can be an unfinished start.
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        let needed = match byte {
            0b1100_0000..=0b1101_1111 => 2,
            0b1110_0000..=0b1110_1111 => 3,
            0b1111_0000..=0b1111_0111 => 4,
            _ => return 0,
        };
        return if needed > back { back } else { 0 };
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_split_between_reads_is_decoded_whole() {
        let bytes = "aé€😀b".as_bytes();
        for split in 0..=bytes.len() {
            let mut decoder = Decoder::default();
            let mut text = decoder.push(&bytes[..split]);
            text.push_str(&decoder.push(&bytes[split..]));
            assert_eq!(text, "aé€😀b", "split at {split}");
        }
    }

    #[test]
    fn invalid_bytes_become_replacement_characters_without_stalling() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.push(&[b'o', 0xff, b'k']), "o\u{fffd}k");
        assert_eq!(
            decoder.push(&[0x80, 0x80, 0x80, 0x80]),
            "\u{fffd}".repeat(4)
        );
        assert_eq!(decoder.push(b"x"), "x");
    }
}
