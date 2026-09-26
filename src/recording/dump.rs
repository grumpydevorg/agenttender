//! Plain-text dump of a decoded recording; the layout and the flagged
//! sequences are documented on [`dump`].

use std::fmt::{self, Write as _};
use std::io::{self, Write};

use super::{DecodedRecording, FORMAT_VERSION, RecordKind, ResizeCause, SegmentEnd, Sequence};

/// The most bytes of one escape sequence the scanner keeps for classification.
const MAX_SEQUENCE: usize = 256;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const DEL: u8 = 0x7f;

/// Write `recording` as plain text.
///
/// A development aid, run as `cargo run --example dump-recording -- <path>`.
/// It is not shipped in the `tendr` binary and its layout is not a stable
/// interface; the golden-fixture test pins it so changes are deliberate.
///
/// # Layout
///
/// Header lines (format, run, segments, origin, geometry, `TERM`, input
/// policy), then one line per record:
///
/// ```text
/// #<sequence> <elapsed>ms output len=<n> "<bytes>"
/// #<sequence> <elapsed>ms input len=<n> "<bytes>"
/// #<sequence> <elapsed>ms resize rows=<r> cols=<c> user
/// #<sequence> <elapsed>ms resize rows=<r> cols=<c> repaint correlation=<id>
/// ```
///
/// Elapsed time is milliseconds since the recording origin, truncated to the
/// microsecond. Bytes are printed as ASCII: `\\`, `\"`, `\n`, `\r` and `\t`
/// are escaped, other printable ASCII is literal, and every other byte
/// (controls, DEL, non-ASCII, invalid UTF-8) is `\xHH`.
///
/// After an output record, each flagged terminal sequence it completes gets an
/// indented `  ^ <label> "<sequence>"` line, with ` from #<n>` when the
/// sequence began in an earlier record. The dump ends with any escape sequence
/// still unfinished when the output stops, then how the final segment ended
/// (`end clean`, or where the incomplete final record was cut off).
///
/// # Flagged sequences
///
/// | Label | Bytes |
/// |---|---|
/// | `query:da1` | `ESC[c`, `ESC[0c` |
/// | `query:da2` | `ESC[>c`, `ESC[>0c` |
/// | `query:dsr-cpr` | `ESC[6n` |
/// | `query:dsr-status` | `ESC[5n` |
/// | `query:decrqm` | `ESC[?N$p`, `ESC[N$p` |
/// | `query:kitty-keyboard` | `ESC[?u` |
/// | `mode:kitty-keyboard-push` | `ESC[>u`, `ESC[>Nu` |
/// | `mode:kitty-keyboard-pop` | `ESC[<u`, `ESC[<Nu` |
/// | `mode:kitty-keyboard-set` | `ESC[=N;Mu` |
/// | `mode:modify-other-keys=N` | `ESC[>4;Nm`; `=reset` for `ESC[>4m` and `ESC[>m`, `=off` for `ESC[>4n`, no suffix for a level above 3 |
/// | `query:osc10`, `query:osc11` | `ESC]10;?`, `ESC]11;?` (also `ESC]10;?;?`), ended by BEL or `ESC\` |
/// | `query:xtversion` | `ESC[>q`, `ESC[>0q` |
/// | `query:kitty-graphics` | `ESC_G<control>;<payload>` with `a=q` among the comma-separated control keys (payload optional), ended by BEL or `ESC\` |
/// | `mode:alt-screen-on`/`-off` | `ESC[?1049h` / `ESC[?1049l`, alone or among other private modes |
/// | `mode:bracketed-paste-on`/`-off` | `ESC[?2004h` / `ESC[?2004l`, likewise |
///
/// # Detection across records
///
/// The child writes one continuous byte stream, but the recorder cuts it into
/// records wherever a read ended, so a query can begin in one output record and
/// finish in a later one. The scanner is therefore a single parser whose state
/// carries from one output record to the next, across segment boundaries and
/// across the resize and input records between them, which are not part of the
/// child's output. It remembers the record holding a sequence's `ESC`, and the
/// annotation follows the record holding its final byte.
///
/// Parsing follows the usual VT rules closely enough to avoid look-alikes:
/// `ESC` restarts a sequence, CAN and SUB cancel one, other C0 controls after
/// `ESC` or inside a CSI are ignored, and a CSI with intermediate bytes (such as
/// `ESC[ q`) matches only as DECRQM (`$p`). Only 7-bit introducers are
/// recognised; the 8-bit C1 forms (`0x9B`, `0x9D`, `0x9F`) are ambiguous in
/// UTF-8 output. At most 256 bytes of a sequence are kept: a longer one (an
/// OSC 52 clipboard write or a kitty graphics image transfer, say) is consumed
/// to its end but not classified.
///
/// # Errors
///
/// Errors from `out`.
pub fn dump(recording: &DecodedRecording, out: &mut impl Write) -> io::Result<()> {
    let header = &recording.header;
    writeln!(out, "format {FORMAT_VERSION}")?;
    writeln!(out, "run {}", header.run_id)?;
    writeln!(
        out,
        "segments {} (first index {}, first sequence {})",
        recording.segment_count,
        header.segment_index,
        header.first_sequence.get()
    )?;
    writeln!(out, "origin_unix_ns {}", header.origin_unix_ns)?;
    writeln!(
        out,
        "geometry rows={} cols={}",
        header.geometry.rows(),
        header.geometry.cols()
    )?;
    writeln!(out, "term {}", header.term.as_str())?;
    let input = if header.input_recorded {
        "recorded"
    } else {
        "not recorded"
    };
    writeln!(out, "input {input}")?;

    let mut scanner = Scanner::default();
    let mut found = Vec::new();
    for record in &recording.records {
        let sequence = record.sequence;
        write!(out, "#{} {}ms ", sequence.get(), Millis(record.elapsed_ns))?;
        match &record.kind {
            RecordKind::Output(bytes) => {
                writeln!(out, "output len={} \"{}\"", bytes.len(), Escaped(bytes))?;
                scanner.feed(sequence, bytes, &mut found);
            }
            RecordKind::Input(bytes) => {
                writeln!(out, "input len={} \"{}\"", bytes.len(), Escaped(bytes))?;
            }
            RecordKind::Resize { geometry, cause } => {
                write!(
                    out,
                    "resize rows={} cols={} ",
                    geometry.rows(),
                    geometry.cols()
                )?;
                match cause {
                    ResizeCause::User => writeln!(out, "user")?,
                    ResizeCause::Repaint { correlation } => {
                        writeln!(out, "repaint correlation={correlation}")?;
                    }
                }
            }
        }
        for finding in found.drain(..) {
            write!(
                out,
                "  ^ {} \"{}\"",
                finding.marker.label(),
                Escaped(&finding.bytes)
            )?;
            if finding.start != sequence {
                write!(out, " from #{}", finding.start.get())?;
            }
            writeln!(out)?;
        }
    }

    if let Some((start, bytes)) = scanner.unfinished() {
        let more = if scanner.overflow { "..." } else { "" };
        writeln!(
            out,
            "unfinished escape sequence from #{}: \"{}\"{more}",
            start.get(),
            Escaped(bytes)
        )?;
    }
    match recording.end {
        SegmentEnd::Clean => writeln!(out, "end clean"),
        SegmentEnd::Truncated { offset } => {
            let last_segment = u64::from(header.segment_index)
                + u64::from(recording.segment_count.saturating_sub(1));
            writeln!(
                out,
                "end truncated at segment {last_segment} offset {offset} \
                 (incomplete final record excluded)"
            )
        }
    }
}

/// Nanoseconds shown as milliseconds, truncated to the microsecond.
struct Millis(u64);

impl fmt::Display for Millis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{:03}",
            self.0 / 1_000_000,
            self.0 % 1_000_000 / 1_000
        )
    }
}

/// Bytes as printable ASCII; see the module docs.
struct Escaped<'a>(&'a [u8]);

impl fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &byte in self.0 {
            match byte {
                b'\\' => f.write_str("\\\\")?,
                b'"' => f.write_str("\\\"")?,
                b'\n' => f.write_str("\\n")?,
                b'\r' => f.write_str("\\r")?,
                b'\t' => f.write_str("\\t")?,
                0x20..=0x7e => f.write_char(char::from(byte))?,
                _ => write!(f, "\\x{byte:02x}")?,
            }
        }
        Ok(())
    }
}

/// A terminal query or mode change worth finding in a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Da1Query,
    Da2Query,
    CursorPositionQuery,
    StatusQuery,
    ModeQuery,
    KittyKeyboardQuery,
    KittyKeyboardPush,
    KittyKeyboardPop,
    KittyKeyboardSet,
    ForegroundColourQuery,
    BackgroundColourQuery,
    XtversionQuery,
    KittyGraphicsQuery,
    AltScreen(bool),
    BracketedPaste(bool),
    /// XTMODKEYS `modifyOtherKeys`: the level set, `None` for one above 3.
    ModifyOtherKeys(Option<u8>),
    ModifyOtherKeysReset,
    ModifyOtherKeysOff,
}

impl Marker {
    fn label(self) -> &'static str {
        match self {
            Self::Da1Query => "query:da1",
            Self::Da2Query => "query:da2",
            Self::CursorPositionQuery => "query:dsr-cpr",
            Self::StatusQuery => "query:dsr-status",
            Self::ModeQuery => "query:decrqm",
            Self::KittyKeyboardQuery => "query:kitty-keyboard",
            Self::KittyKeyboardPush => "mode:kitty-keyboard-push",
            Self::KittyKeyboardPop => "mode:kitty-keyboard-pop",
            Self::KittyKeyboardSet => "mode:kitty-keyboard-set",
            Self::ForegroundColourQuery => "query:osc10",
            Self::BackgroundColourQuery => "query:osc11",
            Self::XtversionQuery => "query:xtversion",
            Self::KittyGraphicsQuery => "query:kitty-graphics",
            Self::AltScreen(true) => "mode:alt-screen-on",
            Self::AltScreen(false) => "mode:alt-screen-off",
            Self::BracketedPaste(true) => "mode:bracketed-paste-on",
            Self::BracketedPaste(false) => "mode:bracketed-paste-off",
            Self::ModifyOtherKeys(Some(0)) => "mode:modify-other-keys=0",
            Self::ModifyOtherKeys(Some(1)) => "mode:modify-other-keys=1",
            Self::ModifyOtherKeys(Some(2)) => "mode:modify-other-keys=2",
            Self::ModifyOtherKeys(Some(3)) => "mode:modify-other-keys=3",
            Self::ModifyOtherKeys(_) => "mode:modify-other-keys",
            Self::ModifyOtherKeysReset => "mode:modify-other-keys=reset",
            Self::ModifyOtherKeysOff => "mode:modify-other-keys=off",
        }
    }
}

/// A flagged sequence: what it is, the record holding its `ESC`, and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    marker: Marker,
    start: Sequence,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    /// After `ESC`.
    Escape,
    /// After `ESC [`.
    Csi,
    /// Inside an OSC (after `ESC ]`) or an APC (after `ESC _`).
    Str(StrKind),
    /// After an `ESC` inside an OSC or APC, which is either the start of the
    /// `ESC \` terminator or a new sequence starting in the given record.
    StrEscape(StrKind, Sequence),
}

/// A control string ended by BEL or `ESC \`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrKind {
    Osc,
    Apc,
}

/// Finds [`Marker`]s in output bytes fed record by record; see the module docs.
#[derive(Debug, Default)]
struct Scanner {
    state: State,
    /// The record holding the current sequence's `ESC`.
    start: Option<Sequence>,
    /// The current sequence from its `ESC`, at most [`MAX_SEQUENCE`] bytes.
    bytes: Vec<u8>,
    /// The current sequence is longer than [`MAX_SEQUENCE`].
    overflow: bool,
}

impl Scanner {
    fn feed(&mut self, sequence: Sequence, bytes: &[u8], found: &mut Vec<Finding>) {
        for &byte in bytes {
            self.step(sequence, byte, found);
        }
    }

    /// The sequence in progress, if any, and the record it began in.
    fn unfinished(&self) -> Option<(Sequence, &[u8])> {
        match self.state {
            State::Ground => None,
            _ => self.start.map(|start| (start, self.bytes.as_slice())),
        }
    }

    fn step(&mut self, sequence: Sequence, byte: u8, found: &mut Vec<Finding>) {
        match self.state {
            State::Ground => {
                if byte == ESC {
                    self.begin(sequence);
                }
            }
            State::Escape => match byte {
                b'[' => {
                    self.push(byte);
                    self.state = State::Csi;
                }
                b']' => {
                    self.push(byte);
                    self.state = State::Str(StrKind::Osc);
                }
                b'_' => {
                    self.push(byte);
                    self.state = State::Str(StrKind::Apc);
                }
                ESC => self.begin(sequence),
                // Executed by the terminal without ending the sequence.
                0x00..=0x17 | 0x19 | 0x1c..=0x1f => {}
                _ => self.reset(),
            },
            State::Csi => match byte {
                ESC => self.begin(sequence),
                CAN | SUB => self.reset(),
                // Executed by the terminal without ending the sequence.
                0x00..=0x1f | DEL => {}
                0x20..=0x3f => self.push(byte),
                0x40..=0x7e => {
                    self.push(byte);
                    if !self.overflow {
                        csi_markers(&self.bytes[2..], |marker| self.found(marker, found));
                    }
                    self.reset();
                }
                _ => self.reset(),
            },
            State::Str(kind) => match byte {
                BEL => {
                    self.push(byte);
                    self.str_end(kind, 1, found);
                }
                ESC => {
                    self.push(byte);
                    self.state = State::StrEscape(kind, sequence);
                }
                CAN | SUB => self.reset(),
                _ => self.push(byte),
            },
            State::StrEscape(kind, escape_at) => {
                if byte == b'\\' {
                    self.push(byte);
                    self.str_end(kind, 2, found);
                } else {
                    self.begin(escape_at);
                    self.step(sequence, byte, found);
                }
            }
        }
    }

    /// Classify a finished OSC or APC whose terminator is the last
    /// `terminator` bytes.
    fn str_end(&mut self, kind: StrKind, terminator: usize, found: &mut Vec<Finding>) {
        if !self.overflow {
            let payload = &self.bytes[2..self.bytes.len() - terminator];
            match kind {
                StrKind::Osc => osc_markers(payload, |marker| self.found(marker, found)),
                StrKind::Apc => apc_markers(payload, |marker| self.found(marker, found)),
            }
        }
        self.reset();
    }

    fn found(&self, marker: Marker, found: &mut Vec<Finding>) {
        found.push(Finding {
            marker,
            start: self.start.expect("a sequence in progress has a start"),
            bytes: self.bytes.clone(),
        });
    }

    fn begin(&mut self, sequence: Sequence) {
        self.reset();
        self.state = State::Escape;
        self.start = Some(sequence);
        self.bytes.push(ESC);
    }

    fn push(&mut self, byte: u8) {
        if self.bytes.len() < MAX_SEQUENCE {
            self.bytes.push(byte);
        } else {
            self.overflow = true;
        }
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.start = None;
        self.bytes.clear();
        self.overflow = false;
    }
}

fn is_number(bytes: &[u8]) -> bool {
    bytes.iter().all(u8::is_ascii_digit)
}

/// Markers for a complete CSI, given the bytes after `ESC [`.
fn csi_markers(body: &[u8], mut found: impl FnMut(Marker)) {
    let Some((&final_byte, rest)) = body.split_last() else {
        return;
    };
    // Parameter bytes (0x30..=0x3F) come first, then intermediate bytes
    // (0x20..=0x2F), which make a different function.
    let split = rest
        .iter()
        .position(|b| (0x20..=0x2f).contains(b))
        .unwrap_or(rest.len());
    let (params, intermediates) = rest.split_at(split);
    if intermediates.iter().any(|b| !(0x20..=0x2f).contains(b)) {
        return;
    }
    if (final_byte, intermediates) == (b'p', b"$") {
        let mode = params.strip_prefix(b"?").unwrap_or(params);
        if !mode.is_empty() && is_number(mode) {
            found(Marker::ModeQuery);
        }
        return;
    }
    if !intermediates.is_empty() {
        return;
    }
    match (final_byte, params) {
        (b'c', b"" | b"0") => found(Marker::Da1Query),
        (b'c', b">" | b">0") => found(Marker::Da2Query),
        (b'n', b"6") => found(Marker::CursorPositionQuery),
        (b'n', b"5") => found(Marker::StatusQuery),
        (b'n', b">4") => found(Marker::ModifyOtherKeysOff),
        (b'm', b">" | b">4" | b">4;") => found(Marker::ModifyOtherKeysReset),
        (b'm', [b'>', b'4', b';', level @ ..]) if is_number(level) => {
            let level = std::str::from_utf8(level)
                .ok()
                .and_then(|l| l.parse::<u8>().ok())
                .filter(|&l| l <= 3);
            found(Marker::ModifyOtherKeys(level));
        }
        (b'u', b"?") => found(Marker::KittyKeyboardQuery),
        (b'u', [b'>', flags @ ..]) if is_number(flags) => found(Marker::KittyKeyboardPush),
        (b'u', [b'<', count @ ..]) if is_number(count) => found(Marker::KittyKeyboardPop),
        (b'u', [b'=', params @ ..])
            if !params.is_empty() && params.split(|&b| b == b';').all(is_number) =>
        {
            found(Marker::KittyKeyboardSet);
        }
        (b'q', b">" | b">0") => found(Marker::XtversionQuery),
        (b'h' | b'l', [b'?', modes @ ..]) => {
            let set = final_byte == b'h';
            for mode in modes.split(|&b| b == b';') {
                match mode {
                    b"1049" => found(Marker::AltScreen(set)),
                    b"2004" => found(Marker::BracketedPaste(set)),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Markers for a complete APC, given its payload between `ESC _` and the
/// terminator. A kitty graphics command is `G`, comma-separated `key=value`
/// control data, then optionally `;` and a payload; `a=q` makes it a query.
fn apc_markers(payload: &[u8], mut found: impl FnMut(Marker)) {
    let Some(command) = payload.strip_prefix(b"G") else {
        return;
    };
    let control = command.split(|&b| b == b';').next().unwrap_or_default();
    if control.split(|&b| b == b',').any(|kv| kv == b"a=q") {
        found(Marker::KittyGraphicsQuery);
    }
}

/// Markers for a complete OSC, given its payload between `ESC ]` and the
/// terminator. `OSC Ps ; ? ; ? …` queries colour `Ps`, then `Ps + 1`, and so on.
fn osc_markers(payload: &[u8], mut found: impl FnMut(Marker)) {
    let mut fields = payload.split(|&b| b == b';');
    let first: u32 = match fields.next() {
        Some(ps) if !ps.is_empty() && ps.len() <= 3 && is_number(ps) => ps
            .iter()
            .fold(0, |n, &digit| n * 10 + u32::from(digit - b'0')),
        _ => return,
    };
    for (colour, field) in (first..).zip(fields) {
        if field != b"?" {
            continue;
        }
        match colour {
            10 => found(Marker::ForegroundColourQuery),
            11 => found(Marker::BackgroundColourQuery),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(n: u64) -> Sequence {
        Sequence::new(n).unwrap()
    }

    /// Feed each chunk as its own record (sequences 1, 2, …) and return the
    /// labels found, with the record each one started in and completed in.
    fn scan(chunks: &[&[u8]]) -> Vec<(&'static str, u64, u64)> {
        let mut scanner = Scanner::default();
        let mut labels = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let sequence = seq(i as u64 + 1);
            let mut found = Vec::new();
            scanner.feed(sequence, chunk, &mut found);
            labels.extend(
                found
                    .into_iter()
                    .map(|f| (f.marker.label(), f.start.get(), sequence.get())),
            );
        }
        labels
    }

    fn labels(bytes: &[u8]) -> Vec<&'static str> {
        scan(&[bytes])
            .into_iter()
            .map(|(label, ..)| label)
            .collect()
    }

    const FLAGGED: &[(&[u8], &str)] = &[
        (b"\x1b[c", "query:da1"),
        (b"\x1b[0c", "query:da1"),
        (b"\x1b[6n", "query:dsr-cpr"),
        (b"\x1b[?u", "query:kitty-keyboard"),
        (b"\x1b[>1u", "mode:kitty-keyboard-push"),
        (b"\x1b[>u", "mode:kitty-keyboard-push"),
        (b"\x1b[<u", "mode:kitty-keyboard-pop"),
        (b"\x1b[<2u", "mode:kitty-keyboard-pop"),
        (b"\x1b[=1;1u", "mode:kitty-keyboard-set"),
        (b"\x1b]10;?\x07", "query:osc10"),
        (b"\x1b]11;?\x1b\\", "query:osc11"),
        (b"\x1b[>0q", "query:xtversion"),
        (b"\x1b[>q", "query:xtversion"),
        (b"\x1b[?1049h", "mode:alt-screen-on"),
        (b"\x1b[?1049l", "mode:alt-screen-off"),
        (b"\x1b[?2004h", "mode:bracketed-paste-on"),
        (b"\x1b[?2004l", "mode:bracketed-paste-off"),
        (b"\x1b[>c", "query:da2"),
        (b"\x1b[>0c", "query:da2"),
        (b"\x1b[5n", "query:dsr-status"),
        (b"\x1b[?1049$p", "query:decrqm"),
        (b"\x1b[?2026$p", "query:decrqm"),
        (b"\x1b[4$p", "query:decrqm"),
        (b"\x1b[>4;0m", "mode:modify-other-keys=0"),
        (b"\x1b[>4;1m", "mode:modify-other-keys=1"),
        (b"\x1b[>4;2m", "mode:modify-other-keys=2"),
        (b"\x1b[>4;3m", "mode:modify-other-keys=3"),
        (b"\x1b[>4;7m", "mode:modify-other-keys"),
        (b"\x1b[>4m", "mode:modify-other-keys=reset"),
        (b"\x1b[>4;m", "mode:modify-other-keys=reset"),
        (b"\x1b[>m", "mode:modify-other-keys=reset"),
        (b"\x1b[>4n", "mode:modify-other-keys=off"),
        // Claude Code 2.1.283's probe.
        (
            b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\",
            "query:kitty-graphics",
        ),
        (b"\x1b_Ga=q,i=1;AAAA\x07", "query:kitty-graphics"),
        (b"\x1b_Ga=q\x1b\\", "query:kitty-graphics"),
    ];

    #[test]
    fn each_query_and_mode_change_is_flagged() {
        for (bytes, label) in FLAGGED {
            assert_eq!(labels(bytes), [*label], "{bytes:?}");
        }
    }

    #[test]
    fn a_flagged_sequence_split_at_any_byte_is_flagged_once_from_its_first_record() {
        for (bytes, label) in FLAGGED {
            for cut in 1..bytes.len() {
                let (head, tail) = bytes.split_at(cut);
                assert_eq!(scan(&[head, tail]), [(*label, 1, 2)], "{bytes:?} cut {cut}");
            }
        }
    }

    #[test]
    fn a_sequence_split_across_three_records_names_the_first() {
        assert_eq!(
            scan(&[b"text\x1b", b"[?10", b"49h more"]),
            [("mode:alt-screen-on", 1, 3)]
        );
    }

    #[test]
    fn similar_bytes_are_not_flagged() {
        let lookalikes: &[&[u8]] = &[
            b"[c [6n [?u ]11;? plain text",
            b"\x1b[>1;10;0c",                  // DA2 reply
            b"\x1b[>1c",                       // not a DA2 request
            b"\x1b[=c",                        // DA3
            b"\x1b[1c",                        // not a DA1 request
            b"\x1b[?6n",                       // DECXCPR
            b"\x1b[?5n",                       // DEC private, not DSR 5
            b"\x1b[0n",                        // "terminal OK" reply
            b"\x1b[16n",                       // not 6
            b"\x1b[u",                         // restore cursor (SCORC)
            b"\x1b[?1u",                       // not the bare query
            b"\x1b[>1;2u",                     // push takes one parameter
            b"\x1b[0q",                        // DECLL
            b"\x1b[ q",                        // DECSCUSR
            b"\x1b[>1q",                       // not XTVERSION
            b"\x1b[1049h",                     // ANSI mode, not DEC private
            b"\x1b[?104h",                     // other private mode
            b"\x1b[?10490h",                   // other private mode
            b"\x1b[?2004r",                    // restore, not set
            b"\x1b[?1049$y",                   // DECRPM reply
            b"\x1b[?$p",                       // DECRQM without a mode
            b"\x1b[?1049;1$p",                 // DECRQM takes one mode
            b"\x1b[$1p",                       // parameter after intermediate
            b"\x1b[!p",                        // DECSTR
            b"\x1b[1\"p",                      // DECSCL
            b"\x1b[4m",                        // underline
            b"\x1b[?4m",                       // not XTMODKEYS
            b"\x1b[>1;2m",                     // modifyCursorKeys
            b"\x1b[>14;2m",                    // other resource
            b"\x1b[>4;2;1m",                   // too many parameters
            b"\x1b[>4;2 m",                    // intermediate byte
            b"\x1b[>1n",                       // disables another resource
            b"\x1b]11;rgb:0000/0000/0000\x07", // sets the colour
            b"\x1b]110\x07",                   // resets it
            b"\x1b]12;?\x07",                  // cursor colour
            b"\x1b]1;?\x07",                   // icon name
            b"\x1b[\x18c",                     // CAN cancels the sequence
            b"\x1b[\x1a6n",                    // SUB cancels the sequence
            b"\x1bP>|c\x1b\\",                 // DCS, not CSI
            b"\x1b_Gi=31,a=T,f=24;AAAA\x1b\\", // kitty graphics transmit and display
            b"\x1b_Gi=31;a=q\x1b\\",           // `a=q` in the payload, not the keys
            b"\x1b_Gaa=q\x1b\\",               // another key
            b"\x1b_Ga=qq\x1b\\",               // another value
            b"\x1b_Xa=q\x1b\\",                // not a graphics command
            b"\x1b_Ga=q\x18\x1b\\",            // CAN cancels the sequence
        ];
        for bytes in lookalikes {
            assert_eq!(labels(bytes), Vec::<&str>::new(), "{bytes:?}");
        }
    }

    #[test]
    fn c0_controls_after_escape_do_not_end_it() {
        assert_eq!(labels(b"\x1b\r[6n"), ["query:dsr-cpr"]);
        assert_eq!(labels(b"\x1b\x18[6n"), Vec::<&str>::new());
    }

    #[test]
    fn escape_restarts_an_unfinished_sequence() {
        assert_eq!(labels(b"\x1b[?10\x1b[c"), ["query:da1"]);
        assert_eq!(labels(b"\x1b]11;\x1b[6n"), ["query:dsr-cpr"]);
        assert_eq!(labels(b"\x1b\x1b[6n"), ["query:dsr-cpr"]);
    }

    #[test]
    fn an_escape_ending_an_osc_record_can_start_the_next_sequence() {
        assert_eq!(
            scan(&[b"\x1b]0;title\x1b", b"[6n"]),
            [("query:dsr-cpr", 1, 2)]
        );
    }

    #[test]
    fn a_kitty_graphics_query_split_across_records_is_flagged_from_its_first() {
        assert_eq!(
            scan(&[
                b"x\x1b_Gi=31,s=1,v=1,",
                b"a=q,t=d,f=24;AA",
                b"AA\x1b",
                b"\\y"
            ]),
            [("query:kitty-graphics", 1, 4)]
        );
    }

    #[test]
    fn an_escape_inside_an_apc_starts_a_new_sequence() {
        assert_eq!(labels(b"\x1b_Ga=q;AA\x1b[6n"), ["query:dsr-cpr"]);
        assert_eq!(scan(&[b"\x1b_Ga=T;AA\x1b", b"[c"]), [("query:da1", 1, 2)]);
    }

    #[test]
    fn one_private_mode_sequence_can_carry_several_changes() {
        assert_eq!(
            labels(b"\x1b[?1000;1049;2004l"),
            ["mode:alt-screen-off", "mode:bracketed-paste-off"]
        );
    }

    #[test]
    fn one_osc_can_query_both_colours() {
        assert_eq!(labels(b"\x1b]10;?;?\x07"), ["query:osc10", "query:osc11"]);
    }

    #[test]
    fn a_long_sequence_is_consumed_without_hiding_what_follows() {
        let mut bytes = b"\x1b]52;c;".to_vec();
        bytes.extend(std::iter::repeat_n(b'A', 100_000));
        bytes.extend(b"\x07\x1b[c");
        assert_eq!(labels(&bytes), ["query:da1"]);

        let mut csi = b"\x1b[".to_vec();
        csi.extend(std::iter::repeat_n(b'1', 10_000));
        csi.extend(b"n\x1b[6n");
        assert_eq!(labels(&csi), ["query:dsr-cpr"]);
    }

    #[test]
    fn c0_controls_inside_a_csi_do_not_end_it() {
        assert_eq!(labels(b"\x1b[\r6n"), ["query:dsr-cpr"]);
    }

    #[test]
    fn an_unfinished_sequence_is_reported_with_its_first_record() {
        let mut scanner = Scanner::default();
        let mut found = Vec::new();
        scanner.feed(seq(1), b"ok", &mut found);
        assert_eq!(scanner.unfinished(), None);
        scanner.feed(seq(2), b"\x1b[?20", &mut found);
        scanner.feed(seq(3), b"04", &mut found);
        assert_eq!(scanner.unfinished(), Some((seq(2), &b"\x1b[?2004"[..])));
        scanner.feed(seq(4), b"h", &mut found);
        assert_eq!(scanner.unfinished(), None);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn escaping_leaves_only_printable_ascii() {
        let all: Vec<u8> = (0..=255).collect();
        let text = Escaped(&all).to_string();
        assert!(text.bytes().all(|b| (0x20..=0x7e).contains(&b)), "{text}");
        assert!(text.starts_with(r"\x00\x01"));
        assert!(text.contains(r"\t\n\x0b\x0c\r"));
        assert!(text.contains(r##" !\"#"##));
        assert!(text.contains(r"[\\]"));
        assert!(text.contains(r"}~\x7f\x80"));
        assert!(text.ends_with(r"\xfe\xff"));
    }

    #[test]
    fn elapsed_time_is_milliseconds_truncated_to_the_microsecond() {
        assert_eq!(Millis(0).to_string(), "0.000");
        assert_eq!(Millis(1_234_999).to_string(), "1.234");
        assert_eq!(Millis(61_000_000_000).to_string(), "61000.000");
    }
}
