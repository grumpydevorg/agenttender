//! The attach client's detach escape: a two-key sequence recognized in the
//! user's keystrokes before they are forwarded to the session.
//!
//! With the default escape, `Ctrl-\` followed by `d` detaches, `Ctrl-\` twice
//! forwards one literal `Ctrl-\`, and `Ctrl-\` followed by any other byte
//! forwards both bytes unchanged. Every other byte is forwarded as typed. A
//! prefix at the end of one read is held until the next read decides it.
//! `Enter` is not part of the sequence (it would submit an unfinished prompt),
//! and neither is newline-tilde-dot, which an outer `ssh -t` consumes.

/// `Ctrl-\`.
pub const PREFIX: u8 = 0x1c;
/// The key that detaches after [`PREFIX`].
pub const DETACH_KEY: u8 = b'd';

/// Which escape the client recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeMode {
    /// `Ctrl-\ d` detaches.
    CtrlBackslash,
    /// No escape: every byte is forwarded.
    Disabled,
}

/// What the keystrokes fed so far ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    /// Keep relaying.
    Continue,
    /// Detach now. Bytes after the detach key are discarded.
    Detach,
}

/// Incremental escape recognizer.
#[derive(Debug, Clone)]
pub struct EscapeParser {
    mode: EscapeMode,
    prefix_pending: bool,
}

impl EscapeParser {
    #[must_use]
    pub fn new(mode: EscapeMode) -> Self {
        Self {
            mode,
            prefix_pending: false,
        }
    }

    /// Feed one read's worth of keystrokes, appending the bytes to forward to
    /// `forward`.
    pub fn feed(&mut self, input: &[u8], forward: &mut Vec<u8>) -> Escape {
        if self.mode == EscapeMode::Disabled {
            forward.extend_from_slice(input);
            return Escape::Continue;
        }
        for &byte in input {
            if self.prefix_pending {
                self.prefix_pending = false;
                match byte {
                    DETACH_KEY => return Escape::Detach,
                    PREFIX => forward.push(PREFIX),
                    other => forward.extend_from_slice(&[PREFIX, other]),
                }
            } else if byte == PREFIX {
                self.prefix_pending = true;
            } else {
                forward.push(byte);
            }
        }
        Escape::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn run(mode: EscapeMode, chunks: &[&[u8]]) -> (Vec<u8>, Escape) {
        let mut parser = EscapeParser::new(mode);
        let mut forward = Vec::new();
        for chunk in chunks {
            if parser.feed(chunk, &mut forward) == Escape::Detach {
                return (forward, Escape::Detach);
            }
        }
        (forward, Escape::Continue)
    }

    #[test]
    fn prefix_then_d_detaches_and_forwards_what_came_before() {
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"ls\r\x1cd"]),
            (b"ls\r".to_vec(), Escape::Detach)
        );
    }

    #[test]
    fn bytes_after_the_detach_key_are_discarded() {
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"a\x1cdrm -rf\r"]),
            (b"a".to_vec(), Escape::Detach)
        );
    }

    #[test]
    fn a_doubled_prefix_forwards_one_literal_prefix() {
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"A\x1c\x1cB"]),
            (b"A\x1cB".to_vec(), Escape::Continue)
        );
    }

    #[test]
    fn prefix_then_another_key_forwards_both_unchanged() {
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"\x1cx\x1c\r"]),
            (b"\x1cx\x1c\r".to_vec(), Escape::Continue)
        );
    }

    #[test]
    fn ctrl_right_bracket_passes_through() {
        // Alone, Ctrl-] is an ordinary byte: nothing about it is special to
        // the parser.
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"\x1d"]),
            (b"\x1d".to_vec(), Escape::Continue)
        );

        // As the byte that resolves a pending prefix, it is just an "other"
        // key: both bytes forward unchanged.
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"\x1c\x1d"]),
            (b"\x1c\x1d".to_vec(), Escape::Continue)
        );

        // Right after that resolution, a further Ctrl-] still passes
        // through untouched — no state leaks from the resolved prefix.
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"\x1c\x1d\x1d"]),
            (b"\x1c\x1d\x1d".to_vec(), Escape::Continue)
        );
    }

    #[test]
    fn a_prefix_split_across_reads_is_held_then_decided() {
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"abc\x1c", b"d"]),
            (b"abc".to_vec(), Escape::Detach)
        );
        assert_eq!(
            run(EscapeMode::CtrlBackslash, &[b"abc\x1c", b"\x1c", b"q"]),
            (b"abc\x1cq".to_vec(), Escape::Continue)
        );
        let mut parser = EscapeParser::new(EscapeMode::CtrlBackslash);
        let mut forward = Vec::new();
        assert_eq!(parser.feed(b"z\x1c", &mut forward), Escape::Continue);
        assert_eq!(
            forward, b"z",
            "a trailing prefix is held, not forwarded yet"
        );
    }

    #[test]
    fn disabled_escape_forwards_everything() {
        assert_eq!(
            run(EscapeMode::Disabled, &[b"\x1cd\x1c\x1c"]),
            (b"\x1cd\x1c\x1c".to_vec(), Escape::Continue)
        );
    }

    proptest! {
        #[test]
        fn keystrokes_without_the_prefix_pass_through_unchanged(
            chunks in proptest::collection::vec(
                proptest::collection::vec(any::<u8>().prop_filter("not the prefix", |b| *b != PREFIX), 0..64),
                0..16,
            )
        ) {
            let slices: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
            let (forward, escape) = run(EscapeMode::CtrlBackslash, &slices);
            prop_assert_eq!(escape, Escape::Continue);
            prop_assert_eq!(forward, chunks.concat());
        }

        #[test]
        fn chunking_never_changes_the_result(
            bytes in proptest::collection::vec(prop_oneof![Just(PREFIX), Just(DETACH_KEY), any::<u8>()], 0..64),
            split in any::<proptest::sample::Index>(),
        ) {
            let at = split.index(bytes.len() + 1);
            let whole = run(EscapeMode::CtrlBackslash, &[&bytes]);
            let parts = run(EscapeMode::CtrlBackslash, &[&bytes[..at], &bytes[at..]]);
            prop_assert_eq!(whole, parts);
        }
    }
}
