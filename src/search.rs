//! Finding things: exact `grep` (regex crate) and ranked `search` (BM25).
//!
//! Both work on the files as they are on disk, re-read on every call; a
//! personal wiki is small enough that this is instant and nothing can go stale.
//!
//! Room for a semantic index: [`chunks`] is the single place that decides what
//! a searchable unit is (a page header, or a top-level section with its page
//! title). A future background task can watch the data folder, embed those
//! same chunks into `librarians_office/` and answer nearest-neighbour queries;
//! [`Search`] would then fuse its ranking with BM25's (reciprocal rank fusion)
//! without the model seeing a different tool.

use bm25::{Document, Language, SearchEngineBuilder};
use regex::RegexBuilder;
use serde::Deserialize;

use crate::page::{HEADER_LINES, Page, sections};
use crate::sandbox::{LIBRARY, Sandbox, trace, truncate};
use crate::tools::tool;

/// One searchable unit.
#[derive(Clone, Debug)]
pub struct Chunk {
    pub path: String,
    /// 1-based line in the file where the chunk starts.
    pub line: usize,
    pub title: String,
    /// The section heading, or "(header)".
    pub heading: String,
    pub text: String,
}

/// Splits every markdown file under `scope` into chunks: for a page, its
/// header (title, aliases, tags, summary, plus any intro text) and each
/// top-level (`#`/`##`) section with everything under it.
pub fn chunks(sb: &Sandbox, scope: &str) -> Result<Vec<Chunk>, String> {
    let mut out = Vec::new();
    for path in sb.files(scope)?.into_iter().filter(|p| p.ends_with(".md")) {
        let text = sb.read(&path)?;
        let (page, offset) = if path.starts_with(LIBRARY) {
            (Page::parse(&text, &path).0, HEADER_LINES)
        } else {
            let p = Page { header: Default::default(), body: text.clone() };
            (p, 0)
        };
        let lines = page.body_lines();
        let h = &page.header;
        let title = if h.title.is_empty() { path.clone() } else { h.title.clone() };
        let secs: Vec<_> = sections(&lines).into_iter().filter(|s| s.level <= 2).collect();
        let first = secs.iter().find(|s| s.level == 2).map(|s| s.start).unwrap_or(lines.len());
        // Title twice: matches on the name should outrank passing mentions.
        let intro = lines[..first].join("\n");
        out.push(Chunk {
            path: path.clone(),
            line: 1,
            title: title.clone(),
            heading: "(header)".into(),
            text: format!("{title}\n{title}\n{}\n{}\n{}\n{intro}", h.aliases, h.tags, h.summary),
        });
        for s in secs.iter().filter(|s| s.level == 2) {
            out.push(Chunk {
                path: path.clone(),
                line: s.start + offset + 1,
                title: title.clone(),
                heading: s.title.clone(),
                text: format!("{title}\n{}", lines[s.start..s.end].join("\n")),
            });
        }
    }
    Ok(out)
}

fn snippet(chunk: &Chunk) -> String {
    let line = chunk
        .text
        .lines()
        .skip(1)
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#') && *l != chunk.title)
        .unwrap_or("");
    truncate(line, 160).replace('\n', " ")
}

// ---------------------------------------------------------------------------

pub struct Search(pub Sandbox);

#[derive(Deserialize)]
pub struct SearchArgs {
    query: String,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

impl Search {
    fn run(&self, a: SearchArgs) -> Result<String, String> {
        let scope = a.folder.filter(|f| !f.trim().is_empty()).unwrap_or_else(|| LIBRARY.into());
        let scope = self.0.clean(&scope)?;
        trace(format!("search \"{}\" in {scope}", a.query));
        let chunks = chunks(&self.0, &scope)?;
        let docs = chunks.iter().enumerate().map(|(i, c)| Document::new(i, c.text.clone()));
        let engine = SearchEngineBuilder::<usize>::with_documents(Language::English, docs).build();
        let limit = a.limit.unwrap_or(8).clamp(1, 30);
        let hits = engine.search(&a.query, limit);
        if hits.is_empty() {
            return Ok(format!("no results for \"{}\" in {scope}; try other words or grep", a.query));
        }
        let mut out = vec![format!("top {} results for \"{}\" in {scope}", hits.len(), a.query)];
        for (n, hit) in hits.iter().enumerate() {
            let c = &chunks[hit.document.id];
            out.push(format!(
                "{}. {}:{} — {} › {} (score {:.1})\n   {}",
                n + 1,
                c.path,
                c.line,
                c.title,
                c.heading,
                hit.score,
                snippet(c)
            ));
        }
        Ok(out.join("\n"))
    }
}

tool!(Search, SearchArgs, "search", read_only: true,
    "Ranked full-text search (BM25 with stemming) over page headers and sections. Best for questions in natural words; results name the page, section and line. Use grep for exact strings.",
    {"type":"object","required":["query"],"properties":{
        "query":{"type":"string","description":"Several words, synonyms welcome: 'bread sourdough starter recipe'"},
        "folder":{"type":"string","description":"Default 'library'"},
        "limit":{"type":"integer","description":"Default 8"}}});

// ---------------------------------------------------------------------------

pub struct Grep(pub Sandbox);

#[derive(Deserialize)]
pub struct GrepArgs {
    pattern: String,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    files_only: bool,
    #[serde(default)]
    max_hits: Option<usize>,
}

impl Grep {
    fn run(&self, a: GrepArgs) -> Result<String, String> {
        let scope = a.folder.filter(|f| !f.trim().is_empty()).unwrap_or_else(|| LIBRARY.into());
        let scope = self.0.clean(&scope)?;
        trace(format!("grep {:?} in {scope}", a.pattern));
        let source = if a.regex { a.pattern.clone() } else { regex::escape(&a.pattern) };
        let re = RegexBuilder::new(&source)
            .case_insensitive(!a.case_sensitive)
            .size_limit(1 << 20)
            .build()
            .map_err(|e| format!("bad pattern: {e}"))?;
        let context = a.context.unwrap_or(0).min(5);
        let max = a.max_hits.unwrap_or(60).clamp(1, 300);
        let (mut hits, mut files, mut body) = (0usize, 0usize, Vec::new());
        for path in self.0.files(&scope)? {
            let Ok(text) = self.0.read(&path) else { continue };
            let lines: Vec<&str> = text.lines().collect();
            let matched: Vec<usize> = (0..lines.len()).filter(|&i| re.is_match(lines[i])).collect();
            if matched.is_empty() {
                continue;
            }
            files += 1;
            hits += matched.len();
            if a.files_only {
                body.push(format!("{path} ({} matching lines)", matched.len()));
                continue;
            }
            let mut last_shown = None;
            for &i in &matched {
                if body.len() >= max * (1 + 2 * context) {
                    break;
                }
                let from = i.saturating_sub(context);
                let to = (i + context).min(lines.len() - 1);
                for (j, line) in lines.iter().enumerate().take(to + 1).skip(from) {
                    if last_shown.is_some_and(|l| j <= l) {
                        continue;
                    }
                    let mark = if j == i { ':' } else { '-' };
                    body.push(format!("{path}{mark}{}{mark} {}", j + 1, truncate(line, 240)));
                    last_shown = Some(j);
                }
            }
        }
        if hits == 0 {
            return Ok(format!("no lines match {:?} in {scope}", a.pattern));
        }
        let shown = body.len();
        let mut out = vec![format!("{hits} matching lines in {files} files for {:?} in {scope}", a.pattern)];
        out.extend(body);
        if !a.files_only && hits > max {
            out.push(format!("… showing about {max} of {hits}; narrow the pattern or use files_only ({shown} lines shown)"));
        }
        Ok(out.join("\n"))
    }
}

tool!(Grep, GrepArgs, "grep", read_only: true,
    "Find exact text in files, like grep -rn: case-insensitive literal match by default, or a regular expression with regex:true. Shows path:line: text, optionally with context lines.",
    {"type":"object","required":["pattern"],"properties":{
        "pattern":{"type":"string"},
        "regex":{"type":"boolean","description":"Treat pattern as a regular expression (Rust regex syntax)"},
        "case_sensitive":{"type":"boolean"},
        "folder":{"type":"string","description":"Default 'library'; 'librarians_office' or a subfolder also work"},
        "context":{"type":"integer","description":"Lines of context, 0-5"},
        "files_only":{"type":"boolean","description":"Only list matching files"},
        "max_hits":{"type":"integer"}}});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::test_sandbox;

    fn page(sb: &Sandbox, path: &str, title: &str, summary: &str, body: &str) {
        let p = Page {
            header: crate::page::Header { title: title.into(), summary: summary.into(), ..Default::default() },
            body: body.into(),
        };
        sb.write(path, &p.render(), "created").unwrap();
    }

    #[test]
    fn bm25_ranks_and_grep_matches() {
        let sb = test_sandbox("search");
        page(&sb, "library/food/sourdough.md", "Sourdough", "How the user bakes bread.",
            "# Sourdough\n\n## Starter\n\nFeed the starter daily with rye flour.\n\n## Baking\n\nBake loaves at 250C in a dutch oven.\n");
        page(&sb, "library/people/ada.md", "Ada", "A mathematician.", "# Ada\n\nShe liked baked goods.\n");
        let r = Search(sb.clone()).run(SearchArgs { query: "baking loaves oven".into(), folder: None, limit: None }).unwrap();
        let first = r.lines().nth(1).unwrap();
        assert!(first.contains("library/food/sourdough.md:") && first.contains("Baking"), "{r}");

        let r = Grep(sb.clone())
            .run(GrepArgs {
                pattern: "rye|dutch".into(),
                regex: true,
                case_sensitive: false,
                folder: None,
                context: Some(1),
                files_only: false,
                max_hits: None,
            })
            .unwrap();
        assert!(r.starts_with("2 matching lines in 1 files"), "{r}");
        assert!(r.contains("library/food/sourdough.md:13: Feed the starter"), "{r}");
        let bad = Grep(sb.clone()).run(GrepArgs {
            pattern: "x".into(),
            regex: false,
            case_sensitive: false,
            folder: Some("/etc".into()),
            context: None,
            files_only: false,
            max_hits: None,
        });
        assert!(bad.is_err());
    }
}
