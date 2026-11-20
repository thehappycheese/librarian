//! librarian — an LLM that keeps a markdown wiki in ./data/library.
//!
//! The chat is the only interface. Model, budgets and even the API key are set
//! from the chat; nothing is read from config files. (A provider key in the
//! usual environment variable, e.g. ANTHROPIC_API_KEY, is used if present.)

mod input;
mod librarian;
mod mdtools;
mod page;
mod sandbox;
mod search;
mod tools;

use std::{future::IntoFuture, io::Write, sync::Arc, sync::atomic::Ordering};

use rig_agent::prelude::*;
use rig_core::{catalog::Catalog, operation::Completion, providers::registry::ConnectOptions};
use sandbox::{OFFICE, Sandbox, now_utc};
use tokio::sync::Mutex;

const DATA_DIR: &str = "./data";
const DEFAULT_TURNS: usize = 30;
const DEFAULT_TOKENS: u64 = 300_000;
/// Conversation exchanges (user message + reply) carried between messages.
const HISTORY_EXCHANGES: usize = 20;

/// Models tried at start-up; the first whose key is in the environment wins.
const DEFAULT_MODELS: &[(&str, &str)] = &[
    ("ANTHROPIC_API_KEY", "anthropic/claude-sonnet-5-5"),
    ("OPENAI_API_KEY", "openai/gpt-5.5"),
];

const HELP: &str = "\
Just talk: tell the librarian things to file, ask questions, or ask it to tidy up.
It may ask you questions while it works. Commands:
  /budget [turns] [tokens]  show or set the per-message budget (default 30 turns, 300000 tokens)
  /model vendor/id          switch model, e.g. /model anthropic/claude-opus-5-5
  /key <api key>            key for the current model's vendor (kept in memory only)
  /forget                   clear the conversation memory (the wiki is untouched)
  /status, /help, /quit
Multi-line: just paste, then press Enter on an empty line to send. To type several
lines by hand, start and end the message with a line containing only \"\"\".
Ctrl-C interrupts the librarian mid-task, discards a draft, or quits at an empty prompt.";

struct Session {
    sandbox: Sandbox,
    input: Arc<Mutex<input::Input>>,
    turns: usize,
    tokens: u64,
    model_ref: Option<String>,
    api_key: Option<String>,
    model: Option<DynModel<Completion>>,
    history: Vec<Message>,
}

impl Session {
    fn connect(&mut self, reference: &str) -> Result<(), String> {
        // Remember the choice even if connecting fails, so a later /key can complete it.
        self.model_ref = Some(reference.to_string());
        self.model = None;
        let mut options = ConnectOptions::new();
        if let Some(key) = &self.api_key {
            options = options.api_key(key.clone());
        }
        self.model = Some(Catalog::builtin().connect_with(reference, options).map_err(|e| e.to_string())?);
        Ok(())
    }

    fn status(&self) -> String {
        format!(
            "budget {} turns / {} tokens per message · memory {} of {} exchanges · model: {}",
            self.turns,
            self.tokens,
            self.history.len() / 2,
            HISTORY_EXCHANGES,
            match (&self.model_ref, &self.model) {
                (Some(r), Some(_)) => r.clone(),
                (Some(r), None) => format!("{r} (not connected — /key <api key>)"),
                _ => "none — use /model vendor/id".into(),
            }
        )
    }

    fn remember(&mut self, user: &str, librarian: &str) {
        self.history.push(Message::user(user));
        self.history.push(Message::assistant(librarian));
        let excess = self.history.len().saturating_sub(HISTORY_EXCHANGES * 2);
        self.history.drain(..excess);
    }
}

/// Appends to librarians_office/chat_log.md (human-readable transcript).
fn log_chat(sandbox: &Sandbox, who: &str, text: &str) {
    let rel = format!("{OFFICE}/chat_log.md");
    if !sandbox.exists(&rel) {
        let _ = sandbox.append(&rel, "# Chat log\n");
    }
    let _ = sandbox.append(&rel, &format!("### {who} · {}\n\n{}\n", now_utc(), text.trim()));
}

fn say(text: &str) {
    println!("\x1b[1;36mlibrarian\x1b[0m {text}");
}

/// Handles a chat command. Returns false when the user wants to quit.
fn command(session: &mut Session, line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let args: Vec<&str> = parts.collect();
    let arg = line[cmd.len()..].trim();
    match cmd {
        "/quit" | "/exit" => return false,
        "/help" => println!("{HELP}"),
        "/status" | "/mode" => say(&session.status()),
        "/budget" => {
            if let Some(t) = args.first() {
                match t.parse::<usize>() {
                    Ok(n) if (1..=500).contains(&n) => session.turns = n,
                    _ => return say_true("turns must be a number from 1 to 500"),
                }
            }
            if let Some(t) = args.get(1) {
                match t.replace('_', "").parse::<u64>() {
                    Ok(n) if n >= 1_000 => session.tokens = n,
                    _ => return say_true("tokens must be a number of at least 1000"),
                }
            }
            say(&session.status());
        }
        "/author" | "/ask" | "/retrieve" | "/write" => {
            say("There is just one mode now: tell me things, ask me things, or ask me to tidy up.")
        }
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

fn say_true(text: &str) -> bool {
    say(text);
    true
}

async fn turn(session: &mut Session, message: &str) {
    let Some(model) = session.model.clone() else {
        say("No model connected yet. Use /model vendor/id (and /key <api key> if it isn't in the environment).");
        return;
    };
    let (agent, used) = librarian::build(model, &session.sandbox, session.input.clone(), session.turns, session.tokens);
    let run = agent
        .prompt(message)
        .history(session.history.clone())
        .max_turns(session.turns)
        .into_future();

    log_chat(&session.sandbox, "user", message);
    let carry_on = "Work so far is saved; say \"continue\" to carry on, or raise the limit with /budget.";
    let reply = tokio::select! {
        result = run => match result {
            Ok(response) => {
                let text = response.output();
                if text.trim().is_empty() { "(done)".to_string() } else { text }
            }
            Err(PromptError::MaxTurns { max_turns, .. }) => {
                format!("[I used all {max_turns} turns before finishing. {carry_on}]")
            }
            Err(PromptError::Cancelled { reason, .. }) => format!("[Stopped: {reason}. {carry_on}]"),
            Err(e) => format!("[error: {e}]"),
        },
        _ = tokio::signal::ctrl_c() => {
            "[interrupted — any changes already made stay made]".to_string()
        }
    };
    eprintln!("\x1b[2m  · {} tokens\x1b[0m", used.load(Ordering::Relaxed));
    say(&reply);
    log_chat(&session.sandbox, "librarian", &reply);
    session.remember(message, &reply);
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
        input: Arc::new(Mutex::new(input::Input::spawn())),
        turns: DEFAULT_TURNS,
        tokens: DEFAULT_TOKENS,
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

    println!("\x1b[1m📚 librarian\x1b[0m — wiki at {DATA_DIR}/library · type /help");
    say(&session.status());
    if session.model.is_none() {
        say("No API key found. Pick a model with /model vendor/id and give its key with /key <api key>.");
    }

    loop {
        let next = session.input.lock().await.next("\x1b[1;33m›\x1b[0m ").await;
        let message = match next {
            input::Read::Message(m) => m,
            input::Read::Discarded => continue,
            input::Read::Quit => break,
        };
        if message.is_empty() {
            continue;
        }
        // Commands only count as a single line; a pasted lump that happens to
        // start with '/' is just text.
        if message.starts_with('/') && !message.contains('\n') {
            if !command(&mut session, &message) {
                break;
            }
            continue;
        }
        turn(&mut session, &message).await;
        let _ = std::io::stdout().flush();
    }
    say("Goodbye — the wiki is in ./data/library.");
}
