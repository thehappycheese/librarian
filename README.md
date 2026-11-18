# librarian

An LLM librarian that keeps a markdown wiki. You talk to it in the terminal;
it files what you tell it into `./data/library` and answers questions from it.

```
cargo run --release
```

If `ANTHROPIC_API_KEY` (or `OPENAI_API_KEY`) is set it's used. Otherwise, in the chat:
`/model anthropic/claude-sonnet-5-5`, then `/key sk-...` (kept in memory only).
There is no config file. Everything else is set from the chat too.

## Modes

| | author | retrieval (default) |
|---|---|---|
| switch | `/author [turns]` or "author mode" | `/ask [turns]` or "retrieval mode" |
| budget | 40 model turns per message | 6 model turns per message |
| tools | read, search, bash, **write, edit, move, trash**, office notes | read, search, bash, office notes |

When author mode runs out of budget, the work done so far stays on disk; say
"continue". Ctrl-C interrupts a run.

## Files (all plain text)

```
data/
  library/              the wiki; entry point library/index.md (always exists)
  librarians_office/    the agent's own space
    changelog.md        every file change, written by the tools themselves
    chat_log.md         the transcript
    todo.md, style.md,  the agent's notes (todo.md and style.md are fed back
    open_questions.md   into author mode's context)
    trash/              "deleted" pages land here
```

## Code

- `src/main.rs`: chat loop, commands, model connection, turn budgets
- `src/librarian.rs`: the instructions for each mode, the tool sets, the agent
- `src/tools.rs`: sandboxed file tools in plain Rust (paths must start with `library/` or `librarians_office/`; no `..`, absolute paths or symlink escapes; writes only `.md/.txt/.csv`)
- `src/bashtools.rs`: **the only shell-out**. One read-only `bash` tool. It only allows `ls, cat, head, tail, wc, grep, find, sort, uniq, cut, tr, diff, nl` in pipes, with no redirects or substitution, and blocks the options that write or execute. It runs in `./data` with an empty environment and a 10 s timeout.

`cargo test` drives the real agent loop and tools with rig's scripted mock model, so it needs no key.

Built on `rig-core` + `rig-agent` 0.46 (needs rustc ≥ 1.97.1).
