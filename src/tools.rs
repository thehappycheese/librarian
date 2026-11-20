//! Reading tools, the office, trash and `ask_user`. Plain Rust, no shell.
//!
//! Tools never return `Err`: failures come back as `ERROR: ...` text so the
//! model can read them and correct itself. The first line of every result
//! says what it is, because older results are later shrunk to that line.

use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::Mutex;

use crate::input::{Input, Read};
use crate::page::{self, HEADER_LINES, Page, file_line, find_section, outgoing_links, sections};
use crate::sandbox::{INDEX, LIBRARY, OFFICE, Sandbox, now_utc, trace, truncate};

const MAX_READ_LINES: usize = 300;
const MAX_LIST: usize = 300;
const MAX_HEAD_PATHS: usize = 25;

/// Implements rig's `Tool` for a struct with
/// `fn run(&self, args) -> Result<String, String>`.
macro_rules! tool {
    ($ty:ident, $args:ty, $name:literal, read_only: $ro:expr, $desc:expr, $params:tt) => {
        impl rig_core::tool::Tool for $ty {
            const NAME: &'static str = $name;
            type Args = $args;
            type Output = String;
            type Error = std::convert::Infallible;
            fn description(&self) -> String {
                String::from($desc)
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!($params)
            }
            fn annotations(&self) -> rig_core::completion::ToolAnnotations {
                rig_core::completion::ToolAnnotations { read_only_hint: Some($ro), ..Default::default() }
            }
            async fn call(
                &self,
                _: &mut rig_core::tool::ToolContext,
                args: $args,
            ) -> Result<String, std::convert::Infallible> {
                Ok(self.run(args).unwrap_or_else(|e| format!("ERROR: {e}")))
            }
        }
    };
}
pub(crate) use tool;

/// Loads a wiki page (leniently, see [`Page::parse`]).
pub fn load(sb: &Sandbox, path: &str) -> Result<(String, Page), String> {
    let path = sb.page_path(path)?;
    let text = sb.read(&path)?;
    Ok((path.clone(), Page::parse(&text, &path).0))
}

// ---------------------------------------------------------------------------

pub struct ListPages(pub Sandbox);

#[derive(Deserialize)]
pub struct ListArgs {
    #[serde(default)]
    folder: Option<String>,
}

impl ListPages {
    fn run(&self, a: ListArgs) -> Result<String, String> {
        let folder = a.folder.filter(|f| !f.trim().is_empty()).unwrap_or_else(|| LIBRARY.into());
        trace(format!("list {folder}"));
        let folder = self.0.clean(&folder)?;
        let files = self.0.files(&folder)?;
        if folder.starts_with(OFFICE) {
            let lines: Vec<String> = files
                .iter()
                .take(MAX_LIST)
                .map(|f| format!("{f} ({} lines)", self.0.read(f).map(|t| t.lines().count()).unwrap_or(0)))
                .collect();
            return Ok(format!("{} files in {folder}\n{}", files.len(), lines.join("\n")));
        }
        let index = self.0.read(INDEX).unwrap_or_default();
        let (index_page, _) = Page::parse(&index, INDEX);
        let mut in_index = outgoing_links(&index_page.body, LIBRARY);
        in_index.extend(index_page.header.see_also_list());
        let mut out = vec![format!("{} files in {folder} (path — title: summary)", files.len())];
        for f in files.iter().take(MAX_LIST) {
            if !f.ends_with(".md") {
                out.push(format!("{f} (not a page)"));
                continue;
            }
            let text = self.0.read(f).unwrap_or_default();
            let (p, valid) = Page::parse(&text, f);
            let mut line = format!("{f} — {}: {}", p.header.title, p.header.summary);
            if !valid {
                line.push_str("  ⚠ no header");
            }
            if f != INDEX && !in_index.contains(f) {
                line.push_str("  ⚠ not linked from index");
            }
            out.push(line);
        }
        if files.len() > MAX_LIST {
            out.push(format!("… {} more; list a subfolder", files.len() - MAX_LIST));
        }
        Ok(out.join("\n"))
    }
}

tool!(ListPages, ListArgs, "list_pages", read_only: true,
    "List wiki pages with their title and summary (from the header), flagging pages missing a header or not linked from the index. With folder 'librarians_office' it lists office files.",
    {"type":"object","properties":{"folder":{"type":"string","description":"Default 'library'. A subfolder like 'library/people', or 'librarians_office'."}}});

// ---------------------------------------------------------------------------

pub struct Head(pub Sandbox);

#[derive(Deserialize)]
pub struct HeadArgs {
    paths: Vec<String>,
    #[serde(default)]
    lines: Option<usize>,
}

impl Head {
    fn run(&self, a: HeadArgs) -> Result<String, String> {
        let n = a.lines.unwrap_or(HEADER_LINES).clamp(1, 200);
        trace(format!("head -{n} {}", a.paths.join(" ")));
        if a.paths.is_empty() {
            return Err("give at least one path".into());
        }
        let mut out = vec![format!("first {n} lines of {} file(s)", a.paths.len().min(MAX_HEAD_PATHS))];
        for p in a.paths.iter().take(MAX_HEAD_PATHS) {
            out.push(format!("== {p} =="));
            match self.0.read(p) {
                Ok(text) => out.push(text.lines().take(n).collect::<Vec<_>>().join("\n")),
                Err(e) => out.push(format!("ERROR: {e}")),
            }
        }
        Ok(out.join("\n"))
    }
}

tool!(Head, HeadArgs, "head", read_only: true,
    format!("Read the first lines of one or more files at once. The default ({HEADER_LINES} lines) is exactly a page's header (title, summary, aliases, tags, see_also, updated): the cheap way to check what pages contain before reading them."),
    {"type":"object","required":["paths"],"properties":{
        "paths":{"type":"array","items":{"type":"string"},"description":"Up to 25 paths"},
        "lines":{"type":"integer","description":"Default 8 = the header"}}});

// ---------------------------------------------------------------------------

pub struct Outline(pub Sandbox);

#[derive(Deserialize)]
pub struct PathArgs {
    path: String,
}

impl Outline {
    fn run(&self, a: PathArgs) -> Result<String, String> {
        trace(format!("outline {}", a.path));
        let (path, page) = load(&self.0, &a.path)?;
        let lines = page.body_lines();
        let mut out = vec![
            format!("outline of {path} ({} lines): {} — {}", lines.len() + HEADER_LINES, page.header.title, page.header.summary),
        ];
        for s in sections(&lines) {
            out.push(format!(
                "{}{} {} — lines {}-{} ({} lines)",
                "  ".repeat(s.level - 1),
                "#".repeat(s.level),
                s.title,
                file_line(s.start),
                file_line(s.end - 1),
                s.end - s.start
            ));
        }
        Ok(out.join("\n"))
    }
}

tool!(Outline, PathArgs, "outline", read_only: true,
    "Show a page's headings with line ranges and sizes, so you can read or edit just one section.",
    {"type":"object","required":["path"],"properties":{"path":{"type":"string"}}});

// ---------------------------------------------------------------------------

pub struct ReadFile(pub Sandbox);

#[derive(Deserialize)]
pub struct ReadArgs {
    path: String,
    #[serde(default)]
    section: Option<String>,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    max_lines: Option<usize>,
}

impl ReadFile {
    fn run(&self, a: ReadArgs) -> Result<String, String> {
        if let Some(section) = a.section.as_deref().filter(|s| !s.trim().is_empty()) {
            trace(format!("read {} § {section}", a.path));
            let (path, page) = load(&self.0, &a.path)?;
            let lines = page.body_lines();
            let s = find_section(&lines, section)?;
            return Ok(format!(
                "[{path} § {} — lines {}-{}]\n{}",
                s.title,
                file_line(s.start),
                file_line(s.end - 1),
                lines[s.start..s.end].join("\n")
            ));
        }
        trace(format!("read {}", a.path));
        let text = self.0.read(&a.path)?;
        let lines: Vec<&str> = text.lines().collect();
        let start = a.start_line.unwrap_or(1).max(1);
        if start > lines.len().max(1) {
            return Err(format!("'{}' has only {} lines", a.path, lines.len()));
        }
        let end = (start - 1 + a.max_lines.unwrap_or(MAX_READ_LINES).clamp(1, 2000)).min(lines.len());
        Ok(format!("[{} — lines {start}-{end} of {}]\n{}", a.path, lines.len(), lines[start - 1..end].join("\n")))
    }
}

tool!(ReadFile, ReadArgs, "read", read_only: true,
    format!("Read a file, or just one section of a page (a heading and everything under it). Without a section it returns up to {MAX_READ_LINES} lines from start_line."),
    {"type":"object","required":["path"],"properties":{
        "path":{"type":"string"},
        "section":{"type":"string","description":"Heading text, e.g. 'Early life'. Use 'Heading@LINE' if the title repeats."},
        "start_line":{"type":"integer"},"max_lines":{"type":"integer"}}});

// ---------------------------------------------------------------------------

pub struct OfficeWrite(pub Sandbox);

#[derive(Deserialize)]
pub struct OfficeArgs {
    file: String,
    content: String,
    #[serde(default)]
    append: bool,
}

impl OfficeWrite {
    fn run(&self, a: OfficeArgs) -> Result<String, String> {
        let path = self.0.office_path(&a.file)?;
        if a.append {
            trace(format!("note → {path}"));
            self.0.append(&path, &format!("- {} — {}", now_utc(), a.content.trim()))?;
            Ok(format!("appended to {path}"))
        } else {
            trace(format!("write {path}"));
            self.0.write(&path, &a.content, "wrote")?;
            Ok(format!("wrote {path} ({} lines)", a.content.lines().count()))
        }
    }
}

tool!(OfficeWrite, OfficeArgs, "office_write",
    read_only: false,
    format!("Write your own working files in {OFFICE}/ (todo.md, style.md, glossary.md, open_questions.md, plans…). append:true adds a timestamped bullet instead of replacing the file."),
    {"type":"object","required":["file","content"],"properties":{
        "file":{"type":"string","description":"e.g. todo.md"},
        "content":{"type":"string"},
        "append":{"type":"boolean"}}});

// ---------------------------------------------------------------------------

pub struct Trash(pub Sandbox);

impl Trash {
    fn run(&self, a: PathArgs) -> Result<String, String> {
        trace(format!("trash {}", a.path));
        let path = self.0.clean(&a.path)?;
        if path == INDEX {
            return Err("the index is the wiki's entry point; edit it instead".into());
        }
        if !self.0.exists(&path) {
            return Err(format!("'{path}' does not exist"));
        }
        let to = self.0.trash(&path)?;
        let inbound = inbound_links(&self.0, &path);
        let mut out = format!("moved {path} to {to}");
        if !inbound.is_empty() {
            out.push_str(&format!("\nstill linking to it (fix these): {}", inbound.join(", ")));
        }
        Ok(out)
    }
}

/// Pages whose body links or see_also point at `target`.
pub fn inbound_links(sb: &Sandbox, target: &str) -> Vec<String> {
    sb.pages()
        .into_iter()
        .filter(|p| p != target)
        .filter(|p| {
            let (page, _) = Page::parse(&sb.read(p).unwrap_or_default(), p);
            outgoing_links(&page.body, page::dir_of(p)).iter().any(|l| l == target)
                || page.header.see_also_list().iter().any(|l| l == target)
        })
        .collect()
}

tool!(Trash, PathArgs, "trash", read_only: false,
    format!("Remove a page or office file by moving it to {OFFICE}/trash/ (recoverable). Reports pages that still link to it. To fold a page into another, use merge_pages instead."),
    {"type":"object","required":["path"],"properties":{"path":{"type":"string"}}});

// ---------------------------------------------------------------------------

/// Lets the librarian ask the user something mid-task.
pub struct AskUser(pub Arc<Mutex<Input>>);

#[derive(Deserialize)]
pub struct AskArgs {
    question: String,
}

impl rig_core::tool::Tool for AskUser {
    const NAME: &'static str = "ask_user";
    type Args = AskArgs;
    type Output = String;
    type Error = std::convert::Infallible;

    fn description(&self) -> String {
        "Ask the user a short question and wait for the answer. Use it when you cannot file \
         something correctly without it (ambiguity, contradiction with the wiki). Questions \
         that can wait belong in your final reply instead."
            .into()
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","required":["question"],"properties":{"question":{"type":"string"}}})
    }
    fn annotations(&self) -> rig_core::completion::ToolAnnotations {
        // Not read-only: it must never run in parallel with another question.
        rig_core::completion::ToolAnnotations::default()
    }
    async fn call(&self, _: &mut rig_core::tool::ToolContext, a: AskArgs) -> Result<String, std::convert::Infallible> {
        println!("\x1b[1;36mlibrarian asks\x1b[0m {}", truncate(a.question.trim(), 2000));
        let answer = self.0.lock().await.next("\x1b[1;33manswer›\x1b[0m ").await;
        Ok(match answer {
            Read::Message(m) if !m.trim().is_empty() => format!("the user answered: {m}"),
            _ => "the user did not answer; carry on without it and mention the open question in your reply".into(),
        })
    }
}
