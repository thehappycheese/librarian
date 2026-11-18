//! librarian — an LLM that keeps a markdown wiki in ./data/library.
//!
//! The chat is the only interface. Mode, model, budgets and even the API key
//! can be set from the chat; nothing is read from config files. (A provider
//! key in the usual environment variable, e.g. ANTHROPIC_API_KEY, is picked up
//! if present, purely as a convenience.)

mod bashtools;
mod librarian;
mod tools;

use std::{
    future::IntoFuture,
    io::{self, BufRead, Write},
};

use librarian::Mode;
use rig_agent::prelude::*;
use rig_core::{catalog::Catalog, operation::Completion, providers::registry::ConnectOptions};
use tools::{OFFICE, Sandbox, now_utc};

const DATA_DIR: &str = "./data";
const DEFAULT_AUTHOR_BUDGET: usize = 40;
const DEFAULT_RETRIEVAL_BUDGET: usize = 6;
const HISTORY_KEEP: usize = 24; // messages (user + librarian) carried between turns

/// Models tried at start-up, first one whose key is in the environment wins.
const DEFAULT_MODELS: &[(&str, &str)] = &[
    ("ANTHROPIC_API_KEY", "anthropic/claude-sonnet-5-5"),
    ("OPENAI_API_KEY", "openai/gpt-5.5"),
];

const HELP: &str = "\
Just talk. Commands (or plain phrases like \"author mode\" / \"retrieval mode\"):
  /author [turns]    author mode: the librarian writes and reorganises the wiki
  /ask [turns]       retrieval mode: answers from the wiki with a small budget
  /mode              show mode, budgets and model
  /model vendor/id   switch model, e.g. /model anthropic/claude-opus-5-5
  /key <api key>     use this key for the current model's vendor (kept in memory only)
  /forget            clear the conversation memory (the wiki is untouched)
  /help, /quit
Ctrl-C interrupts the librarian mid-task.";

struct Session {
    sandbox: Sandbox,
    mode: Mode,
    author_budget: usize,
    retrieval_budget: usize,
    model_ref: Option<String>,
    api_key: Option<String>,
    model: Option<DynModel<Completion>>,
    history: Vec<Message>,
}

impl Session {
    fn budget(&self) -> usize {
        match self.mode {
            Mode::Author => self.author_budget,
            Mode::Retrieval => self.retrieval_budget,
        }
    }

    fn connect(&mut self, reference: &str) -> Result<(), String> {
        // Remember the choice even if connecting fails, so a later /key can complete it.
        self.model_ref = Some(reference.to_string());
        self.model = None;
        let catalog = Catalog::builtin();
        let mut options = ConnectOptions::new();
        if let Some(key) = &self.api_key {
            options = options.api_key(key.clone());
        }
        let model = catalog.connect_with(reference, options).map_err(|e| e.to_string())?;
        self.model = Some(model);
        Ok(())
    }

    fn status(&self) -> String {
        format!(
            "mode: {} (budget {} turns) · author budget {} · retrieval budget {} · model: {}",
            self.mode.label(),
            self.budget(),
            self.author_budget,
            self.retrieval_budget,
            self.model_ref.as_deref().unwrap_or("none — use /model vendor/id")
        )
    }

    fn remember(&mut self, user: &str, librarian: &str) {
        self.history.push(Message::user(user));
        self.history.push(Message::assistant(librarian));
        let excess = self.history.len().saturating_sub(HISTORY_KEEP);
        self.history.drain(..excess);
    }
}

/// Appends to librarians_office/chat_log.md (human-readable transcript).
fn log_chat(sandbox: &Sandbox, who: &str, text: &str) {
    let path = sandbox.root().join(OFFICE).join("chat_log.md");
    let fresh = !path.exists();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        if fresh {
            let _ = writeln!(f, "# Chat log\n");
        }
        let _ = writeln!(f, "### {who} · {}\n\n{}\n", now_utc(), text.trim());
    }
}

/// Recognises mode switches typed as plain words, e.g. "author mode please".
fn natural_mode_switch(line: &str) -> Option<Mode> {
    let l = line.to_lowercase();
    let l = l.trim().trim_end_matches(['.', '!', '?']);
    if l.split_whitespace().count() > 5 || !l.contains("mode") {
        return None;
    }
    if ["author", "write", "edit", "build"].iter().any(|w| l.contains(w)) {
        Some(Mode::Author)
    } else if ["retriev", "ask", "read", "query", "question", "lookup"].iter().any(|w| l.contains(w)) {
        Some(Mode::Retrieval)
    } else {
        None
    }
}

fn say(text: &str) {
    println!("\x1b[1;36mlibrarian\x1b[0m {text}");
}

fn read_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    io::stdout().flush().ok()?;
    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end().to_string()),
    }
}

/// Handles a chat command. Returns false when the user wants to quit.
fn command(session: &mut Session, line: &str) -> bool {
    let mut parts = line.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    let turns = arg.parse::<usize>().ok().filter(|n| (1..=500).contains(n));
    match cmd {
        "/quit" | "/exit" => return false,
        "/help" => println!("{HELP}"),
        "/author" | "/write" => {
            session.mode = Mode::Author;
            if let Some(n) = turns {
                session.author_budget = n;
            }
            say(&format!("Author mode — I'll file what you tell me. Budget: {} turns per message.", session.author_budget));
        }
        "/ask" | "/retrieve" | "/retrieval" | "/read" => {
            session.mode = Mode::Retrieval;
            if let Some(n) = turns {
                session.retrieval_budget = n;
            }
            say(&format!("Retrieval mode — ask away. Budget: {} turns per question.", session.retrieval_budget));
        }
        "/mode" | "/status" => say(&session.status()),
        "/model" if arg.is_empty() => say(&session.status()),
        "/model" => match session.connect(arg) {
            Ok(()) => say(&format!("Now using {arg}.")),
            Err(e) => say(&format!("Selected {arg}, but it is not connected yet: {e}\nGive the key with /key <api key>.")),
        },
        "/key" if arg.is_empty() => say("Usage: /key <api key>"),
        "/key" => {
            session.api_key = Some(arg.to_string());
            match session.model_ref.clone() {
                Some(r) => match session.connect(&r) {
                    Ok(()) => say(&format!("Key set for {r} (memory only, not saved).")),
                    Err(e) => say(&format!("Key stored, but {r} failed to connect: {e}")),
                },
                None => say("Key stored (memory only). Now pick a model with /model vendor/id."),
            }
        }
        "/forget" => {
            session.history.clear();
            say("Conversation memory cleared. The wiki is untouched.");
        }
        _ => say(&format!("Unknown command {cmd}. Type /help.")),
    }
    true
}

async fn turn(session: &mut Session, input: &str) {
    let Some(model) = session.model.clone() else {
        say("No model connected yet. Use /model vendor/id (and /key <api key> if it isn't in the environment).");
        return;
    };
    let budget = session.budget();
    let agent = librarian::build(session.mode, model, &session.sandbox, budget);
    let run = agent
        .prompt(input)
        .history(session.history.clone())
        .max_turns(budget)
        .into_future();

    log_chat(&session.sandbox, "user", input);
    let reply = tokio::select! {
        result = run => match result {
            Ok(response) => {
                let text = response.output();
                let text = if text.trim().is_empty() { "(done)".to_string() } else { text };
                eprintln!("\x1b[2m  · {} model calls\x1b[0m", response.requests());
                text
            }
            Err(PromptError::MaxTurns { max_turns, .. }) => match session.mode {
                Mode::Author => format!(
                    "[I used my whole budget of {max_turns} turns before finishing. Changes made so far are \
                     saved. Say \"continue\" to let me carry on, or /author <turns> for a bigger budget.]"
                ),
                Mode::Retrieval => format!(
                    "[I couldn't find the answer within {max_turns} turns. Try a more specific question, \
                     /ask <turns> for a bigger budget, or /author if it needs filing first.]"
                ),
            },
            Err(e) => format!("[error: {e}]"),
        },
        _ = tokio::signal::ctrl_c() => {
            "[interrupted — any files already written stay written]".to_string()
        }
    };
    say(&reply);
    log_chat(&session.sandbox, "librarian", &reply);
    session.remember(input, &reply);
}

#[tokio::main]
async fn main() {
    let sandbox = match Sandbox::open(DATA_DIR) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot prepare {DATA_DIR}: {e}");
            std::process::exit(1);
        }
    };
    let mut session = Session {
        sandbox,
        mode: Mode::Retrieval,
        author_budget: DEFAULT_AUTHOR_BUDGET,
        retrieval_budget: DEFAULT_RETRIEVAL_BUDGET,
        model_ref: None,
        api_key: None,
        model: None,
        history: Vec::new(),
    };
    for (env, reference) in DEFAULT_MODELS {
        if std::env::var(env).is_ok_and(|v| !v.is_empty()) && session.connect(reference).is_ok() {
            break;
        }
    }

    println!("\x1b[1m📚 librarian\x1b[0m — wiki at {}/library · type /help", DATA_DIR);
    say(&session.status());
    if session.model.is_none() {
        say("No API key found. Pick a model with /model vendor/id and give its key with /key <api key>.");
    }

    while let Some(line) = read_line(&format!("\x1b[1;33m{}›\x1b[0m ", session.mode.label())) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('/') {
            if !command(&mut session, line) {
                break;
            }
            continue;
        }
        if let Some(mode) = natural_mode_switch(line) {
            command(&mut session, if mode == Mode::Author { "/author" } else { "/ask" });
            continue;
        }
        turn(&mut session, line).await;
    }
    say("Goodbye — the wiki is in ./data/library.");
}

