//! THE ONLY PLACE THIS PROGRAM SHELLS OUT.
//!
//! One tool, `bash`, for quick read-only inspection of the data folder with
//! familiar Unix utilities (grep, find, wc, …). Everything the librarian needs
//! to *change* files goes through the plain-Rust tools in `tools.rs`; this
//! module only exists because `grep -c` or `find -newer` is sometimes the
//! fastest way for the model to get an overview.
//!
//! Guard rails, checked before `bash -c` ever runs:
//! - every pipeline stage must start with a command from [`ALLOWED`];
//! - no `;`, `&`, `>`, `<`, `$`, backticks, parentheses or newlines, so no
//!   chaining, redirection, substitution or subshells; `|` only as a pipe;
//! - no absolute paths, `..` or `~` in any argument;
//! - per-command bans for the options that write or execute (`find -exec`,
//!   `find -delete`, `sort -o`, …);
//! - runs in `./data` with an empty environment (only PATH), stdin closed, a
//!   10 second timeout, and output capped.

use std::{convert::Infallible, process::Stdio, time::Duration};

use rig_core::{
    completion::ToolAnnotations,
    tool::{Tool, ToolContext},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::{Sandbox, trace, truncate};

pub const ALLOWED: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "grep", "find", "sort", "uniq", "cut", "tr", "diff", "nl",
];
const FORBIDDEN_CHARS: &[char] = &[';', '&', '>', '<', '$', '`', '(', ')', '\n', '\r', '\\'];
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT_CHARS: usize = 12_000;

/// Returns `Err(reason)` when `command` is outside the allowed subset.
pub fn vet(command: &str) -> Result<(), String> {
    let command = command.trim();
    if command.is_empty() {
        return Err("empty command".into());
    }
    if let Some(c) = command.chars().find(|c| FORBIDDEN_CHARS.contains(c)) {
        return Err(format!("character {c:?} is not allowed (no chaining, redirects or substitution)"));
    }
    for stage in command.split('|') {
        let tokens: Vec<String> = stage
            .split_whitespace()
            .map(|t| t.trim_matches(|c| c == '\'' || c == '"').to_string())
            .collect();
        let Some(program) = tokens.first() else {
            return Err("empty pipeline stage ('||' is not allowed)".into());
        };
        if !ALLOWED.contains(&program.as_str()) {
            return Err(format!("'{program}' is not allowed; use one of: {}", ALLOWED.join(", ")));
        }
        for arg in &tokens[1..] {
            if arg.starts_with('/') || arg.starts_with('~') || arg.contains("..") {
                return Err(format!("argument '{arg}' leaves the data folder"));
            }
            // A '=' value can smuggle a path too, e.g. --exclude-from=/etc/x
            if let Some((_, value)) = arg.split_once('=')
                && (value.starts_with('/') || value.starts_with('~'))
            {
                return Err(format!("argument '{arg}' leaves the data folder"));
            }
        }
        let args = &tokens[1..];
        match program.as_str() {
            "find" => {
                const BAD: &[&str] =
                    &["-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf", "-fls"];
                if let Some(a) = args.iter().find(|a| BAD.contains(&a.as_str())) {
                    return Err(format!("find {a} is not allowed"));
                }
            }
            // `sort -o FILE` writes; refuse any short-option cluster containing 'o'.
            "sort" => {
                if args.iter().any(|a| {
                    a.starts_with("--output") || (a.starts_with('-') && !a.starts_with("--") && a.contains('o'))
                }) {
                    return Err("sort -o/--output is not allowed".into());
                }
            }
            // `uniq IN OUT` writes OUT; only options are allowed, input comes from the pipe.
            "uniq"
                if args.iter().any(|a| !a.starts_with('-')) => {
                    return Err("uniq only reads from a pipe here (options only, no file names)".into());
                }
            _ => {}
        }
    }
    Ok(())
}

pub struct Bash(pub Sandbox);

#[derive(Deserialize)]
pub struct BashArgs {
    command: String,
}

impl Tool for Bash {
    const NAME: &'static str = "bash";
    type Args = BashArgs;
    type Output = String;
    type Error = Infallible;

    fn description(&self) -> String {
        format!(
            "Run a read-only shell pipeline in the data folder (contains library/ and \
             librarians_office/). Allowed commands: {}. Pipes are fine; no redirects, ';', '&', \
             '$', absolute paths or '..'. Use it for overviews such as \
             `grep -ril term library | sort` or `find library -name '*.md' | wc -l`. \
             It cannot modify files.",
            ALLOWED.join(", ")
        )
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","required":["command"],"properties":{
            "command":{"type":"string"}
        }})
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations { read_only_hint: Some(true), ..Default::default() }
    }
    async fn call(&self, _: &mut ToolContext, a: BashArgs) -> Result<String, Infallible> {
        trace(format!("bash: {}", a.command));
        if let Err(reason) = vet(&a.command) {
            return Ok(format!("REFUSED: {reason}"));
        }
        let child = tokio::process::Command::new("bash")
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg(&a.command)
            .current_dir(self.0.root())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => return Ok(format!("ERROR: could not start bash: {e}")),
        };
        match tokio::time::timeout(TIMEOUT, child.wait_with_output()).await {
            Err(_) => Ok(format!("ERROR: timed out after {}s", TIMEOUT.as_secs())),
            Ok(Err(e)) => Ok(format!("ERROR: {e}")),
            Ok(Ok(out)) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stderr.trim().is_empty() {
                    text.push_str(&format!("\n[stderr]\n{stderr}"));
                }
                if text.trim().is_empty() {
                    text = "(no output)".into();
                }
                Ok(format!("[exit {}]\n{}", out.status.code().unwrap_or(-1), truncate(&text, MAX_OUTPUT_CHARS)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::vet;

    #[test]
    fn allows_read_only_pipelines() {
        assert!(vet("grep -ril bread library | sort").is_ok());
        assert!(vet("find library -name '*.md' | wc -l").is_ok());
        assert!(vet("cat library/index.md | grep -c '#'").is_ok());
        assert!(vet("sort -u -r").is_ok());
    }

    #[test]
    fn refuses_escapes_and_writes() {
        for bad in [
            "rm -rf library",
            "cat /etc/passwd",
            "cat library/../../secret",
            "ls ~",
            "ls; rm x",
            "ls && rm x",
            "echo hi > library/x.md",
            "cat $(which ls)",
            "cat `ls`",
            "find library -delete",
            "find library -exec rm {} +",
            "sort -o library/x.md library/y.md",
            "sort -uo out in",
            "uniq library/a.md library/b.md",
            "ls || ls",
            "grep --exclude-from=/etc/x a library",
            "bash -c ls",
        ] {
            assert!(vet(bad).is_err(), "should refuse: {bad}");
        }
    }
}
