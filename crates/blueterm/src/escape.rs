//! ssh-style escape sequences on keyboard input: the escape character is only
//! recognized at the beginning of a line (or of the session).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send these bytes to the remote side.
    Send(Vec<u8>),
    /// `~.`: disconnect.
    Disconnect,
    /// `~?`: show help.
    Help,
}

pub struct Escaper {
    esc: Option<u8>,
    at_line_start: bool,
    pending: bool,
}

fn is_newline(b: u8) -> bool {
    b == b'\r' || b == b'\n'
}

impl Escaper {
    /// `esc = None` disables escape processing.
    pub fn new(esc: Option<u8>) -> Self {
        Escaper { esc, at_line_start: true, pending: false }
    }

    pub fn feed(&mut self, input: &[u8]) -> Vec<Action> {
        let Some(esc) = self.esc else {
            return vec![Action::Send(input.to_vec())];
        };
        let mut actions = Vec::new();
        let mut out = Vec::with_capacity(input.len());
        let flush = |out: &mut Vec<u8>, actions: &mut Vec<Action>| {
            if !out.is_empty() {
                actions.push(Action::Send(std::mem::take(out)));
            }
        };
        for &b in input {
            if self.pending {
                self.pending = false;
                match b {
                    b'.' => {
                        flush(&mut out, &mut actions);
                        actions.push(Action::Disconnect);
                        return actions;
                    }
                    b'?' => {
                        flush(&mut out, &mut actions);
                        actions.push(Action::Help);
                        self.at_line_start = true;
                        continue;
                    }
                    b if b == esc => out.push(esc),
                    b => {
                        // Not an escape sequence: pass both through.
                        out.push(esc);
                        out.push(b);
                    }
                }
                self.at_line_start = is_newline(b);
                continue;
            }
            if self.at_line_start && b == esc {
                self.pending = true;
                continue;
            }
            out.push(b);
            self.at_line_start = is_newline(b);
        }
        flush(&mut out, &mut actions);
        actions
    }
}

pub fn help_text(esc: u8) -> String {
    let e = esc as char;
    format!(
        "\r\nSupported escape sequences (at the start of a line):\r\n \
         {e}.   - disconnect\r\n \
         {e}?   - this message\r\n \
         {e}{e}   - send the escape character\r\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(s: &[u8]) -> Action {
        Action::Send(s.to_vec())
    }

    #[test]
    fn disconnect_at_session_start() {
        assert_eq!(Escaper::new(Some(b'~')).feed(b"~."), [Action::Disconnect]);
    }

    #[test]
    fn disconnect_after_newline() {
        let mut e = Escaper::new(Some(b'~'));
        assert_eq!(e.feed(b"ls\r~."), [send(b"ls\r"), Action::Disconnect]);
    }

    #[test]
    fn split_across_reads() {
        let mut e = Escaper::new(Some(b'~'));
        assert_eq!(e.feed(b"x\r"), [send(b"x\r")]);
        assert_eq!(e.feed(b"~"), []);
        assert_eq!(e.feed(b"."), [Action::Disconnect]);
    }

    #[test]
    fn mid_line_tilde_passes_through() {
        let mut e = Escaper::new(Some(b'~'));
        assert_eq!(e.feed(b"cd ~.\r"), [send(b"cd ~.\r")]);
    }

    #[test]
    fn double_escape_sends_one() {
        let mut e = Escaper::new(Some(b'~'));
        assert_eq!(e.feed(b"~~/x"), [send(b"~/x")]);
    }

    #[test]
    fn unknown_sequence_passes_both() {
        let mut e = Escaper::new(Some(b'~'));
        assert_eq!(e.feed(b"~a"), [send(b"~a")]);
    }

    #[test]
    fn help_then_disconnect() {
        let mut e = Escaper::new(Some(b'~'));
        assert_eq!(e.feed(b"~?~."), [Action::Help, Action::Disconnect]);
    }

    #[test]
    fn escape_after_escaped_newline() {
        let mut e = Escaper::new(Some(b'~'));
        // "~" + "\r" is not a sequence; both are sent and we are at a line start again.
        assert_eq!(e.feed(b"~\r~."), [send(b"~\r"), Action::Disconnect]);
    }

    #[test]
    fn disabled() {
        let mut e = Escaper::new(None);
        assert_eq!(e.feed(b"~."), [send(b"~.")]);
    }
}
