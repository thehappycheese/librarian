//! Who the librarian is in each mode: instructions, tools and budget.

use rig_agent::agent::{InvalidToolCallAction, InvalidToolCallContext};
use rig_agent::prelude::*;
use rig_core::operation::Completion;

use crate::bashtools::Bash;
use crate::tools::{
    EditFile, LIBRARY, ListFiles, MoveFile, OFFICE, OfficeNote, ReadFile, Sandbox, Search, TrashFile,
    WriteFile, today_utc,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Builds and improves the wiki. Large turn budget, write tools.
    Author,
    /// Answers from the wiki. Small turn budget, read-only tools.
    Retrieval,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Author => "author",
            Mode::Retrieval => "retrieval",
        }
    }
}

const SHARED: &str = r#"You are the Librarian. You keep a wiki of plain markdown files for one user, and the only way the user reaches you is this chat.

Layout (all paths are relative to the data folder):
- library/ — the wiki. Its entry point is library/index.md, which must always exist and link to every page.
- librarians_office/ — your private office: working notes, todo lists, open questions, a style guide, anything that helps you do the job better next time. Also human-readable markdown. librarians_office/changelog.md is written automatically by the file tools; you do not need to maintain it.

Wiki conventions (follow them; refine them in librarians_office/style.md if you find better ones):
- One topic per page. Lowercase kebab-case file names, grouped in a few broad folders (e.g. library/people/, library/projects/, library/howto/). Do not over-nest.
- Every page: `# Title`, a one or two sentence summary, then content in sections. Use relative markdown links between pages, e.g. [Ada](../people/ada-lovelace.md).
- Finish each page with `## Sources`, noting where facts came from (e.g. "Told by the user, 2026-11-18").
- library/index.md: a short intro, then sections listing every page as `- [Title](path) — one-line summary`.
- Record only what the user told you or what is already in the wiki. Never invent facts. Mark uncertainty and contradictions explicitly instead of silently picking one.

Tools: list_files, read_file and search are cheap; bash runs read-only grep/find/wc pipelines for overviews. Call independent tools in the same turn to save budget."#;

const AUTHOR: &str = r#"MODE: AUTHOR. You may create and edit wiki pages.

Your budget is {BUDGET} model turns for this message (each round of tool calls costs one). Plan accordingly and keep 2 turns for the wrap-up.

How to work:
1. Work out what the user is telling you or asking for. If it is a question, answer it from the wiki.
2. Search for existing pages on the topic before creating new ones; extend, merge or correct rather than duplicate.
3. Write or edit the pages. Prefer edit_file for small changes, write_file for new pages or rewrites.
4. Update library/index.md and add links from related pages, so nothing is orphaned.
5. When you notice structural problems (duplicates, a messy folder, pages that should be split), fix them if the budget allows, otherwise add them to librarians_office/todo.md.
6. Reply briefly: what you understood, which pages you created or changed, and any question that would help you fill gaps. Do not paste whole pages back.

If the user gives no new information but asks you to tidy up, work through librarians_office/todo.md."#;

const RETRIEVAL: &str = r#"MODE: RETRIEVAL. You can read the wiki but not change it.

Your budget is only {BUDGET} model turns for this message, so go straight to the right page: the index is below; use search when it does not point the way.

Answer concisely from the wiki and name the pages you used as paths (library/...). If the wiki does not contain the answer, say so plainly. You may add general knowledge only if you label it clearly as not from the library. When something is missing or unclear, record it with office_note in open_questions.md so it can be filled in later.

If the user starts telling you new facts, tell them you are in retrieval mode and that they can switch with /author (or by saying "author mode")."#;

pub fn preamble(mode: Mode, sandbox: &Sandbox, budget: usize) -> String {
    let mode_text = match mode {
        Mode::Author => AUTHOR,
        Mode::Retrieval => RETRIEVAL,
    }
    .replace("{BUDGET}", &budget.to_string());

    let mut out = format!("{SHARED}\n\n{mode_text}\n\nToday is {}.\n", today_utc());
    out.push_str(&format!(
        "\n<current {LIBRARY}/index.md>\n{}\n</current {LIBRARY}/index.md>\n",
        sandbox.index_snapshot(8_000)
    ));
    if mode == Mode::Author {
        for name in ["style.md", "todo.md"] {
            if let Some(text) = sandbox.office_snapshot(name, 3_000) {
                out.push_str(&format!("\n<{OFFICE}/{name}>\n{text}\n</{OFFICE}/{name}>\n"));
            }
        }
    }
    out
}

/// Turns a call to a tool that does not exist in this mode (e.g. write_file
/// during retrieval) into feedback the model can read, instead of ending the run.
struct ExplainUnavailableTools(Mode);

impl AgentHook for ExplainUnavailableTools {
    async fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        _event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        Some(InvalidToolCallAction::skip(match self.0 {
            Mode::Retrieval => "ERROR: that tool does not exist in retrieval mode, which is read-only. \
                                Answer from the wiki, or tell the user to switch with /author.",
            Mode::Author => "ERROR: that tool does not exist or its arguments were malformed. \
                             Check the tool list and try again.",
        }))
    }
}

pub fn build(mode: Mode, model: DynModel<Completion>, sandbox: &Sandbox, budget: usize) -> Agent {
    let sb = || sandbox.clone();
    let builder = AgentBuilder::new(model)
        .name("librarian")
        .preamble(preamble(mode, sandbox, budget))
        .add_hook(ExplainUnavailableTools(mode))
        .tool(ListFiles(sb()))
        .tool(ReadFile(sb()))
        .tool(Search(sb()))
        .tool(OfficeNote(sb()))
        .tool(Bash(sb()));
    match mode {
        Mode::Author => builder
            .tool(WriteFile(sb()))
            .tool(EditFile(sb()))
            .tool(MoveFile(sb()))
            .tool(TrashFile(sb()))
            .max_tokens(16_000),
        Mode::Retrieval => builder.max_tokens(4_000),
    }
    .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::test_utils::{MockCompletionModel, MockTurn};
    use serde_json::json;

    fn sandbox(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("librarian-agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Sandbox::open(&dir).unwrap()
    }

    /// Drives the real agent loop and real tools with a scripted model.
    #[tokio::test]
    async fn author_mode_builds_the_wiki() {
        let sb = sandbox("author");
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("1", "write_file", json!({
                "path": "library/people/ada-lovelace.md",
                "content": "# Ada Lovelace\n\nWrote the first published program.\n\n## Sources\n- Told by the user"
            })),
            MockTurn::tool_call("2", "edit_file", json!({
                "path": "library/index.md",
                "old_text": "This wiki is empty so far.",
                "new_text": "## People\n- [Ada Lovelace](people/ada-lovelace.md) — first programmer\n\nThis wiki is young."
            })),
            MockTurn::tool_call("3", "bash", json!({"command": "find library -name '*.md' | sort"})),
            MockTurn::tool_call("4", "write_file", json!({"path": "../escape.md", "content": "x"})),
            MockTurn::text("Filed Ada under library/people/ada-lovelace.md and linked it from the index."),
        ]);
        let agent = build(Mode::Author, model.clone().into(), &sb, 10);
        let reply = agent.prompt("Ada Lovelace wrote the first program").max_turns(10).await.unwrap();
        assert!(reply.output().contains("Filed Ada"));

        let root = sb.root();
        let page = std::fs::read_to_string(root.join("library/people/ada-lovelace.md")).unwrap();
        assert!(page.starts_with("# Ada Lovelace"));
        let index = std::fs::read_to_string(root.join("library/index.md")).unwrap();
        assert!(index.contains("(people/ada-lovelace.md)"));
        let changelog = std::fs::read_to_string(root.join("librarians_office/changelog.md")).unwrap();
        assert!(changelog.contains("created library/people/ada-lovelace.md"));
        assert!(changelog.contains("edited library/index.md"));
        assert!(!root.parent().unwrap().join("escape.md").exists());

        // The tool results the model saw on later turns.
        let requests = format!("{:?}", model.requests());
        assert!(requests.contains("[exit 0]") && requests.contains("library/index.md"), "bash output reached the model");
        assert!(requests.contains("is not allowed: use plain relative paths"), "sandbox refusal reached the model");
    }

    #[tokio::test]
    async fn retrieval_mode_is_read_only_and_budgeted() {
        let sb = sandbox("retrieval");
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("1", "write_file", json!({"path": "library/x.md", "content": "nope"})),
            MockTurn::tool_call("2", "search", json!({"query": "anything"})),
            MockTurn::tool_call("3", "read_file", json!({"path": "library/index.md"})),
        ]);
        let agent = build(Mode::Retrieval, model.clone().into(), &sb, 2);
        let result = agent.prompt("what's in the wiki?").max_turns(2).await;
        // write_file was answered with feedback, search ran, then the budget of 2 ran out.
        assert!(format!("{:?}", model.requests()).contains("does not exist in retrieval mode"));
        assert!(matches!(result, Err(PromptError::MaxTurns { max_turns: 2, .. })), "{result:?}");
        assert!(!sb.root().join("library/x.md").exists());
        let definitions = format!("{:?}", model.requests()[0]);
        assert!(!definitions.contains("\"write_file\"") && !definitions.contains("name: \"write_file\""));
    }
}
