//! Reading chat messages from the terminal, including multi-line pastes.
//!
//! A terminal delivers a paste as many lines in quick succession. A reader
//! thread forwards stdin lines over a channel; lines arriving within a few
//! milliseconds of each other are treated as one burst. A single typed line is
//! sent as-is. A burst of several lines switches to paste mode: the user can
//! add more (another paste, or a last line that had no trailing newline) and
//! sends the whole lump with Enter on an empty line.
//!
//! For typing a multi-line message by hand, start with a line containing only
//! `"""` and finish with another `"""` line.
//!
//! When stdin is not a terminal (piped input), every line is its own message.

use std::{
    io::{self, BufRead, IsTerminal, Write},
    time::Duration,
};

use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Gap between lines that still counts as the same paste.
const BURST_GAP: Duration = Duration::from_millis(30);
const FENCE: &str = "\"\"\"";

pub struct Input {
    rx: UnboundedReceiver<String>,
    interactive: bool,
}

pub enum Read {
    Message(String),
    /// Ctrl-C while composing: the draft was thrown away.
    Discarded,
    /// End of input, or Ctrl-C at an empty prompt.
    Quit,
}

impl Input {
    pub fn spawn() -> Self {
        let (tx, rx) = unbounded_channel();
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { rx, interactive: io::stdin().is_terminal() }
    }

    /// Waits for the next whole message.
    pub async fn next(&mut self, prompt: &str) -> Read {
        print!("{prompt}");
        let _ = io::stdout().flush();
        let first = tokio::select! {
            line = self.rx.recv() => match line {
                Some(line) => line,
                None => return Read::Quit,
            },
            _ = tokio::signal::ctrl_c() => {
                println!();
                return Read::Quit;
            }
        };
        if !self.interactive {
            return Read::Message(first);
        }

        let fenced = first.trim() == FENCE;
        let mut lines = if fenced { Vec::new() } else { vec![first] };
        if self.take_burst(&mut lines, fenced).await {
            return Read::Message(join(lines));
        }
        if !fenced && lines.len() == 1 {
            return Read::Message(join(lines));
        }

        if fenced {
            hint(&format!("multi-line message, finish with {FENCE} on its own line"));
        } else {
            hint(&format!(
                "pasted {} lines. Paste or type more, or press Enter on an empty line to send",
                lines.len()
            ));
        }
        loop {
            print!("\x1b[2m…\x1b[0m ");
            let _ = io::stdout().flush();
            let line = tokio::select! {
                line = self.rx.recv() => line,
                _ = tokio::signal::ctrl_c() => {
                    println!();
                    hint("discarded");
                    return Read::Discarded;
                }
            };
            let Some(line) = line else { break };
            if is_end(&line, fenced) {
                break;
            }
            lines.push(line);
            if self.take_burst(&mut lines, fenced).await {
                break;
            }
        }
        Read::Message(join(lines))
    }

    /// Collects the lines that follow straight after the last one. Returns
    /// true if a fenced message's closing fence turned up in the burst.
    async fn take_burst(&mut self, lines: &mut Vec<String>, fenced: bool) -> bool {
        while let Ok(Some(line)) = tokio::time::timeout(BURST_GAP, self.rx.recv()).await {
            if fenced && is_end(&line, true) {
                return true;
            }
            lines.push(line);
        }
        false
    }
}

fn is_end(line: &str, fenced: bool) -> bool {
    if fenced { line.trim() == FENCE } else { line.trim().is_empty() }
}

fn join(lines: Vec<String>) -> String {
    lines.join("\n").trim().to_string()
}

fn hint(text: &str) {
    eprintln!("\x1b[2m  ({text})\x1b[0m");
}