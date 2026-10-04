//! Wraps kitty graphics commands in tmux's DCS passthrough.
//!
//! A kitty graphics command is an APC string, `ESC _ G ... ESC \`. tmux drops
//! APC strings it does not understand, but it forwards the payload of
//! `ESC P tmux; ... ESC \` to the outer terminal when `allow-passthrough` is on,
//! with every ESC inside doubled.

use memchr::memmem;

const APC_G: &[u8] = b"\x1b_G";
const ST: &[u8] = b"\x1b\\";
const DCS_TMUX: &[u8] = b"\x1bPtmux;";

/// Streams output through, wrapping each complete kitty graphics command.
///
/// Input arrives in arbitrary chunks, so a command split across reads is held
/// back until its terminator shows up, as is a trailing `ESC` or `ESC _` that
/// may turn out to start one.
#[derive(Default)]
pub struct Rewriter {
    pending: Vec<u8>,
}

impl Rewriter {
    /// Appends the rewritten form of everything in `input` that can be
    /// decided now to `out`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        if self.pending.is_empty() {
            let rest = rewrite_complete(input, out);
            self.pending.extend_from_slice(rest);
        } else {
            self.pending.extend_from_slice(input);
            let buf = std::mem::take(&mut self.pending);
            let rest = rewrite_complete(&buf, out);
            self.pending.extend_from_slice(rest);
        }
    }

    /// Emits whatever is still held back, unchanged. For end of stream.
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.pending);
    }
}

/// Rewrites `buf` into `out` up to the first byte that cannot be decided yet,
/// and returns the undecided tail.
fn rewrite_complete<'a>(mut buf: &'a [u8], out: &mut Vec<u8>) -> &'a [u8] {
    loop {
        let Some(start) = memmem::find(buf, APC_G) else {
            let keep = partial_prefix_len(buf);
            out.extend_from_slice(&buf[..buf.len() - keep]);
            return &buf[buf.len() - keep..];
        };
        let body = start + APC_G.len();
        let Some(end) = memmem::find(&buf[body..], ST).map(|i| body + i + ST.len()) else {
            out.extend_from_slice(&buf[..start]);
            return &buf[start..];
        };
        out.extend_from_slice(&buf[..start]);
        wrap(&buf[start..end], out);
        buf = &buf[end..];
    }
}

/// How many trailing bytes of `buf` are a proper prefix of `ESC _ G`.
fn partial_prefix_len(buf: &[u8]) -> usize {
    (1..APC_G.len())
        .rev()
        .find(|&n| buf.ends_with(&APC_G[..n]))
        .unwrap_or(0)
}

fn wrap(cmd: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(DCS_TMUX);
    for &b in cmd {
        if b == 0x1b {
            out.push(0x1b);
        }
        out.push(b);
    }
    out.extend_from_slice(ST);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut r = Rewriter::default();
        let mut out = Vec::new();
        for c in chunks {
            r.feed(c, &mut out);
        }
        r.flush(&mut out);
        out
    }

    const WRAPPED: &[u8] = b"\x1bPtmux;\x1b\x1b_Ga=T;QUJD\x1b\x1b\\\x1b\\";

    #[test]
    fn passes_plain_output_through() {
        assert_eq!(
            run(&[b"hello \x1b[31mred\x1b[0m"]),
            b"hello \x1b[31mred\x1b[0m"
        );
    }

    #[test]
    fn wraps_a_command() {
        let mut want = b"ab".to_vec();
        want.extend_from_slice(WRAPPED);
        want.extend_from_slice(b"cd");
        assert_eq!(run(&[b"ab\x1b_Ga=T;QUJD\x1b\\cd"]), want);
    }

    #[test]
    fn wraps_a_command_split_at_every_byte() {
        let input = b"ab\x1b_Ga=T;QUJD\x1b\\cd\x1b_Ga=T;QUJD\x1b\\";
        let chunks: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(run(&chunks), run(&[input]));
    }

    #[test]
    fn leaves_other_apc_and_escapes_alone() {
        assert_eq!(
            run(&[b"\x1b_Xnot kitty\x1b\\\x1b]0;title\x07"]),
            b"\x1b_Xnot kitty\x1b\\\x1b]0;title\x07"
        );
    }

    #[test]
    fn flushes_an_unterminated_command_unchanged() {
        assert_eq!(run(&[b"x\x1b_Ga=T;QU"]), b"x\x1b_Ga=T;QU");
    }

    #[test]
    fn holds_a_trailing_escape_until_decided() {
        let mut r = Rewriter::default();
        let mut out = Vec::new();
        r.feed(b"x\x1b", &mut out);
        assert_eq!(out, b"x");
        r.feed(b"[0m", &mut out);
        assert_eq!(out, b"x\x1b[0m");
    }
}
