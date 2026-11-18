//! The librarian's hands: plain-Rust file tools, all sandboxed to `./data`.
//!
//! Every path the model gives is relative to the data root and must start with
//! `library/` (the wiki) or `librarians_office/` (the agent's private cache).
//! Absolute paths, `..`, and symlinks pointing outside are refused. Writes are
//! limited to human-readable text formats, and every change is appended to
//! `librarians_office/changelog.md` by the tool itself, so the record does not
//! depend on the model remembering to keep it.
//!
//! Tools never return `Err`: failures come back as `ERROR: ...` text so the
//! model can read them and correct itself.

use std::{
    convert::Infallible,
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
};

use rig_core::{
    completion::ToolAnnotations,
    tool::{Tool, ToolContext},
};
use serde::Deserialize;
use serde_json::{Value, json};

pub const LIBRARY: &str = "library";
pub const OFFICE: &str = "librarians_office";
const WRITABLE_EXTENSIONS: &[&str] = &["md", "txt", "csv"];
const MAX_READ_LINES: usize = 400;
const MAX_LIST_ENTRIES: usize = 500;
const MAX_SEARCH_HITS: usize = 80;

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

/// The data root (`./data`) and the rules for touching anything inside it.
#[derive(Clone)]
pub struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// Creates `./data/library` and `./data/librarians_office` if needed and
    /// guarantees the wiki's entry point `library/index.md` exists.
    pub fn open(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let root = root.as_ref();
        fs::create_dir_all(root.join(LIBRARY))?;
        fs::create_dir_all(root.join(OFFICE))?;
        let sandbox = Self { root: fs::canonicalize(root)? };
        let index = sandbox.root.join(LIBRARY).join("index.md");
        if !index.exists() {
            fs::write(
                &index,
                "# Library index\n\nThis wiki is empty so far. The librarian will grow it as you \
                 share information in author mode.\n",
            )?;
        }
        Ok(sandbox)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a model-supplied relative path to an absolute one inside the
    /// sandbox. `allow_root` permits the empty path (the data root itself) for
    /// listing and searching.
    pub fn resolve(&self, rel: &str, allow_root: bool) -> Result<PathBuf, String> {
        let rel = rel.trim().trim_start_matches("./").trim_end_matches('/');
        if rel.is_empty() || rel == "." {
            return if allow_root {
                Ok(self.root.clone())
            } else {
                Err("a path is required, e.g. library/index.md".into())
            };
        }
        let path = Path::new(rel);
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => parts.push(part),
                Component::CurDir => {}
                _ => {
                    return Err(format!(
                        "'{rel}' is not allowed: use plain relative paths starting with \
                         {LIBRARY}/ or {OFFICE}/ (no '/', '..' or '~' prefixes)"
                    ));
                }
            }
        }
        match parts.first().and_then(|p| p.to_str()) {
            Some(LIBRARY) | Some(OFFICE) => {}
            _ => return Err(format!("'{rel}' must start with {LIBRARY}/ or {OFFICE}/")),
        }
        let full = self.root.join(path);
        // Follow symlinks on the deepest existing ancestor and make sure we are
        // still inside the data root.
        let mut probe = full.clone();
        while !probe.exists() {
            if !probe.pop() {
                break;
            }
        }
        let real = fs::canonicalize(&probe).map_err(|e| format!("cannot resolve '{rel}': {e}"))?;
        if !real.starts_with(&self.root) {
            return Err(format!("'{rel}' points outside the data folder"));
        }
        Ok(full)
    }

    /// Like [`resolve`](Self::resolve) but also requires a writable text extension.
    fn resolve_writable(&self, rel: &str) -> Result<PathBuf, String> {
        let full = self.resolve(rel, false)?;
        let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !WRITABLE_EXTENSIONS.contains(&ext) {
            return Err(format!(
                "'{rel}': only human-readable files may be written ({})",
                WRITABLE_EXTENSIONS.join(", ")
            ));
        }
        Ok(full)
    }

    /// The path relative to the data root, for display and logs.
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Appends one line to `librarians_office/changelog.md`.
    fn log_change(&self, what: &str) {
        let path = self.root.join(OFFICE).join("changelog.md");
        let fresh = !path.exists();
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            if fresh {
                let _ = writeln!(f, "# Changelog\n\nEvery change made to the data folder, newest last.\n");
            }
            let _ = writeln!(f, "- {} — {what}", now_utc());
        }
    }

    /// Text of `library/index.md`, capped, for putting into the agent's context.
    pub fn index_snapshot(&self, max_chars: usize) -> String {
        let text = fs::read_to_string(self.root.join(LIBRARY).join("index.md")).unwrap_or_default();
        truncate(&text, max_chars)
    }

    /// Text of an office file (if any), capped.
    pub fn office_snapshot(&self, name: &str, max_chars: usize) -> Option<String> {
        fs::read_to_string(self.root.join(OFFICE).join(name))
            .ok()
            .map(|t| truncate(&t, max_chars))
    }
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{cut}\n… [truncated]")
}

/// Prints what a tool is doing, dimmed, so the user can watch the librarian work.
pub fn trace(msg: impl AsRef<str>) {
    eprintln!("\x1b[2m  · {}\x1b[0m", msg.as_ref());
}

fn read_only() -> ToolAnnotations {
    ToolAnnotations { read_only_hint: Some(true), ..Default::default() }
}

fn err(msg: impl std::fmt::Display) -> Result<String, Infallible> {
    Ok(format!("ERROR: {msg}"))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if out.len() >= MAX_LIST_ENTRIES * 4 {
            return;
        }
        // Never follow symlinked directories.
        let is_dir = fs::symlink_metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        out.push(path.clone());
        if is_dir {
            walk(&path, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Read-only tools (both modes)
// ---------------------------------------------------------------------------

pub struct ListFiles(pub Sandbox);

#[derive(Deserialize)]
pub struct ListArgs {
    #[serde(default)]
    path: String,
}

impl Tool for ListFiles {
    const NAME: &'static str = "list_files";
    type Args = ListArgs;
    type Output = String;
    type Error = Infallible;

    fn description(&self) -> String {
        "Recursively list files under a folder (default: whole data root) with their sizes.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string","description":"Folder, e.g. 'library' or 'library/people'. Empty = everything."}
        }})
    }
    fn annotations(&self) -> ToolAnnotations {
        read_only()
    }
    async fn call(&self, _: &mut ToolContext, args: ListArgs) -> Result<String, Infallible> {
        trace(format!("list {}", if args.path.is_empty() { "." } else { &args.path }));
        let dir = match self.0.resolve(&args.path, true) {
            Ok(d) => d,
            Err(e) => return err(e),
        };
        if !dir.is_dir() {
            return err(format!("'{}' is not a folder", args.path));
        }
        let mut paths = Vec::new();
        walk(&dir, &mut paths);
        let total = paths.len();
        let mut out = String::new();
        for p in paths.iter().take(MAX_LIST_ENTRIES) {
            let shown = self.0.display(p);
            if p.is_dir() {
                out.push_str(&format!("{shown}/\n"));
            } else {
                let size = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                out.push_str(&format!("{shown}  ({size} B)\n"));
            }
        }
        if total > MAX_LIST_ENTRIES {
            out.push_str(&format!("… {} more entries; list a subfolder\n", total - MAX_LIST_ENTRIES));
        }
        Ok(if out.is_empty() { "(empty)".into() } else { out })
    }
}

pub struct ReadFile(pub Sandbox);

#[derive(Deserialize)]
pub struct ReadArgs {
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    max_lines: Option<usize>,
}

impl Tool for ReadFile {
    const NAME: &'static str = "read_file";
    type Args = ReadArgs;
    type Output = String;
    type Error = Infallible;

    fn description(&self) -> String {
        format!("Read a text file. Returns up to {MAX_READ_LINES} lines unless max_lines says otherwise.")
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","required":["path"],"properties":{
            "path":{"type":"string","description":"e.g. library/index.md"},
            "start_line":{"type":"integer","description":"1-based first line (default 1)"},
            "max_lines":{"type":"integer"}
        }})
    }
    fn annotations(&self) -> ToolAnnotations {
        read_only()
    }
    async fn call(&self, _: &mut ToolContext, a: ReadArgs) -> Result<String, Infallible> {
        trace(format!("read {}", a.path));
        let path = match self.0.resolve(&a.path, false) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => return err(format!("cannot read '{}': {e}", a.path)),
        };
        let lines: Vec<&str> = text.lines().collect();
        let start = a.start_line.unwrap_or(1).max(1);
        let take = a.max_lines.unwrap_or(MAX_READ_LINES).clamp(1, 2000);
        let end = (start - 1 + take).min(lines.len());
        if start > lines.len().max(1) {
            return err(format!("'{}' has only {} lines", a.path, lines.len()));
        }
        let body = lines[start - 1..end].join("\n");
        Ok(format!("[{} — lines {}-{} of {}]\n{body}", a.path, start, end, lines.len()))
    }
}

pub struct Search(pub Sandbox);

#[derive(Deserialize)]
pub struct SearchArgs {
    query: String,
    #[serde(default)]
    path: String,
}

impl Tool for Search {
    const NAME: &'static str = "search";
    type Args = SearchArgs;
    type Output = String;
    type Error = Infallible;

    fn description(&self) -> String {
        "Case-insensitive search over file names and lines. A line matches when it contains \
         every word of the query. Returns path:line: text."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","required":["query"],"properties":{
            "query":{"type":"string","description":"One or more words; all must appear on the line."},
            "path":{"type":"string","description":"Folder to search, default 'library'."}
        }})
    }
    fn annotations(&self) -> ToolAnnotations {
        read_only()
    }
    async fn call(&self, _: &mut ToolContext, a: SearchArgs) -> Result<String, Infallible> {
        let scope = if a.path.trim().is_empty() { LIBRARY.to_string() } else { a.path.clone() };
        trace(format!("search \"{}\" in {scope}", a.query));
        let dir = match self.0.resolve(&scope, true) {
            Ok(d) => d,
            Err(e) => return err(e),
        };
        let words: Vec<String> = a.query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return err("empty query");
        }
        let mut files = Vec::new();
        walk(&dir, &mut files);
        let mut hits = Vec::new();
        for f in files.iter().filter(|p| p.is_file()) {
            let shown = self.0.display(f);
            let lower_name = shown.to_lowercase();
            if words.iter().all(|w| lower_name.contains(w)) {
                hits.push(format!("{shown}: (file name matches)"));
            }
            let Ok(text) = fs::read_to_string(f) else { continue };
            for (i, line) in text.lines().enumerate() {
                let lower = line.to_lowercase();
                if words.iter().all(|w| lower.contains(w)) {
                    hits.push(format!("{shown}:{}: {}", i + 1, truncate(line.trim(), 200)));
                }
            }
        }
        if hits.is_empty() {
            return Ok(format!("no matches for \"{}\" in {scope}", a.query));
        }
        let total = hits.len();
        let mut out = hits.into_iter().take(MAX_SEARCH_HITS).collect::<Vec<_>>().join("\n");
        if total > MAX_SEARCH_HITS {
            out.push_str(&format!("\n… {} more hits; narrow the query", total - MAX_SEARCH_HITS));
        }
        Ok(out)
    }
}

/// Append-only notes in the office. Available in retrieval mode so the
/// librarian can record gaps ("user asked X, the wiki has nothing").
pub struct OfficeNote(pub Sandbox);

#[derive(Deserialize)]
pub struct NoteArgs {
    file: String,
    text: String,
}

impl Tool for OfficeNote {
    const NAME: &'static str = "office_note";
    type Args = NoteArgs;
    type Output = String;
    type Error = Infallible;

    fn description(&self) -> String {
        format!(
            "Append a timestamped note to a markdown file in {OFFICE}/ (e.g. 'open_questions.md', \
             'todo.md'). Use it to record knowledge gaps and follow-ups."
        )
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","required":["file","text"],"properties":{
            "file":{"type":"string","description":"File name inside the office, e.g. open_questions.md"},
            "text":{"type":"string"}
        }})
    }
    async fn call(&self, _: &mut ToolContext, a: NoteArgs) -> Result<String, Infallible> {
        let name = a.file.trim().trim_start_matches(&format!("{OFFICE}/")).to_string();
        trace(format!("note → {OFFICE}/{name}"));
        let rel = format!("{OFFICE}/{name}");
        let path = match self.0.resolve_writable(&rel) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let result = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "- {} — {}", now_utc(), a.text.trim()));
        match result {
            Ok(()) => Ok(format!("noted in {rel}")),
            Err(e) => err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Writing tools (author mode only)
// ---------------------------------------------------------------------------

pub struct WriteFile(pub Sandbox);

#[derive(Deserialize)]
pub struct WriteArgs {
    path: String,
    content: String,
}

impl Tool for WriteFile {
    const NAME: &'static str = "write_file";
    type Args = WriteArgs;
    type Output = String;
    type Error = Infallible;

    fn description(&self) -> String {
        "Create or completely overwrite a text file (.md/.txt/.csv). Folders are created as \
         needed. Prefer edit_file for small changes to existing pages."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","required":["path","content"],"properties":{
            "path":{"type":"string","description":"e.g. library/people/ada-lovelace.md"},
            "content":{"type":"string","description":"Full file contents."}
        }})
    }
    async fn call(&self, _: &mut ToolContext, a: WriteArgs) -> Result<String, Infallible> {
        trace(format!("write {} ({} chars)", a.path, a.content.len()));
        let path = match self.0.resolve_writable(&a.path) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let existed = path.exists();
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            return err(e);
        }
        let mut content = a.content;
        if !content.ends_with('\n') {
            content.push('\n');
        }
        match fs::write(&path, &content) {
            Ok(()) => {
                let verb = if existed { "rewrote" } else { "created" };
                self.0.log_change(&format!("{verb} {}", self.0.display(&path)));
                Ok(format!("{verb} {} ({} lines)", a.path, content.lines().count()))
            }
            Err(e) => err(e),
        }
    }
}

pub struct EditFile(pub Sandbox);

#[derive(Deserialize)]
pub struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
}

impl Tool for EditFile {
    const NAME: &'static str = "edit_file";
    type Args = EditArgs;
    type Output = String;
    type Error = Infallible;

    fn description(&self) -> String {
        "Replace one exact occurrence of old_text with new_text in an existing file. old_text \
         must appear exactly once; include surrounding lines to make it unique."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","required":["path","old_text","new_text"],"properties":{
            "path":{"type":"string"},
            "old_text":{"type":"string"},
            "new_text":{"type":"string"}
        }})
    }
    async fn call(&self, _: &mut ToolContext, a: EditArgs) -> Result<String, Infallible> {
        trace(format!("edit {}", a.path));
        let path = match self.0.resolve_writable(&a.path) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => return err(format!("cannot read '{}': {e}", a.path)),
        };
        if a.old_text.is_empty() {
            return err("old_text is empty");
        }
        match text.matches(&a.old_text).count() {
            0 => err(format!("old_text not found in '{}'; read the file again", a.path)),
            1 => match fs::write(&path, text.replacen(&a.old_text, &a.new_text, 1)) {
                Ok(()) => {
                    self.0.log_change(&format!("edited {}", self.0.display(&path)));
                    Ok(format!("edited {}", a.path))
                }
                Err(e) => err(e),
            },
            n => err(format!("old_text occurs {n} times in '{}'; make it unique", a.path)),
        }
    }
}

pub struct MoveFile(pub Sandbox);

#[derive(Deserialize)]
pub struct MoveArgs {
    from: String,
    to: String,
}

impl Tool for MoveFile {
    const NAME: &'static str = "move_file";
    type Args = MoveArgs;
    type Output = String;
    type Error = Infallible;

    fn description(&self) -> String {
        "Move or rename a file. Refuses to overwrite. Remember to fix links that pointed to it."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","required":["from","to"],"properties":{
            "from":{"type":"string"},"to":{"type":"string"}
        }})
    }
    async fn call(&self, _: &mut ToolContext, a: MoveArgs) -> Result<String, Infallible> {
        trace(format!("move {} → {}", a.from, a.to));
        let (from, to) = match (self.0.resolve_writable(&a.from), self.0.resolve_writable(&a.to)) {
            (Ok(f), Ok(t)) => (f, t),
            (Err(e), _) | (_, Err(e)) => return err(e),
        };
        if !from.is_file() {
            return err(format!("'{}' does not exist", a.from));
        }
        if to.exists() {
            return err(format!("'{}' already exists", a.to));
        }
        if let Some(parent) = to.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match fs::rename(&from, &to) {
            Ok(()) => {
                self.0.log_change(&format!("moved {} → {}", self.0.display(&from), self.0.display(&to)));
                Ok(format!("moved {} → {}", a.from, a.to))
            }
            Err(e) => err(e),
        }
    }
}

/// "Deleting" moves the file into `librarians_office/trash/` so nothing is lost.
pub struct TrashFile(pub Sandbox);

#[derive(Deserialize)]
pub struct TrashArgs {
    path: String,
}

impl Tool for TrashFile {
    const NAME: &'static str = "trash_file";
    type Args = TrashArgs;
    type Output = String;
    type Error = Infallible;

    fn description(&self) -> String {
        format!("Remove a file from the wiki by moving it to {OFFICE}/trash/ (recoverable).")
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}})
    }
    async fn call(&self, _: &mut ToolContext, a: TrashArgs) -> Result<String, Infallible> {
        trace(format!("trash {}", a.path));
        let from = match self.0.resolve(&a.path, false) {
            Ok(p) => p,
            Err(e) => return err(e),
        };
        if !from.is_file() {
            return err(format!("'{}' is not a file", a.path));
        }
        if self.0.display(&from) == format!("{LIBRARY}/index.md") {
            return err("the index is the wiki's entry point and cannot be removed; edit it instead");
        }
        let trash = self.0.root.join(OFFICE).join("trash");
        let _ = fs::create_dir_all(&trash);
        let stamp = now_utc().replace([':', ' '], "-");
        let flat = self.0.display(&from).replace('/', "__");
        let to = trash.join(format!("{stamp}__{flat}"));
        match fs::rename(&from, &to) {
            Ok(()) => {
                self.0.log_change(&format!("trashed {} → {}", self.0.display(&from), self.0.display(&to)));
                Ok(format!("moved {} to {}", a.path, self.0.display(&to)))
            }
            Err(e) => err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Time without a date crate
// ---------------------------------------------------------------------------

/// Current UTC time as `YYYY-MM-DD HH:MMZ`.
pub fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}Z", rem / 3600, (rem % 3600) / 60)
}

pub fn today_utc() -> String {
    now_utc()[..10].to_string()
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> (Sandbox, PathBuf) {
        let dir = std::env::temp_dir().join(format!("librarian-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (Sandbox::open(&dir).unwrap(), dir)
    }

    #[test]
    fn sandbox_rules() {
        let (sb, _dir) = sandbox();
        assert!(sb.resolve("library/index.md", false).is_ok());
        assert!(sb.resolve("./librarians_office/x.md", false).is_ok());
        assert!(sb.resolve("", true).is_ok());
        assert!(sb.resolve("", false).is_err());
        assert!(sb.resolve("/etc/passwd", false).is_err());
        assert!(sb.resolve("library/../../etc/passwd", false).is_err());
        assert!(sb.resolve("src/main.rs", false).is_err());
        assert!(sb.resolve_writable("library/a.md").is_ok());
        assert!(sb.resolve_writable("library/a.sh").is_err());
        assert!(sb.root().join("library/index.md").exists());
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_775), (2026, 11, 18));
    }
}
