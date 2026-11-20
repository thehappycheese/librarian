//! Wiki pages: the fixed-length header, sections, and relative links.
//!
//! Every page starts with exactly [`HEADER_LINES`] lines of front matter, in
//! this order, one line per field, each at most [`FIELD_MAX`] characters:
//!
//! ```text
//! ---
//! title: Ada Lovelace
//! summary: English mathematician; wrote the first published computer program.
//! aliases: Augusta Ada King, Countess of Lovelace
//! tags: people, computing-history
//! see_also: library/people/charles-babbage.md, library/topics/analytical-engine.md
//! updated: 2026-11-19
//! ---
//! ```
//!
//! Because the size is fixed, reading the first 8 lines of any page gives its
//! header, which is what `head` does. Tools render the header from fields, so
//! the model never has to format it, and `updated` is set automatically.
//! Pages written by hand without a header are read leniently and get one the
//! next time a tool writes them.

use std::sync::LazyLock;

use regex::Regex;

pub const HEADER_LINES: usize = 8;
pub const FIELD_MAX: usize = 200;
const FIELDS: [&str; 6] = ["title", "summary", "aliases", "tags", "see_also", "updated"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Header {
    pub title: String,
    pub summary: String,
    /// Comma-separated.
    pub aliases: String,
    /// Comma-separated.
    pub tags: String,
    /// Comma-separated paths like `library/people/ada.md`.
    pub see_also: String,
    pub updated: String,
}

impl Header {
    fn get_mut(&mut self, key: &str) -> Option<&mut String> {
        Some(match key {
            "title" => &mut self.title,
            "summary" => &mut self.summary,
            "aliases" => &mut self.aliases,
            "tags" => &mut self.tags,
            "see_also" => &mut self.see_also,
            "updated" => &mut self.updated,
            _ => return None,
        })
    }

    fn values(&self) -> [&str; 6] {
        [&self.title, &self.summary, &self.aliases, &self.tags, &self.see_also, &self.updated]
    }

    pub fn render(&self) -> String {
        let mut out = String::from("---\n");
        for (key, value) in FIELDS.iter().zip(self.values()) {
            let value = one_line(value);
            if value.is_empty() {
                out.push_str(&format!("{key}:\n"));
            } else {
                out.push_str(&format!("{key}: {value}\n"));
            }
        }
        out.push_str("---\n");
        out
    }

    /// Checks field lengths; tools call this so the model learns to keep the
    /// header short instead of having it silently cut.
    pub fn validate(&self) -> Result<(), String> {
        if one_line(&self.title).is_empty() {
            return Err("title must not be empty".into());
        }
        for (key, value) in FIELDS.iter().zip(self.values()) {
            let n = one_line(value).chars().count();
            if n > FIELD_MAX {
                return Err(format!("header field '{key}' is {n} characters; the limit is {FIELD_MAX}"));
            }
        }
        Ok(())
    }

    pub fn see_also_list(&self) -> Vec<String> {
        split_list(&self.see_also)
    }

    pub fn set_see_also(&mut self, items: Vec<String>) {
        self.see_also = join_list(items);
    }
}

/// Splits a comma-separated header list.
pub fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

/// Joins a header list, dropping duplicates while keeping order.
pub fn join_list(items: impl IntoIterator<Item = String>) -> String {
    let mut seen = Vec::new();
    for item in items {
        if !item.is_empty() && !seen.contains(&item) {
            seen.push(item);
        }
    }
    seen.join(", ")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub header: Header,
    /// Everything after the header.
    pub body: String,
}

impl Page {
    /// Parses a page. The flag is false when the file had no valid fixed
    /// header; a header is then made up from what is there.
    pub fn parse(text: &str, path: &str) -> (Page, bool) {
        let lines: Vec<&str> = text.lines().collect();
        if let Some(page) = Self::parse_strict(&lines) {
            return (page, true);
        }
        // Lenient: any front matter, fields in any order.
        let mut header = Header::default();
        let mut body_start = 0;
        if lines.first() == Some(&"---")
            && let Some(end) = lines.iter().skip(1).take(30).position(|l| *l == "---")
        {
            for line in &lines[1..=end] {
                if let Some((k, v)) = line.split_once(':')
                    && let Some(slot) = header.get_mut(k.trim())
                {
                    *slot = v.trim().to_string();
                }
            }
            body_start = end + 2;
        }
        let body = lines.get(body_start..).unwrap_or_default().join("\n");
        if header.title.is_empty() {
            header.title = body
                .lines()
                .find_map(|l| l.strip_prefix("# "))
                .map(str::trim)
                .map(String::from)
                .unwrap_or_else(|| {
                    let stem = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".md");
                    stem.replace(['-', '_'], " ")
                });
        }
        (Page { header, body: with_newline(body) }, false)
    }

    fn parse_strict(lines: &[&str]) -> Option<Page> {
        if lines.len() < HEADER_LINES || lines[0] != "---" || lines[HEADER_LINES - 1] != "---" {
            return None;
        }
        let mut header = Header::default();
        for (i, key) in FIELDS.iter().enumerate() {
            let line = lines[i + 1];
            let rest = line.strip_prefix(key)?.strip_prefix(':')?;
            *header.get_mut(key)? = rest.trim().to_string();
        }
        let body = lines[HEADER_LINES..].join("\n");
        Some(Page { header, body: with_newline(body) })
    }

    pub fn render(&self) -> String {
        format!("{}{}", self.header.render(), with_newline(self.body.clone()))
    }

    pub fn body_lines(&self) -> Vec<String> {
        self.body.lines().map(String::from).collect()
    }

    pub fn set_body_lines(&mut self, lines: Vec<String>) {
        self.body = with_newline(lines.join("\n"));
    }
}

fn with_newline(mut s: String) -> String {
    while s.ends_with("\n\n") {
        s.pop();
    }
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// 1-based line number in the file of a 0-based body line.
pub fn file_line(body_index: usize) -> usize {
    body_index + HEADER_LINES + 1
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

/// A heading and everything under it, up to the next heading of the same or a
/// higher level. Indices are 0-based body lines; `end` is exclusive.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub level: usize,
    pub title: String,
    pub start: usize,
    pub end: usize,
}

fn heading(line: &str) -> Option<(usize, String)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) || !line[hashes..].starts_with(' ') {
        return None;
    }
    Some((hashes, line[hashes..].trim().trim_end_matches('#').trim().to_string()))
}

fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

pub fn sections<S: AsRef<str>>(lines: &[S]) -> Vec<Section> {
    let mut heads = Vec::new();
    let mut in_fence = false;
    for (i, line) in lines.iter().enumerate() {
        let line = line.as_ref();
        if is_fence(line) {
            in_fence = !in_fence;
        } else if !in_fence && let Some((level, title)) = heading(line) {
            heads.push((i, level, title));
        }
    }
    heads
        .iter()
        .enumerate()
        .map(|(n, (start, level, title))| {
            let end = heads[n + 1..]
                .iter()
                .find(|(_, l, _)| l <= level)
                .map(|(i, _, _)| *i)
                .unwrap_or(lines.len());
            Section { level: *level, title: title.clone(), start: *start, end }
        })
        .collect()
}

/// Finds a section by heading text (case-insensitive, leading `#`s ignored).
/// `"Heading@42"` picks the one starting on file line 42 when titles repeat.
pub fn find_section<S: AsRef<str>>(lines: &[S], query: &str) -> Result<Section, String> {
    let all = sections(lines);
    let (name, at) = match query.rsplit_once('@') {
        Some((n, l)) if l.trim().parse::<usize>().is_ok() => (n, l.trim().parse::<usize>().ok()),
        _ => (query, None),
    };
    let name = name.trim().trim_start_matches('#').trim().to_lowercase();
    let hits: Vec<&Section> = all
        .iter()
        .filter(|s| s.title.to_lowercase() == name && at.is_none_or(|l| file_line(s.start) == l))
        .collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => {
            let list: Vec<String> = all.iter().take(40).map(|s| format!("{} (line {})", s.title, file_line(s.start))).collect();
            Err(format!(
                "no heading '{query}'. Headings: {}",
                if list.is_empty() { "(none)".into() } else { list.join("; ") }
            ))
        }
        many => {
            let list: Vec<String> = many.iter().map(|s| format!("{}@{}", s.title, file_line(s.start))).collect();
            Err(format!("'{query}' is ambiguous; use one of: {}", list.join(", ")))
        }
    }
}

/// Shifts heading levels (outside code fences), clamped to 1..=6.
pub fn shift_headings(lines: &[String], delta: isize) -> Vec<String> {
    let mut in_fence = false;
    lines
        .iter()
        .map(|line| {
            if is_fence(line) {
                in_fence = !in_fence;
                return line.clone();
            }
            match heading(line) {
                Some((level, title)) if !in_fence => {
                    let new = (level as isize + delta).clamp(1, 6) as usize;
                    format!("{} {title}", "#".repeat(new))
                }
                _ => line.clone(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\]\(([^)\s]+)\)").unwrap());

/// Folder of a data-relative path: `library/people/a.md` → `library/people`.
pub fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// Lexically resolves `target` against `dir`; `None` if it climbs out.
pub fn resolve(dir: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for p in target.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

/// Relative path from folder `from_dir` to data-relative `to`.
pub fn relative(from_dir: &str, to: &str) -> String {
    let a: Vec<&str> = from_dir.split('/').filter(|p| !p.is_empty()).collect();
    let b: Vec<&str> = to.split('/').filter(|p| !p.is_empty()).collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out: Vec<&str> = vec![".."; a.len() - common];
    out.extend(&b[common..]);
    out.join("/")
}

/// Calls `f` with the resolved target of every relative `.md` link in `text`
/// (as seen from `dir`); when `f` returns a new data-relative target, the link
/// is rewritten relative to `new_dir`. Returns the text and how many changed.
pub fn map_links(text: &str, dir: &str, new_dir: &str, f: impl Fn(&str) -> Option<String>) -> (String, usize) {
    let mut changed = 0;
    let out = LINK.replace_all(text, |caps: &regex::Captures| {
        let whole = caps[0].to_string();
        let raw = &caps[1];
        let (path, anchor) = match raw.split_once('#') {
            Some((p, a)) => (p, format!("#{a}")),
            None => (raw, String::new()),
        };
        if path.is_empty() || path.contains("://") || path.starts_with('/') || !path.ends_with(".md") {
            return whole;
        }
        let Some(abs) = resolve(dir, path) else { return whole };
        let target = f(&abs);
        if target.is_none() && dir == new_dir {
            return whole;
        }
        let target = target.unwrap_or(abs);
        let new = format!("]({}{anchor})", relative(new_dir, &target));
        if new != whole {
            changed += 1;
        }
        new
    });
    (out.into_owned(), changed)
}

/// Data-relative targets of every relative `.md` link in `text`.
pub fn outgoing_links(text: &str, dir: &str) -> Vec<String> {
    let found = std::cell::RefCell::new(Vec::new());
    map_links(text, dir, dir, |abs| {
        found.borrow_mut().push(abs.to_string());
        None
    });
    found.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_fixed_length_and_round_trips() {
        let page = Page {
            header: Header { title: "Ada".into(), summary: "A person.".into(), ..Header::default() },
            body: "# Ada\n\nText\n".into(),
        };
        let text = page.render();
        assert_eq!(text.lines().take(HEADER_LINES).last(), Some("---"));
        assert_eq!(text.lines().nth(HEADER_LINES), Some("# Ada"));
        let (back, valid) = Page::parse(&text, "library/ada.md");
        assert!(valid);
        assert_eq!(back, page);
    }

    #[test]
    fn lenient_parse_of_hand_written_pages() {
        let (p, valid) = Page::parse("# Bread\n\nFlour and water.\n", "library/food/bread.md");
        assert!(!valid);
        assert_eq!(p.header.title, "Bread");
        assert!(p.body.starts_with("# Bread"));
        let (p, _) = Page::parse("---\ntags: x\ntitle: T\n---\nbody\n", "library/t.md");
        assert_eq!((p.header.title.as_str(), p.header.tags.as_str(), p.body.as_str()), ("T", "x", "body\n"));
    }

    #[test]
    fn sections_nest_and_skip_code() {
        let lines = ["# T", "intro", "## A", "a", "### A1", "x", "```", "## not a heading", "```", "## B", "b"];
        let s = sections(&lines);
        let a = find_section(&lines, "## a").unwrap();
        assert_eq!((a.start, a.end), (2, 9));
        assert_eq!(s.iter().filter(|s| s.title == "not a heading").count(), 0);
        assert!(find_section(&lines, "nope").unwrap_err().contains("Headings:"));
    }

    #[test]
    fn links_resolve_and_rewrite() {
        assert_eq!(relative("library/people", "library/topics/x.md"), "../topics/x.md");
        assert_eq!(resolve("library/people", "../topics/x.md").unwrap(), "library/topics/x.md");
        let text = "See [B](bob.md#early) and [web](https://x.y/a.md) and [T](../topics/t.md).";
        // Page moved from library/people to library: its own links are rebased.
        let (moved, n) = map_links(text, "library/people", "library", |_| None);
        assert_eq!(moved, "See [B](people/bob.md#early) and [web](https://x.y/a.md) and [T](topics/t.md).");
        assert_eq!(n, 2);
        // bob.md was renamed: links to it are retargeted.
        let (re, n) = map_links(text, "library/people", "library/people", |abs| {
            (abs == "library/people/bob.md").then(|| "library/people/robert.md".to_string())
        });
        assert!(re.contains("[B](robert.md#early)") && n == 1);
        assert_eq!(outgoing_links(text, "library/people"), ["library/people/bob.md", "library/topics/t.md"]);
    }
}
