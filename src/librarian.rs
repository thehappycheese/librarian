//! Who the librarian is: instructions, tools, budgets and context hygiene.
//!
//! There is one mode. Every tool is always available; the instructions tell
//! the model to answer cheaply, file new information when it is given, ask
//! when unsure, and break big jobs into steps across messages.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use rig_agent::agent::{
    CompletionCallAction, CompletionCallEvent, InvalidToolCallAction, InvalidToolCallContext, ModelTurnAction,
    ModelTurnFinished,
};
use rig_agent::prelude::*;
use rig_agent::run::RequestPatch;
use rig_core::message::{AssistantContent, Message, ToolResultContent, UserContent};
use rig_core::operation::Completion;
use tokio::sync::Mutex;

use crate::input::Input;
use crate::mdtools::{EditSection, EditText, MergePages, MovePage, MoveSection, SetHeader, SplitSection, WritePage};
use crate::page::{FIELD_MAX, HEADER_LINES};
use crate::sandbox::{INDEX, OFFICE, Sandbox, today_utc};
use crate::search::{Grep, Search};
use crate::tools::{AskUser, Head, ListPages, OfficeWrite, Outline, ReadFile, Trash};

/// Tool results from this many most recent tool rounds stay verbatim.
const KEEP_RECENT_ROUNDS: usize = 2;
/// Older tool results / tool-call arguments longer than this are shrunk.
const SHRINK_OVER: usize = 400;

const INSTRUCTIONS: &str = r#"You are the Librarian. You keep a wiki of plain markdown files for one user, and the only way the user reaches you is this chat. The user may tell you things to file, ask questions, or ask you to reorganise; often all three. File new information whenever you are given it; when you are only asked something, answer without editing (but note gaps).

Layout (paths are relative to the data folder):
- library/ — the wiki. library/index.md is the entry point and must link to every page.
- librarians_office/ — your own working space: todo.md (a plan for multi-step work), style.md (conventions you settle on), glossary.md (names and synonyms), open_questions.md (gaps). changelog.md and chat_log.md are written automatically.

Every page starts with a fixed {HEADER_LINES}-line header: title, summary, aliases, tags, see_also, updated (each one line, ≤{FIELD_MAX} chars). The tools write it for you: write_page takes the fields, set_header changes them, `updated` is automatic. Make headers good, they are how you find things later: a precise one-sentence summary, the other names people would use in aliases, related pages in see_also.

Reading costs budget, so read in this order and stop as soon as you know enough:
1. The index (below) and list_pages (title + summary of every page).
2. search (ranked, natural words) or grep (exact text / regex).
3. head on several candidate pages at once: the first {HEADER_LINES} lines are exactly the header.
4. outline, then read with section: just the part you need.
5. A whole page with read only when you really need all of it.

Writing:
- New page: write_page with path library/<area>/<kebab-name>.md, body starting with '# Title'. One topic per page, a few broad folders.
- Changing a page: edit_section, edit_text, set_header. Never rewrite a whole page to change part of it.
- Restructuring: split_section, move_section, merge_pages, move_page. They fix relative links and see_also for you.
- Add new pages to library/index.md (edit_section on the right area) so nothing is orphaned.
- Record only what the user said or what the wiki already says. Never invent facts. Mark uncertainty and contradictions explicitly; finish pages with a '## Sources' section ("Told by the user, {TODAY}").

Asking: if you cannot file something correctly without an answer (ambiguous name, contradiction), use ask_user with one short question. Questions that can wait go at the end of your reply.

Budget for this message: {TURNS} model turns and about {TOKENS} tokens. Older tool results are shrunk to their first line as you go; call the tool again if you need them. Small questions should take 1–4 turns. For big jobs (review, consolidate, reorganise the whole wiki) never read everything: survey with list_pages and head, write a short plan to librarians_office/todo.md, do the first items, and tell the user to say "continue". On "continue", resume from todo.md and tick items off.

Reply briefly: answer with the page paths you used, or say what you filed and where. Don't paste pages back. If the wiki doesn't know something, say so; general knowledge is fine only when clearly labelled as not from the library."#;

pub fn preamble(sandbox: &Sandbox, turns: usize, tokens: u64) -> String {
    let mut out = INSTRUCTIONS
        .replace("{HEADER_LINES}", &HEADER_LINES.to_string())
        .replace("{FIELD_MAX}", &FIELD_MAX.to_string())
        .replace("{TODAY}", &today_utc())
        .replace("{TURNS}", &turns.to_string())
        .replace("{TOKENS}", &tokens.to_string());
    out.push_str(&format!("\n\nToday is {}.\n", today_utc()));
    out.push_str(&format!("\n<{INDEX}>\n{}\n</{INDEX}>\n", sandbox.snapshot(INDEX, 6_000).unwrap_or_default()));
    for name in ["todo.md", "style.md"] {
        let rel = format!("{OFFICE}/{name}");
        if let Some(text) = sandbox.snapshot(&rel, 2_500) {
            out.push_str(&format!("\n<{rel}>\n{text}\n</{rel}>\n"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// Keeps a run lean and bounded:
/// - before each model call, older tool results and long tool-call arguments
///   (e.g. a whole page passed to write_page) are shrunk to a one-line stub;
/// - token usage is summed and the run stops once the budget is spent;
/// - calls to unknown tools get feedback instead of ending the run.
struct Housekeeping {
    token_budget: u64,
    used: Arc<AtomicU64>,
}

impl AgentHook for Housekeeping {
    async fn on_completion_call(&self, _ctx: &HookContext, event: CompletionCallEvent<'_>) -> CompletionCallAction {
        let used = self.used.load(Ordering::Relaxed);
        if used >= self.token_budget {
            return CompletionCallAction::stop(format!("token budget spent ({used} of {} tokens)", self.token_budget));
        }
        match shrink_history(event.history) {
            Some(history) => CompletionCallAction::patch(RequestPatch::new().history(history)),
            None => CompletionCallAction::Continue,
        }
    }

    async fn on_model_turn_finished(&self, _ctx: &HookContext, event: ModelTurnFinished<'_>) -> ModelTurnAction {
        let u = &event.usage;
        let n = u.total_tokens.unwrap_or(u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0));
        self.used.fetch_add(n, Ordering::Relaxed);
        ModelTurnAction::Continue
    }

    async fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        _event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        Some(InvalidToolCallAction::skip(
            "ERROR: that tool does not exist or its arguments were not a JSON object. Check the tool list and try again.",
        ))
    }
}

fn has_tool_result(m: &Message) -> bool {
    matches!(m, Message::User { content } if content.iter().any(|c| matches!(c, UserContent::ToolResult(_))))
}

fn stub(text: &str) -> String {
    let first: String = text.lines().next().unwrap_or("").chars().take(160).collect();
    format!("{first} … [older output shrunk from {} chars; call the tool again if you need it]", text.len())
}

/// Returns a copy of `history` with everything before the last
/// [`KEEP_RECENT_ROUNDS`] tool rounds shrunk, or `None` if nothing changed.
pub fn shrink_history(history: &[Arc<Message>]) -> Option<Vec<Arc<Message>>> {
    let rounds: Vec<usize> = history.iter().enumerate().filter(|(_, m)| has_tool_result(m)).map(|(i, _)| i).collect();
    // The current prompt holds the newest results, so keep one fewer here.
    let keep = KEEP_RECENT_ROUNDS.saturating_sub(1);
    let cutoff = match keep {
        0 => history.len(),
        _ if rounds.len() < keep => return None,
        // Keep the tool call that produced the oldest kept result, too.
        _ => rounds[rounds.len() - keep].saturating_sub(1),
    };
    let mut changed = false;
    let out = history
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if i >= cutoff {
                return m.clone();
            }
            let mut msg = (**m).clone();
            let mut touched = false;
            match &mut msg {
                Message::User { content } => {
                    for c in content.iter_mut() {
                        if let UserContent::ToolResult(r) = c {
                            for part in r.content.iter_mut() {
                                if let ToolResultContent::Text(t) = part
                                    && t.text.len() > SHRINK_OVER
                                {
                                    t.text = stub(&t.text);
                                    touched = true;
                                }
                            }
                        }
                    }
                }
                Message::Assistant(a) => {
                    for c in a.content.iter_mut() {
                        if let AssistantContent::ToolCall(call) = c {
                            for value in call.function.arguments.values_mut() {
                                if let Some(s) = value.as_str()
                                    && s.len() > SHRINK_OVER
                                {
                                    *value = serde_json::Value::String(format!(
                                        "[{} chars, already applied]",
                                        s.len()
                                    ));
                                    touched = true;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            if touched {
                changed = true;
                Arc::new(msg)
            } else {
                m.clone()
            }
        })
        .collect();
    changed.then_some(out)
}

// ---------------------------------------------------------------------------

/// Builds the agent. Returns it with a counter of tokens used in the run.
pub fn build(
    model: DynModel<Completion>,
    sandbox: &Sandbox,
    input: Arc<Mutex<Input>>,
    turns: usize,
    tokens: u64,
) -> (Agent, Arc<AtomicU64>) {
    let used = Arc::new(AtomicU64::new(0));
    let sb = || sandbox.clone();
    let agent = AgentBuilder::new(model)
        .name("librarian")
        .preamble(preamble(sandbox, turns, tokens))
        .add_hook(Housekeeping { token_budget: tokens, used: used.clone() })
        // finding
        .tool(ListPages(sb()))
        .tool(Search(sb()))
        .tool(Grep(sb()))
        .tool(Head(sb()))
        .tool(Outline(sb()))
        .tool(ReadFile(sb()))
        // writing
        .tool(WritePage(sb()))
        .tool(SetHeader(sb()))
        .tool(EditSection(sb()))
        .tool(EditText(sb()))
        // restructuring
        .tool(SplitSection(sb()))
        .tool(MoveSection(sb()))
        .tool(MergePages(sb()))
        .tool(MovePage(sb()))
        .tool(Trash(sb()))
        // office and user
        .tool(OfficeWrite(sb()))
        .tool(AskUser(input))
        .max_tokens(16_000)
        .build();
    (agent, used)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::test_sandbox;
    use rig_core::test_utils::{MockCompletionModel, MockTurn};
    use serde_json::json;

    fn input() -> Arc<Mutex<Input>> {
        Arc::new(Mutex::new(Input::spawn()))
    }

    /// Drives the real agent loop and tools with a scripted model.
    #[tokio::test]
    async fn files_a_fact_then_old_results_shrink() {
        let sb = test_sandbox("agent");
        let long_body = format!("# Ada Lovelace\n\n{}\n\n## Sources\n\nTold by the user.", "Wrote the first program. ".repeat(40));
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("1", "search", json!({"query": "Ada Lovelace"})),
            MockTurn::tool_call("2", "write_page", json!({
                "path": "library/people/ada-lovelace.md", "title": "Ada Lovelace",
                "summary": "Mathematician who wrote the first published program.",
                "aliases": "Augusta Ada King", "tags": "people", "body": long_body
            })),
            MockTurn::tool_call("3", "edit_section", json!({
                "path": "library/index.md", "action": "append",
                "content": "## People\n\n- [Ada Lovelace](people/ada-lovelace.md) — first programmer"
            })),
            MockTurn::tool_call("4", "head", json!({"paths": ["library/people/ada-lovelace.md", "library/index.md"]})),
            MockTurn::tool_call("5", "list_pages", json!({})),
            MockTurn::text("Filed Ada under library/people/ada-lovelace.md and linked it from the index."),
        ]);
        let (agent, used) = build(model.clone().into(), &sb, input(), 12, 1_000_000);
        let reply = agent.prompt("Ada Lovelace wrote the first program").max_turns(12).await.unwrap();
        assert!(reply.output().contains("Filed Ada"));
        assert!(used.load(Ordering::Relaxed) < 1_000_000);

        let text = sb.read("library/people/ada-lovelace.md").unwrap();
        assert!(text.starts_with("---\ntitle: Ada Lovelace\nsummary: Mathematician"));
        assert_eq!(text.lines().nth(HEADER_LINES), Some("# Ada Lovelace"));
        assert!(sb.read(INDEX).unwrap().contains("(people/ada-lovelace.md)"));

        let requests = model.requests();
        let last = format!("{:?}", requests.last().unwrap());
        // The list_pages result (latest) found the page in the index.
        assert!(last.contains("Ada Lovelace: Mathematician") && !last.contains("not linked from index"));
        // By the last call, the long write_page body sent in turn 2 is shrunk.
        assert!(last.contains("chars, already applied"), "old tool-call arguments shrunk");
        let first_with_body = requests.iter().position(|r| format!("{r:?}").contains("Wrote the first program. Wrote")).unwrap();
        assert!(first_with_body < requests.len() - 1);
    }

    #[tokio::test]
    async fn bad_calls_get_feedback_and_budgets_stop_runs() {
        let sb = test_sandbox("agent-budget");
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("1", "bash", json!({"command": "rm -rf /"})),
            MockTurn::tool_call("2", "read", json!({"path": "/etc/passwd"})),
            MockTurn::tool_call("3", "read", json!({"path": "library/index.md"})),
        ]);
        let (agent, _) = build(model.clone().into(), &sb, input(), 3, 1_000_000);
        let result = agent.prompt("hi").max_turns(3).await;
        assert!(matches!(result, Err(PromptError::MaxTurns { max_turns: 3, .. })), "{result:?}");
        let seen = format!("{:?}", model.requests());
        assert!(seen.contains("that tool does not exist"));
        assert!(seen.contains("'/etc/passwd' is not allowed"));

        // A tiny token budget stops the run before the second model call.
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("1", "list_pages", json!({})).with_usage(rig_core::completion::Usage::new().input_tokens(900).output_tokens(200)),
            MockTurn::text("never reached"),
        ]);
        let (agent, used) = build(model.clone().into(), &sb, input(), 10, 1_000);
        let result = agent.prompt("hi").max_turns(10).await;
        assert!(matches!(result, Err(PromptError::Cancelled { .. })), "{result:?}");
        assert_eq!(used.load(Ordering::Relaxed), 1_100);
    }
}
