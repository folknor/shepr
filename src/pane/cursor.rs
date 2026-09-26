#[derive(Debug, Default)]
pub(crate) struct DecscusrTracker {
    state: DecscusrParseState,
    cursor_shape_overridden: bool,
}

#[derive(Debug, Default)]
enum DecscusrParseState {
    #[default]
    Ground,
    Escape,
    Csi {
        first_param: Option<u16>,
        collecting_first_param: bool,
        has_space_intermediate: bool,
    },
}

impl DecscusrTracker {
    pub(crate) fn observe(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if matches!(self.state, DecscusrParseState::Ground) {
                let Some(escape) = bytes.iter().position(|&byte| byte == 0x1b) else {
                    return;
                };
                bytes = &bytes[escape..];
            }
            self.observe_byte(bytes[0]);
            bytes = &bytes[1..];
        }
    }

    fn observe_byte(&mut self, byte: u8) {
        match &mut self.state {
            DecscusrParseState::Ground => {
                if byte == 0x1b {
                    self.state = DecscusrParseState::Escape;
                }
            }
            DecscusrParseState::Escape => {
                self.state = if byte == b'[' {
                    DecscusrParseState::Csi {
                        first_param: None,
                        collecting_first_param: true,
                        has_space_intermediate: false,
                    }
                } else if byte == 0x1b {
                    DecscusrParseState::Escape
                } else {
                    DecscusrParseState::Ground
                };
            }
            DecscusrParseState::Csi {
                first_param,
                collecting_first_param,
                has_space_intermediate,
            } => {
                if byte == 0x1b {
                    self.state = DecscusrParseState::Escape;
                } else if byte.is_ascii_digit() && *collecting_first_param {
                    let digit = u16::from(byte - b'0');
                    *first_param = Some(first_param.unwrap_or(0).saturating_mul(10) + digit);
                } else if byte == b';' || byte == b':' {
                    *collecting_first_param = false;
                } else if byte == b' ' {
                    *has_space_intermediate = true;
                    *collecting_first_param = false;
                } else if (0x40..=0x7e).contains(&byte) {
                    if byte == b'q' && *has_space_intermediate {
                        let param = first_param.unwrap_or(0);
                        if param <= 6 {
                            self.cursor_shape_overridden = param != 0;
                        }
                    }
                    self.state = DecscusrParseState::Ground;
                } else if !(0x20..=0x3f).contains(&byte) {
                    self.state = DecscusrParseState::Ground;
                }
            }
        }
    }

    pub(crate) fn cursor_shape_overridden(&self) -> bool {
        self.cursor_shape_overridden
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_decscusr_matches_bytewise(chunks: &[&[u8]]) {
        let mut optimized = DecscusrTracker::default();
        let mut bytewise = DecscusrTracker::default();
        for chunk in chunks {
            optimized.observe(chunk);
            for &byte in *chunk {
                bytewise.observe_byte(byte);
            }
            assert_eq!(
                optimized.cursor_shape_overridden(),
                bytewise.cursor_shape_overridden(),
                "chunks: {chunks:?}"
            );
            // Compare the complete parser state, including incomplete CSI parameters.
            assert_eq!(
                format!("{:?}", optimized.state),
                format!("{:?}", bytewise.state),
                "chunks: {chunks:?}"
            );
        }
    }

    #[test]
    fn decscusr_bulk_search_matches_bytewise_control_sequences_and_splits() {
        let cases: &[&[u8]] = &[
            b"",
            b"plain text\n\twithout escapes",
            b"text\x1b[1 qmore\x1b[0 qend",
            b"\x1b[ q\x1b[2 q\x1b[3 q\x1b[4 q\x1b[5 q\x1b[6 q\x1b[7 q",
            b"\x1b\x1b[1 q\x1b[2;9 q\x1b[0:4 q",
            b"\x1b[1q\x1b[? q\x1b[12$ q\x1b[1\x00 q\x1b[2\xff q",
            b"\x1b[12\x1b[5 q\x1b]0;title\x07\x1bPdata\x1b\\",
            b"\x1b[1 q\x1b",
            b"\x1b[1 q\x1b[",
            b"\x1b[1 q\x1b[0 ",
        ];
        for &bytes in cases {
            for split in 0..=bytes.len() {
                assert_decscusr_matches_bytewise(&[&bytes[..split], &[], &bytes[split..]]);
            }
            let chunks: Vec<_> = bytes.chunks(1).collect();
            assert_decscusr_matches_bytewise(&chunks);
        }
    }

    #[test]
    fn decscusr_bulk_search_matches_bytewise_all_byte_values() {
        let all_bytes: Vec<u8> = (0..=255).collect();
        for split in 0..=all_bytes.len() {
            assert_decscusr_matches_bytewise(&[&all_bytes[..split], &all_bytes[split..]]);
        }
        // Exercise every possible byte in ground, escape, and partial CSI states.
        let prefixes: &[&[u8]] = &[b"", b"\x1b", b"\x1b[", b"\x1b[2", b"\x1b[0 ", b"\x1b[2;"];
        for &prefix in prefixes {
            for byte in 0..=255u8 {
                let mut bytes = b"\x1b[1 q".to_vec();
                bytes.extend_from_slice(prefix);
                bytes.push(byte);
                bytes.extend_from_slice(b" qtext\x1b[0 q\x1b[6 q");
                for split in 0..=bytes.len() {
                    assert_decscusr_matches_bytewise(&[&bytes[..split], &bytes[split..]]);
                }
            }
        }
    }
}
