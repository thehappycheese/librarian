//! The data folder and the rules for touching anything inside it.
//!
//! Every path the model gives is relative to `./data` and must start with
//! `library/` (the wiki) or `librarians_office/` (the agent's private cache).
//! Absolute paths, `..`, and symlinks pointing outside are refused. Every change
//! is appended to `librarians_office/changelog.md` by the tools themselves.

use std::{
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
};

use crate::page::{Header, Page};

pub const LIBRARY: &str = "library";
pub const OFFICE: &str = "librarians_office";
pub const INDEX: &str = "library/index.md";
const OFFICE_EXTENSIONS: &[&str] = &["md", "txt", "csv"];

#[derive(Clone)]
pub struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// Creates `library/` and `librarians_office/` if needed and guarantees the
    /// wiki's entry point `library/index.md` exists.
    pub fn open(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let root = root.as_ref();
        fs::create_dir_all(root.join(LIBRARY))?;
        fs::create_dir_all(root.join(OFFICE))?;
        let sandbox = Self { root: fs::canonicalize(root)? };
        let index = sandbox.root.join(INDEX);
        if !index.exists() {
            let page = Page {
                header: Header {
                    title: "Library index".into(),
                    summary: "Entry point of the wiki: every page, grouped by area.".into(),
                    tags: "index".into(),
                    updated: today_utc(),
                    ..Header::default()
                },
                body: "# Library index\n\nThe wiki is empty so far. The librarian grows it as you share information.\n"
                    .into(),
            };
            fs::write(&index, page.render())?;
        }
        Ok(sandbox)
    }

    /// Normalises a model-supplied path to `library/...` or `librarians_office/...`
    /// (forward slashes, no `./`), or explains why it is not allowed.
    pub fn clean(&self, rel: &str) -> Result<String, String> {
        let rel = rel.trim().trim_start_matches("./").trim_end_matches('/');
        if rel.is_empty() {
            return Err("a path is required, e.g. library/index.md".into());
        }
        let mut parts = Vec::new();
        for component in Path::new(rel).components() {
            match component {
                Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
                Component::CurDir => {}
                _ => {
                    return Err(format!(
                        "'{rel}' is not allowed: use plain relative paths starting with {LIBRARY}/ or {OFFICE}/"
                    ));
                }
            }
        }
        match parts.first().map(String::as_str) {
            Some(LIBRARY) | Some(OFFICE) => Ok(parts.join("/")),
            _ => Err(format!("'{rel}' must start with {LIBRARY}/ or {OFFICE}/")),
        }
    }

    /// Absolute path for a cleaned relative path, refusing symlink escapes.
    pub fn abs(&self, rel: &str) -> Result<PathBuf, String> {
        let rel = self.clean(rel)?;
        let full = self.root.join(&rel);
        let mut probe = full.clone();
        while !probe.exists() && probe.pop() {}
        let real = fs::canonicalize(&probe).map_err(|e| format!("cannot resolve '{rel}': {e}"))?;
        if !real.starts_with(&self.root) {
            return Err(format!("'{rel}' points outside the data folder"));
        }
        Ok(full)
    }

    /// A wiki page path: inside `library/` and ending in `.md`.
    pub fn page_path(&self, rel: &str) -> Result<String, String> {
        let rel = self.clean(rel)?;
        if !rel.starts_with(&format!("{LIBRARY}/")) || !rel.ends_with(".md") {
            return Err(format!("'{rel}' is not a wiki page: pages are library/**/*.md"));
        }
        Ok(rel)
    }

    /// A file in the office with a human-readable extension.
    pub fn office_path(&self, rel: &str) -> Result<String, String> {
        let rel = rel.trim().trim_start_matches("./");
        let rel = if rel.starts_with(OFFICE) { rel.to_string() } else { format!("{OFFICE}/{rel}") };
        let rel = self.clean(&rel)?;
        let ext = rel.rsplit('.').next().unwrap_or("");
        if !OFFICE_EXTENSIONS.contains(&ext) || !rel.contains('.') {
            return Err(format!("office files must be {}", OFFICE_EXTENSIONS.join(", ")));
        }
        Ok(rel)
    }

    pub fn read(&self, rel: &str) -> Result<String, String> {
        let path = self.abs(rel)?;
        fs::read_to_string(&path).map_err(|e| format!("cannot read '{rel}': {e}"))
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.abs(rel).map(|p| p.exists()).unwrap_or(false)
    }

    /// Writes a file (creating folders) and records the change.
    pub fn write(&self, rel: &str, content: &str, what: &str) -> Result<(), String> {
        let path = self.abs(rel)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut content = content.to_string();
        if !content.ends_with('\n') {
            content.push('\n');
        }
        fs::write(&path, content).map_err(|e| format!("cannot write '{rel}': {e}"))?;
        self.log_change(&format!("{what} {rel}"));
        Ok(())
    }

    /// Moves a file to `librarians_office/trash/` and returns where it went.
    pub fn trash(&self, rel: &str) -> Result<String, String> {
        let from = self.abs(rel)?;
        let stamp = now_utc().replace([':', ' '], "-");
        let to_rel = format!("{OFFICE}/trash/{stamp}__{}", self.clean(rel)?.replace('/', "__"));
        let to = self.abs(&to_rel)?;
        fs::create_dir_all(to.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::rename(&from, &to).map_err(|e| e.to_string())?;
        self.log_change(&format!("trashed {rel} → {to_rel}"));
        Ok(to_rel)
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let (a, b) = (self.abs(from)?, self.abs(to)?);
        if b.exists() {
            return Err(format!("'{to}' already exists"));
        }
        fs::create_dir_all(b.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::rename(a, b).map_err(|e| e.to_string())?;
        self.log_change(&format!("moved {from} → {to}"));
        Ok(())
    }

    pub fn append(&self, rel: &str, text: &str) -> Result<(), String> {
        let path = self.abs(rel)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "{text}"))
            .map_err(|e| e.to_string())
    }

    fn log_change(&self, what: &str) {
        let rel = format!("{OFFICE}/changelog.md");
        if !self.exists(&rel) {
            let _ = self.append(&rel, "# Changelog\n\nEvery change to the data folder, newest last.\n");
        }
        let _ = self.append(&rel, &format!("- {} — {what}", now_utc()));
    }

    /// All files under a folder (relative paths, sorted), never following symlinks.
    pub fn files(&self, folder: &str) -> Result<Vec<String>, String> {
        let dir = self.abs(folder)?;
        let mut out = Vec::new();
        walk(&dir, &mut out);
        let mut rels: Vec<String> = out
            .into_iter()
            .filter_map(|p| p.strip_prefix(&self.root).ok().map(|r| r.to_string_lossy().replace('\\', "/")))
            .filter(|r| !r.starts_with(&format!("{OFFICE}/trash/")))
            .collect();
        rels.sort();
        Ok(rels)
    }

    /// Every wiki page (`library/**/*.md`).
    pub fn pages(&self) -> Vec<String> {
        self.files(LIBRARY).unwrap_or_default().into_iter().filter(|p| p.ends_with(".md")).collect()
    }

    /// Text of a file, capped, for the agent's context; `None` if missing.
    pub fn snapshot(&self, rel: &str, max_chars: usize) -> Option<String> {
        self.read(rel).ok().map(|t| truncate(&t, max_chars))
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        if meta.is_dir() {
            walk(&path, out);
        } else if meta.is_file() {
            out.push(path);
        }
    }
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}\n… [truncated]", &text[..cut]),
    }
}

/// Prints what a tool is doing, dimmed, so the user can watch the librarian work.
pub fn trace(msg: impl AsRef<str>) {
    eprintln!("\x1b[2m  · {}\x1b[0m", msg.as_ref());
}

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
pub fn test_sandbox(name: &str) -> Sandbox {
    let dir = std::env::temp_dir().join(format!("librarian-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    Sandbox::open(&dir).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_rules() {
        let sb = test_sandbox("paths");
        assert_eq!(sb.clean("./library/a/b.md").unwrap(), "library/a/b.md");
        assert!(sb.clean("/etc/passwd").is_err());
        assert!(sb.clean("library/../../x").is_err());
        assert!(sb.clean("src/main.rs").is_err());
        assert!(sb.page_path("library/a.md").is_ok());
        assert!(sb.page_path("library/a.txt").is_err());
        assert!(sb.page_path("librarians_office/a.md").is_err());
        assert_eq!(sb.office_path("todo.md").unwrap(), "librarians_office/todo.md");
        assert!(sb.office_path("run.sh").is_err());
        assert!(sb.exists(INDEX));
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_775), (2026, 11, 18));
    }
}
