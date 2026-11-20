# librarian

An LLM librarian that keeps a markdown wiki. You talk to it in the terminal;
it files what you tell it into `./data/library`, answers questions from it,
reorganises it, and asks you when it is unsure.

```
cargo run --release
```

If `ANTHROPIC_API_KEY` (or `OPENAI_API_KEY`) is set it's used. Otherwise, in the chat:
`/model anthropic/claude-sonnet-5-5`, then `/key sk-...` (kept in memory only).
There is no config file. Everything is set from the chat:

- `/budget [turns] [tokens]`: per-message limits (default 30 turns, 300,000 tokens).
  When one runs out, the work done so far is saved; say "continue".
- `/model`, `/key`, `/forget`, `/status`, `/help`, `/quit`.
- **Pasting:** paste any multi-line lump and press Enter on an empty line to send it.
  To type several lines by hand, open and close with a line containing only `"""`.
- Ctrl-C interrupts a run.

The last 20 exchanges are kept as conversation memory; the wiki is the long-term memory.

## Pages

Every page starts with a fixed 8-line header, so `head` on any page returns exactly its header:

```
---
title: Ada Lovelace
summary: English mathematician; wrote the first published computer program.
aliases: Augusta Ada King, Countess of Lovelace
tags: people, computing-history
see_also: library/people/charles-babbage.md
updated: 2026-11-19
---
# Ada Lovelace
...
```

The tools write the header from fields and set `updated`; the model never formats it by hand.
Hand-written pages without a header still work and get one the next time a tool writes them.

## Tools (all plain Rust, no shell)

| finding | writing | restructuring | other |
|---|---|---|---|
| `list_pages` (title + summary, flags orphans) | `write_page` | `split_section` | `office_write` |
| `search` (BM25, stemmed, per section) | `set_header` | `move_section` | `ask_user` |
| `grep` (literal or regex) | `edit_section` | `merge_pages` | `trash` |
| `head` (batch headers) | `edit_text` | `move_page` | |
| `outline`, `read` (whole page or one section) | | | |

The restructuring tools fix relative links and `see_also` across the wiki.

Inside a run, tool results older than the last two rounds are shrunk to their first line,
and long arguments the model already sent (e.g. a page body) are shrunk too, so a long run
doesn't resend everything every turn.

## Files (all plain text)

```
data/
  library/              the wiki; entry point library/index.md (always exists)
  librarians_office/    the agent's own space
    changelog.md        every file change, written by the tools themselves
    chat_log.md         the transcript
    todo.md, style.md   the agent's plan and conventions (fed back into its context)
    glossary.md, open_questions.md, …
    trash/              removed and merged-away pages land here
```

## Code

- `src/main.rs`: chat loop, commands, model connection, budgets
- `src/input.rs`: terminal input; lines that arrive together (a paste) become one message
- `src/librarian.rs`: instructions, tool set, hooks (shrinking old results, token budget)
- `src/sandbox.rs`: path rules (only `library/` and `librarians_office/`, no `..` or symlink escapes), writes, changelog
- `src/page.rs`: the fixed header, sections, link resolving and rewriting
- `src/tools.rs`: reading, office, trash, ask_user
- `src/mdtools.rs`: page writing and restructuring
- `src/search.rs`: chunking, BM25 `search`, regex `grep`

`cargo test` drives the real agent loop and tools with rig's scripted mock model, so it needs no key.

Built on `rig-core` + `rig-agent` 0.46 (needs rustc ≥ 1.97.1), `bm25` and `regex`.
