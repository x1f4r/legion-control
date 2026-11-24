//! Output events as text, without breaking characters.
//!
//! A program's output arrives in reads of whatever size the pipe hands over, which often ends in
//! the middle of a multi-byte character. Decoding each read on its own would turn both halves into
//! replacement characters, so the incomplete tail is held back until the next read completes it.

/// Decodes one stream's chunks into text.
///
/// Fed a stream in any number of pieces and then finished, it returns exactly what
/// `String::from_utf8_lossy` returns for the whole stream, which is how the captured output is
/// decoded, so the events and the result agree.
#[derive(Debug, Default)]
pub(super) struct Decoder {
    pending: Vec<u8>,
}

impl Decoder {
    /// The text `bytes` complete, holding back a character cut off at the end. Bytes that are not
    /// UTF-8 become U+FFFD.
    pub(super) fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    rest = &[];
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    text.push_str(&String::from_utf8_lossy(valid));
                    match error.error_len() {
                        // The input ends part way through a character the next read may finish.
                        None => {
                            rest = after;
                            break;
                        }
                        Some(invalid) => {
                            text.push(char::REPLACEMENT_CHARACTER);
                            rest = &after[invalid..];
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        text
    }

    /// What is still held back once the stream has ended: a character it never finished, as
    /// U+FFFD.
    pub(super) fn finish(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
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

    #[test]
    fn a_character_the_stream_never_finishes_is_flushed_at_the_end() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.push(b"ok\xc3"), "ok");
        assert_eq!(decoder.finish(), "\u{fffd}");
        assert_eq!(decoder.finish(), "");
    }

    /// Every way of cutting a stream of awkward bytes decodes as the whole stream does.
    #[test]
    fn any_split_decodes_as_the_whole_stream_does() {
        let bytes: &[u8] = b"a\xc3\xa9\xc0\x80\xe0\x80\x80\xed\xa0\x80\xf0\x9f\x98\x80\xf4\x90\x80\x80\xf5\xff\xe2\x82";
        let whole = String::from_utf8_lossy(bytes).into_owned();
        for first in 0..=bytes.len() {
            for second in first..=bytes.len() {
                let mut decoder = Decoder::default();
                let mut text = decoder.push(&bytes[..first]);
                text.push_str(&decoder.push(&bytes[first..second]));
                text.push_str(&decoder.push(&bytes[second..]));
                text.push_str(&decoder.finish());
                assert_eq!(text, whole, "cut at {first} and {second}");
            }
        }
    }
}
