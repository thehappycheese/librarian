//! Tools that change wiki pages without re-emitting whole files.
//!
//! - `write_page` creates a page from header fields + body (Rust renders the
//!   fixed header).
//! - `set_header`, `edit_text`, `edit_section` make small changes in place.
//! - `split_section`, `move_section`, `merge_pages`, `move_page` restructure
//!   the wiki; they rebase relative links in moved text and retarget links
//!   elsewhere in the wiki, so the model doesn't have to.
//!
//! Every write sets `updated` and checks the header fits the fixed format.

use serde::Deserialize;

use crate::page::{
    self, Header, Page, Section, dir_of, find_section, join_list, map_links, relative, shift_headings, split_list,
};
use crate::sandbox::{INDEX, Sandbox, today_utc, trace};
use crate::tools::{load, tool};

/// Validates and writes a page, bumping `updated`.
fn save(sb: &Sandbox, path: &str, mut page: Page, what: &str) -> Result<(), String> {
    page.header.updated = today_utc();
    page.header.validate()?;
    let lines = tidy_blank_lines(page.body_lines());
    page.set_body_lines(lines);
    sb.write(path, &page.render(), what)
}

/// Collapses runs of blank lines (outside code fences) to one and trims the ends.
fn tidy_blank_lines(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut fence = false;
    for line in lines {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fence = !fence;
        }
        if !fence && line.trim().is_empty() && out.last().is_none_or(|l| l.trim().is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out
}

fn lines_of(text: &str) -> Vec<String> {
    text.trim_matches('\n').lines().map(String::from).collect()
}

/// Inserts `new` at `at` with a blank line on either side.
fn insert_block(lines: &mut Vec<String>, at: usize, new: Vec<String>) {
    let mut block = vec![String::new()];
    block.extend(new);
    block.push(String::new());
    lines.splice(at..at, block);
}

fn clean_see_also(sb: &Sandbox, list: &str) -> Result<String, String> {
    let items: Result<Vec<String>, String> = split_list(list).iter().map(|p| sb.page_path(p)).collect();
    Ok(join_list(items?))
}

/// Notes about see_also targets that don't exist yet (not an error: the page
/// may be written next).
fn missing_targets(sb: &Sandbox, header: &Header) -> String {
    let missing: Vec<String> = header.see_also_list().into_iter().filter(|p| !sb.exists(p)).collect();
    if missing.is_empty() { String::new() } else { format!("\nnote: see_also targets not written yet: {}", missing.join(", ")) }
}

/// Retargets links to `old` (body links and see_also) in every other page.
pub fn retarget_everywhere(sb: &Sandbox, old: &str, new: &str, skip: &[&str]) -> Result<Vec<String>, String> {
    let mut touched = Vec::new();
    for p in sb.pages() {
        if p == old || skip.contains(&p.as_str()) {
            continue;
        }
        let (mut page, _) = Page::parse(&sb.read(&p)?, &p);
        let dir = dir_of(&p).to_string();
        let (body, n) = map_links(&page.body, &dir, &dir, |abs| (abs == old).then(|| new.to_string()));
        let see_before = page.header.see_also.clone();
        let see: Vec<String> = page
            .header
            .see_also_list()
            .into_iter()
            .map(|s| if s == old { new.to_string() } else { s })
            .filter(|s| s != &p)
            .collect();
        page.header.set_see_also(see);
        if n > 0 || page.header.see_also != see_before {
            page.body = body;
            save(sb, &p, page, "relinked")?;
            touched.push(p);
        }
    }
    Ok(touched)
}

fn relinked_note(touched: &[String]) -> String {
    if touched.is_empty() { String::new() } else { format!("\nlinks updated in: {}", touched.join(", ")) }
}

// ---------------------------------------------------------------------------

pub struct WritePage(pub Sandbox);

#[derive(Deserialize)]
pub struct WritePageArgs {
    path: String,
    title: String,
    summary: String,
    #[serde(default)]
    aliases: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    see_also: String,
    body: String,
    #[serde(default)]
    overwrite: bool,
}

impl WritePage {
    fn run(&self, a: WritePageArgs) -> Result<String, String> {
        let path = self.0.page_path(&a.path)?;
        trace(format!("write page {path}"));
        let existed = self.0.exists(&path);
        if existed && !a.overwrite {
            return Err(format!(
                "'{path}' exists. Change it with edit_section/edit_text/set_header, or pass overwrite:true to replace it entirely"
            ));
        }
        let header = Header {
            title: a.title,
            summary: a.summary,
            aliases: a.aliases,
            tags: a.tags,
            see_also: clean_see_also(&self.0, &a.see_also)?,
            updated: String::new(),
        };
        let note = missing_targets(&self.0, &header);
        let body = a.body.replace("\r\n", "\n");
        let page = Page { header, body };
        save(&self.0, &path, page, if existed { "rewrote" } else { "created" })?;
        Ok(format!("{} {path}{note}", if existed { "rewrote" } else { "created" }))
    }
}

tool!(WritePage, WritePageArgs, "write_page", read_only: false,
    format!("Create a page (or replace one with overwrite:true). You give the header fields and the markdown body; the fixed {}-line header is written for you. Each header field is one line of at most {} characters. Start the body with '# Title'.", page::HEADER_LINES, page::FIELD_MAX),
    {"type":"object","required":["path","title","summary","body"],"properties":{
        "path":{"type":"string","description":"library/<area>/<kebab-name>.md"},
        "title":{"type":"string"},
        "summary":{"type":"string","description":"One sentence: what the page covers"},
        "aliases":{"type":"string","description":"Comma-separated other names people might search for"},
        "tags":{"type":"string","description":"Comma-separated"},
        "see_also":{"type":"string","description":"Comma-separated related page paths, e.g. library/people/bob.md"},
        "body":{"type":"string"},
        "overwrite":{"type":"boolean"}}});

// ---------------------------------------------------------------------------

pub struct SetHeader(pub Sandbox);

#[derive(Deserialize)]
pub struct SetHeaderArgs {
    path: String,
    title: Option<String>,
    summary: Option<String>,
    aliases: Option<String>,
    tags: Option<String>,
    see_also: Option<String>,
    #[serde(default)]
    add_see_also: Option<String>,
}

impl SetHeader {
    fn run(&self, a: SetHeaderArgs) -> Result<String, String> {
        trace(format!("set header {}", a.path));
        let (path, mut page) = load(&self.0, &a.path)?;
        let h = &mut page.header;
        for (slot, value) in [
            (&mut h.title, a.title),
            (&mut h.summary, a.summary),
            (&mut h.aliases, a.aliases),
            (&mut h.tags, a.tags),
        ] {
            if let Some(v) = value {
                *slot = v;
            }
        }
        if let Some(v) = a.see_also {
            h.see_also = clean_see_also(&self.0, &v)?;
        }
        if let Some(v) = a.add_see_also {
            let extra = clean_see_also(&self.0, &v)?;
            let mut list = h.see_also_list();
            list.extend(split_list(&extra));
            h.set_see_also(list);
        }
        let note = missing_targets(&self.0, &page.header);
        let rendered = page.header.render();
        save(&self.0, &path, page, "set header of")?;
        Ok(format!("header of {path} is now:\n{rendered}{note}"))
    }
}

tool!(SetHeader, SetHeaderArgs, "set_header", read_only: false,
    "Change some header fields of a page without touching its body. Omitted fields stay as they are. add_see_also appends to see_also. Also repairs pages that have no header.",
    {"type":"object","required":["path"],"properties":{
        "path":{"type":"string"},"title":{"type":"string"},"summary":{"type":"string"},
        "aliases":{"type":"string"},"tags":{"type":"string"},
        "see_also":{"type":"string","description":"Replaces the list"},
        "add_see_also":{"type":"string","description":"Comma-separated paths to add"}}});

// ---------------------------------------------------------------------------

pub struct EditText(pub Sandbox);

#[derive(Deserialize)]
pub struct EditTextArgs {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    all: bool,
}

impl EditText {
    fn run(&self, a: EditTextArgs) -> Result<String, String> {
        trace(format!("edit {}", a.path));
        if a.old_text.is_empty() {
            return Err("old_text is empty".into());
        }
        let is_page = self.0.page_path(&a.path).is_ok();
        let (path, text) = if is_page {
            let (p, page) = load(&self.0, &a.path)?;
            (p, page.body)
        } else {
            let p = self.0.office_path(&a.path)?;
            let t = self.0.read(&p)?;
            (p, t)
        };
        let count = text.matches(&a.old_text).count();
        let new = match (count, a.all) {
            (0, _) => {
                return Err(format!(
                    "old_text not found in {path}{}",
                    if is_page { " (the header is not editable here; use set_header)" } else { "" }
                ));
            }
            (1, _) | (_, true) => text.replace(&a.old_text, &a.new_text),
            (n, false) => return Err(format!("old_text occurs {n} times; add context or pass all:true")),
        };
        if is_page {
            let (_, mut page) = load(&self.0, &path)?;
            page.body = new;
            save(&self.0, &path, page, "edited")?;
        } else {
            self.0.write(&path, &new, "edited")?;
        }
        Ok(format!("edited {path} ({count} replacement{})", if count == 1 { "" } else { "s" }))
    }
}

tool!(EditText, EditTextArgs, "edit_text", read_only: false,
    "Replace exact text in a page body or an office file. old_text must occur exactly once unless all:true.",
    {"type":"object","required":["path","old_text","new_text"],"properties":{
        "path":{"type":"string"},"old_text":{"type":"string"},"new_text":{"type":"string"},
        "all":{"type":"boolean"}}});

// ---------------------------------------------------------------------------

pub struct EditSection(pub Sandbox);

#[derive(Deserialize)]
pub struct EditSectionArgs {
    path: String,
    action: String,
    #[serde(default)]
    heading: Option<String>,
    #[serde(default)]
    content: String,
    #[serde(default)]
    new_heading: Option<String>,
}

impl EditSection {
    fn run(&self, a: EditSectionArgs) -> Result<String, String> {
        let (path, mut page) = load(&self.0, &a.path)?;
        let mut lines = page.body_lines();
        let content = lines_of(&a.content);
        let heading = a.heading.as_deref().map(str::trim).filter(|h| !h.is_empty());
        trace(format!("{} section {path} § {}", a.action, heading.unwrap_or("(end of page)")));
        let needs_content = matches!(a.action.as_str(), "replace" | "append" | "prepend" | "insert_after" | "insert_before");
        if needs_content && content.is_empty() && a.action != "replace" {
            return Err("content is required".into());
        }

        let Some(heading) = heading else {
            return match a.action.as_str() {
                "append" => {
                    let at = lines.len();
                    insert_block(&mut lines, at, content);
                    page.set_body_lines(lines);
                    save(&self.0, &path, page, "appended to")?;
                    Ok(format!("appended to the end of {path}"))
                }
                _ => Err("heading is required (only 'append' works without one: it adds to the end of the page)".into()),
            };
        };
        let s: Section = find_section(&lines, heading)?;
        let head_line = |title: &str| format!("{} {}", "#".repeat(s.level), title.trim().trim_start_matches('#').trim());
        let done = match a.action.as_str() {
            "replace" => {
                let mut new = vec![match &a.new_heading {
                    Some(h) => head_line(h),
                    None => lines[s.start].clone(),
                }];
                new.push(String::new());
                new.extend(content);
                new.push(String::new());
                lines.splice(s.start..s.end, new);
                "replaced the contents of"
            }
            "append" => {
                insert_block(&mut lines, s.end, content);
                "appended to"
            }
            "prepend" => {
                insert_block(&mut lines, s.start + 1, content);
                "prepended to"
            }
            "insert_after" => {
                insert_block(&mut lines, s.end, content);
                "inserted after"
            }
            "insert_before" => {
                insert_block(&mut lines, s.start, content);
                "inserted before"
            }
            "delete" => {
                lines.drain(s.start..s.end);
                "deleted"
            }
            "rename" => {
                let Some(h) = &a.new_heading else { return Err("new_heading is required".into()) };
                lines[s.start] = head_line(h);
                "renamed"
            }
            other => return Err(format!("unknown action '{other}'")),
        };
        page.set_body_lines(lines);
        save(&self.0, &path, page, "edited section of")?;
        Ok(format!("{done} section '{}' in {path}", s.title))
    }
}

tool!(EditSection, EditSectionArgs, "edit_section", read_only: false,
    "Change one section of a page (a heading and everything under it, subsections included) without rewriting the page. Actions: replace (new content under the same heading, or new_heading), append (end of the section; without a heading: end of the page), prepend (right under the heading), insert_after / insert_before (content placed next to the section; start it with its own heading), delete, rename (needs new_heading).",
    {"type":"object","required":["path","action"],"properties":{
        "path":{"type":"string"},
        "action":{"type":"string","enum":["replace","append","prepend","insert_after","insert_before","delete","rename"]},
        "heading":{"type":"string","description":"Heading text; 'Title@LINE' if it repeats"},
        "content":{"type":"string","description":"Markdown"},
        "new_heading":{"type":"string"}}});

// ---------------------------------------------------------------------------

pub struct SplitSection(pub Sandbox);

#[derive(Deserialize)]
pub struct SplitArgs {
    path: String,
    heading: String,
    new_path: String,
    summary: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    aliases: String,
    #[serde(default)]
    tags: Option<String>,
}

impl SplitSection {
    fn run(&self, a: SplitArgs) -> Result<String, String> {
        let (path, mut source) = load(&self.0, &a.path)?;
        let new_path = self.0.page_path(&a.new_path)?;
        trace(format!("split {path} § {} → {new_path}", a.heading));
        if self.0.exists(&new_path) {
            return Err(format!("'{new_path}' exists; use move_section to move into an existing page"));
        }
        let mut lines = source.body_lines();
        let s = find_section(&lines, &a.heading)?;
        let title = a.title.unwrap_or_else(|| s.title.clone());
        // Section body minus its heading; children become '##'.
        let moved = lines[s.start + 1..s.end].join("\n");
        let (moved, _) = map_links(&moved, dir_of(&path), dir_of(&new_path), |_| None);
        let moved = shift_headings(&lines_of(&moved), 1 - s.level as isize);
        let mut body = vec![format!("# {title}"), String::new()];
        body.extend(moved);
        let new_page = Page {
            header: Header {
                title: title.clone(),
                summary: a.summary,
                aliases: a.aliases,
                tags: a.tags.unwrap_or_else(|| source.header.tags.clone()),
                see_also: path.clone(),
                updated: String::new(),
            },
            body: body.join("\n"),
        };
        save(&self.0, &new_path, new_page, "created (split)")?;
        let pointer = vec![
            lines[s.start].clone(),
            String::new(),
            format!("See [{title}]({}).", relative(dir_of(&path), &new_path)),
            String::new(),
        ];
        lines.splice(s.start..s.end, pointer);
        source.set_body_lines(lines);
        let mut see = source.header.see_also_list();
        see.push(new_path.clone());
        source.header.set_see_also(see);
        save(&self.0, &path, source, "split")?;
        Ok(format!("moved section '{}' of {path} into new page {new_path}; left a link in its place", s.title))
    }
}

tool!(SplitSection, SplitArgs, "split_section", read_only: false,
    "Turn one section of a page into its own new page. The section is replaced by a link, headers and see_also are filled in, and relative links are fixed.",
    {"type":"object","required":["path","heading","new_path","summary"],"properties":{
        "path":{"type":"string"},"heading":{"type":"string"},
        "new_path":{"type":"string"},"summary":{"type":"string"},
        "title":{"type":"string","description":"Default: the heading"},
        "aliases":{"type":"string"},"tags":{"type":"string","description":"Default: the source page's tags"}}});

// ---------------------------------------------------------------------------

pub struct MoveSection(pub Sandbox);

#[derive(Deserialize)]
pub struct MoveSectionArgs {
    from: String,
    heading: String,
    to: String,
    #[serde(default)]
    after_heading: Option<String>,
    #[serde(default = "yes")]
    leave_pointer: bool,
}

fn yes() -> bool {
    true
}

impl MoveSection {
    fn run(&self, a: MoveSectionArgs) -> Result<String, String> {
        let (from, mut source) = load(&self.0, &a.from)?;
        let (to, mut target) = load(&self.0, &a.to)?;
        trace(format!("move § {} from {from} → {to}", a.heading));
        if from == to {
            return Err("from and to are the same page; use edit_section".into());
        }
        let mut src = source.body_lines();
        let s = find_section(&src, &a.heading)?;
        let mut tgt = target.body_lines();
        let (at, level) = match a.after_heading.as_deref().filter(|h| !h.trim().is_empty()) {
            Some(h) => {
                let t = find_section(&tgt, h)?;
                (t.end, t.level)
            }
            None => (tgt.len(), 2),
        };
        let block = src[s.start..s.end].join("\n");
        let (block, _) = map_links(&block, dir_of(&from), dir_of(&to), |_| None);
        let block = shift_headings(&lines_of(&block), level as isize - s.level as isize);
        insert_block(&mut tgt, at, block);
        target.set_body_lines(tgt);
        let replacement = if a.leave_pointer {
            vec![
                src[s.start].clone(),
                String::new(),
                format!("Moved to [{}]({}).", target.header.title, relative(dir_of(&from), &to)),
                String::new(),
            ]
        } else {
            Vec::new()
        };
        src.splice(s.start..s.end, replacement);
        source.set_body_lines(src);
        if a.leave_pointer {
            let mut see = source.header.see_also_list();
            see.push(to.clone());
            source.header.set_see_also(see);
        }
        save(&self.0, &to, target, "received section in")?;
        save(&self.0, &from, source, "moved section out of")?;
        Ok(format!("moved section '{}' from {from} to {to}", s.title))
    }
}

tool!(MoveSection, MoveSectionArgs, "move_section", read_only: false,
    "Move a section from one page into another existing page (at the end, or after after_heading), adjusting heading levels and relative links. By default a 'Moved to' link is left behind.",
    {"type":"object","required":["from","heading","to"],"properties":{
        "from":{"type":"string"},"heading":{"type":"string"},"to":{"type":"string"},
        "after_heading":{"type":"string"},"leave_pointer":{"type":"boolean"}}});

// ---------------------------------------------------------------------------

pub struct MergePages(pub Sandbox);

#[derive(Deserialize)]
pub struct MergeArgs {
    from: String,
    into: String,
}

impl MergePages {
    fn run(&self, a: MergeArgs) -> Result<String, String> {
        let (from, source) = load(&self.0, &a.from)?;
        let (into, mut target) = load(&self.0, &a.into)?;
        trace(format!("merge {from} → {into}"));
        if from == into || from == INDEX {
            return Err("cannot merge a page into itself or merge away the index".into());
        }
        let mut body = lines_of(&source.body);
        if body.first().is_some_and(|l| l.starts_with("# ")) {
            body.remove(0);
        }
        let (moved, _) = map_links(&body.join("\n"), dir_of(&from), dir_of(&into), |_| None);
        let mut block = vec![format!("## {}", source.header.title), String::new()];
        block.extend(shift_headings(&lines_of(&moved), 1));
        let mut lines = target.body_lines();
        let end = lines.len();
        insert_block(&mut lines, end, block);
        target.set_body_lines(lines);

        let h = &mut target.header;
        h.aliases = join_list(
            split_list(&h.aliases)
                .into_iter()
                .chain([source.header.title.clone()])
                .chain(split_list(&source.header.aliases)),
        );
        h.tags = join_list(split_list(&h.tags).into_iter().chain(split_list(&source.header.tags)));
        let see: Vec<String> = h
            .see_also_list()
            .into_iter()
            .chain(source.header.see_also_list())
            .filter(|p| p != &from && p != &into)
            .collect();
        h.set_see_also(see);
        save(&self.0, &into, target, "merged into")?;
        self.0.trash(&from)?;
        let touched = retarget_everywhere(&self.0, &from, &into, &[&into])?;
        Ok(format!(
            "appended {from} to {into} as section '{}', moved {from} to trash{}\nreview {into} and tidy duplicated text with edit_section",
            source.header.title,
            relinked_note(&touched)
        ))
    }
}

tool!(MergePages, MergeArgs, "merge_pages", read_only: false,
    "Fold one page into another: its body becomes a section of the target, aliases/tags/see_also are combined, the old page goes to trash and every link to it is pointed at the target. Tidy the merged text afterwards.",
    {"type":"object","required":["from","into"],"properties":{"from":{"type":"string"},"into":{"type":"string"}}});

// ---------------------------------------------------------------------------

pub struct MovePage(pub Sandbox);

#[derive(Deserialize)]
pub struct MovePageArgs {
    from: String,
    to: String,
}

impl MovePage {
    fn run(&self, a: MovePageArgs) -> Result<String, String> {
        let (from, mut page) = load(&self.0, &a.from)?;
        let to = self.0.page_path(&a.to)?;
        trace(format!("move page {from} → {to}"));
        if from == INDEX {
            return Err("the index must stay at library/index.md".into());
        }
        self.0.rename(&from, &to)?;
        let (body, _) = map_links(&page.body, dir_of(&from), dir_of(&to), |_| None);
        page.body = body;
        save(&self.0, &to, page, "rebased links in")?;
        let touched = retarget_everywhere(&self.0, &from, &to, &[&to])?;
        Ok(format!("moved {from} → {to}{}", relinked_note(&touched)))
    }
}

tool!(MovePage, MovePageArgs, "move_page", read_only: false,
    "Rename or move a page. Its own relative links and every link pointing to it are updated.",
    {"type":"object","required":["from","to"],"properties":{"from":{"type":"string"},"to":{"type":"string"}}});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::test_sandbox;

    fn write(sb: &Sandbox, path: &str, title: &str, body: &str) {
        let r = WritePage(sb.clone()).run(WritePageArgs {
            path: path.into(),
            title: title.into(),
            summary: format!("About {title}."),
            aliases: String::new(),
            tags: "t".into(),
            see_also: String::new(),
            body: body.into(),
            overwrite: false,
        });
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn restructuring_keeps_links_working() {
        let sb = test_sandbox("mdtools");
        write(&sb, "library/people/ada.md", "Ada", "# Ada\n\nFriend of [Charles](charles.md).\n\n## Early life\n\nBorn 1815.\n\n### School\n\nHome-schooled; see [maths](../topics/maths.md).\n\n## Work\n\nNotes on the engine.\n");
        write(&sb, "library/people/charles.md", "Charles", "# Charles\n\nWorked with [Ada](ada.md#work).\n");
        write(&sb, "library/topics/maths.md", "Maths", "# Maths\n\nSee [Ada](../people/ada.md).\n");

        // A section edit leaves the header intact and the rest untouched.
        let r = EditSection(sb.clone()).run(EditSectionArgs {
            path: "library/people/ada.md".into(),
            action: "append".into(),
            heading: Some("Work".into()),
            content: "Wrote the first program.".into(),
            new_heading: None,
        });
        assert!(r.is_ok(), "{r:?}");
        let text = sb.read("library/people/ada.md").unwrap();
        assert!(Page::parse(&text, "x").1, "header still valid");
        assert!(text.contains("Notes on the engine.\n\nWrote the first program."));

        // Split "Early life" into its own page in another folder.
        let r = SplitSection(sb.clone())
            .run(SplitArgs {
                path: "library/people/ada.md".into(),
                heading: "early life".into(),
                new_path: "library/bios/ada-early-life.md".into(),
                summary: "Ada's childhood.".into(),
                title: None,
                aliases: String::new(),
                tags: None,
            })
            .unwrap();
        assert!(r.contains("new page"));
        let early = sb.read("library/bios/ada-early-life.md").unwrap();
        assert!(early.contains("## School"), "subsection promoted: {early}");
        assert!(early.contains("[maths](../topics/maths.md)"), "link rebased: {early}");
        let ada = sb.read("library/people/ada.md").unwrap();
        assert!(ada.contains("See [Early life](../bios/ada-early-life.md)."));
        assert!(ada.contains("see_also: library/bios/ada-early-life.md"));

        // Move ada.md: links in charles.md and maths.md follow; its own links are rebased.
        let r = MovePage(sb.clone())
            .run(MovePageArgs { from: "library/people/ada.md".into(), to: "library/ada-lovelace.md".into() })
            .unwrap();
        assert!(r.contains("links updated in"), "{r}");
        assert!(sb.read("library/people/charles.md").unwrap().contains("[Ada](../ada-lovelace.md#work)"));
        assert!(sb.read("library/topics/maths.md").unwrap().contains("[Ada](../ada-lovelace.md)"));
        assert!(sb.read("library/ada-lovelace.md").unwrap().contains("[Charles](people/charles.md)"));

        // Merge charles into ada.
        MergePages(sb.clone())
            .run(MergeArgs { from: "library/people/charles.md".into(), into: "library/ada-lovelace.md".into() })
            .unwrap();
        let merged = sb.read("library/ada-lovelace.md").unwrap();
        assert!(merged.contains("## Charles") && merged.contains("aliases: Charles"));
        assert!(!sb.exists("library/people/charles.md"));
        // The self-link from charles ("Ada") now points within the merged page's folder.
        assert!(merged.contains("[Ada](ada-lovelace.md#work)"), "{merged}");
    }

    #[test]
    fn header_limits_are_enforced() {
        let sb = test_sandbox("limits");
        let r = WritePage(sb.clone()).run(WritePageArgs {
            path: "library/x.md".into(),
            title: "X".into(),
            summary: "long ".repeat(60),
            aliases: String::new(),
            tags: String::new(),
            see_also: "/etc/passwd".into(),
            body: "# X".into(),
            overwrite: false,
        });
        assert!(r.is_err());
        let r = EditText(sb.clone()).run(EditTextArgs {
            path: "library/index.md".into(),
            old_text: "title: Library index".into(),
            new_text: "title: hacked".into(),
            all: false,
        });
        assert!(r.unwrap_err().contains("set_header"));
    }
}
