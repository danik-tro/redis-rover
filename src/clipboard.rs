//! Copy text to the system clipboard (#27).
//!
//! Tries the OS clipboard (arboard) first. If that is unavailable (e.g. over
//! SSH or on a headless Linux box), falls back to the OSC 52 escape sequence,
//! which asks the terminal to set the clipboard. Under tmux that needs
//! `set -g set-clipboard on`.

use std::io::Write;

use base64::Engine;

/// How the text reached the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The OS clipboard.
    System,
    /// The terminal, via OSC 52. Some terminals ignore it or cap its size.
    Terminal,
}

/// Holds the OS clipboard open for the app's lifetime: on X11 the copied
/// text is served by its owner, so dropping it right away would lose it.
#[derive(Default)]
pub struct Clipboard {
    native: Option<arboard::Clipboard>,
}

impl Clipboard {
    /// Copy `text`, preferring the OS clipboard.
    ///
    /// # Errors
    ///
    /// Returns an error if both the OS clipboard and OSC 52 fail.
    pub fn copy(&mut self, text: &str) -> std::io::Result<Via> {
        if self.native.is_none() {
            self.native = arboard::Clipboard::new()
                .inspect_err(|err| log::debug!("OS clipboard unavailable: {err}"))
                .ok();
        }
        if let Some(native) = self.native.as_mut() {
            match native.set_text(text) {
                Ok(()) => return Ok(Via::System),
                Err(err) => {
                    log::debug!("OS clipboard copy failed, using OSC 52: {err}");
                    self.native = None;
                }
            }
        }
        write_osc52(text)?;
        Ok(Via::Terminal)
    }
}

/// Send `text` to the terminal clipboard with OSC 52.
fn write_osc52(text: &str) -> std::io::Result<()> {
    let sequence = osc52_sequence(text, std::env::var_os("TMUX").is_some());
    let mut out = std::io::stdout().lock();
    out.write_all(sequence.as_bytes())?;
    out.flush()
}

/// `ESC ] 52 ; c ; <base64> BEL`, wrapped in a tmux passthrough when needed.
fn osc52_sequence(text: &str, tmux: bool) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let sequence = format!("\x1b]52;c;{encoded}\x07");
    if tmux {
        format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
    } else {
        sequence
    }
}

#[cfg(test)]
mod tests {
    use super::osc52_sequence;

    #[test]
    fn osc52_encodes_text() {
        assert_eq!(osc52_sequence("hi", false), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn osc52_tmux_passthrough_doubles_escapes() {
        assert_eq!(
            osc52_sequence("hi", true),
            "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\"
        );
    }
}
