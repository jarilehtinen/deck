//! `deck curate`: the Curated run. Starts Claude Code with the instructions in
//! `curate/prompt.md` (compiled in), lets it call `deck taste` and `deck curate submit`,
//! and writes its progress to the terminal and to `~/.cache/deck/curate.log`. launchd
//! runs this on Mondays (`scripts/install-curate.sh`). On failure the previous list
//! stays.

use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::{config, lists::CuratedFiles};

const PROMPT: &str = include_str!("../curate/prompt.md");
/// Set for Claude's process, so that a `deck curate` started inside the run refuses.
const INSIDE_RUN: &str = "DECK_CURATE_RUN";
/// Added to `PATH` after the inherited one, for runs with a narrow `PATH`.
const FALLBACK_PATH: [&str; 6] = [
    "~/.local/bin",
    "~/.cargo/bin",
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
];

pub fn run() -> Result<()> {
    if std::env::var_os(INSIDE_RUN).is_some() {
        bail!("deck curate cannot start inside a curate run: use deck curate submit");
    }
    let cache = config::private_cache_dir()?;
    let log = Log::open(&cache.join("curate.log"))?;
    let result = run_claude(&cache, &log);
    if let Err(e) = &result {
        log.line(&format!("=== curate: failed: {e:#}"));
    }
    result
}

fn run_claude(cache: &Path, log: &Log) -> Result<()> {
    // Claude's working directory: the candidate files are written here and stay until
    // the next run.
    let work = cache.join("curate");
    let home = config::home()?;
    let path = search_path(&home)?;
    let Some(claude) = find_in_path("claude", &path) else {
        bail!("claude not found (PATH={})", path.to_string_lossy());
    };
    std::fs::create_dir_all(&work).with_context(|| format!("cannot create {}", work.display()))?;
    remove_candidates(&work)?;

    let files = CuratedFiles::default_paths()?;
    let before = created(&files);
    log.line(&format!("=== curate: start ({})", claude.display()));

    let mut child = Command::new(&claude)
        .arg("-p")
        .arg(PROMPT)
        .args(["--permission-mode", "dontAsk", "--allowedTools"])
        .args(allowed_tools(&work))
        .args([
            "--strict-mcp-config",
            "--no-session-persistence",
            "--output-format",
            "stream-json",
            "--verbose",
        ])
        .current_dir(&work)
        .env("PATH", &path)
        .env(INSIDE_RUN, "1")
        // Lets the run start from a terminal where Claude Code is already running.
        .env_remove("CLAUDECODE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start {}", claude.display()))?;

    let stdout = child.stdout.take().context("no stdout from claude")?;
    let stderr = child.stderr.take().context("no stderr from claude")?;
    std::thread::scope(|scope| {
        // Claude's own error messages go to the log as they are.
        scope.spawn(|| {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log.line(&line);
            }
        });
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            for message in describe(&line) {
                log.line(&message);
            }
        }
    });
    let status = child.wait().context("claude did not finish")?;

    match files.load() {
        Ok(Some(curated)) if Some(&curated.created) != before.as_ref() => {
            log.line(&format!(
                "=== curate: done, {} albums, round {}",
                curated.albums.len(),
                curated.round
            ));
            Ok(())
        }
        _ => bail!("claude {status}, the list was not updated and the previous list stays"),
    }
}

/// The tools Claude may use: the two Deck commands, its candidate files and its own
/// saved tool output, and the web.
fn allowed_tools(work: &Path) -> Vec<String> {
    vec![
        "Bash(deck taste)".to_owned(),
        "Bash(deck curate submit:*)".to_owned(),
        "Write(./**)".to_owned(),
        "Read(./**)".to_owned(),
        // Claude Code saves long tool output (deck taste) in the working directory's
        // project folder. Read may read only from there, not other projects'
        // conversations.
        format!("Read(~/.claude/projects/{}/**)", project_slug(work)),
        "WebSearch".to_owned(),
    ]
}

/// Claude Code's project folder name: the path with everything but ASCII letters and
/// digits replaced by hyphens.
fn project_slug(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// `PATH` for Claude: this `deck` first, so that Claude calls the same version, then the
/// inherited `PATH` and the usual install directories.
fn search_path(home: &Path) -> Result<OsString> {
    let exe = std::env::current_exe().context("cannot find the deck executable")?;
    let mut dirs: Vec<PathBuf> = exe.parent().map(Path::to_path_buf).into_iter().collect();
    if let Some(inherited) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&inherited));
    }
    dirs.extend(
        FALLBACK_PATH
            .iter()
            .map(|dir| match dir.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(dir),
            }),
    );
    let mut unique: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    std::env::join_paths(unique).context("invalid PATH")
}

fn find_in_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|file| file.is_file())
}

/// Removes the previous run's candidate files, so that Write creates them anew.
fn remove_candidates(work: &Path) -> Result<()> {
    let entries =
        std::fs::read_dir(work).with_context(|| format!("cannot read {}", work.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.extension() == Some(OsStr::new("json")) {
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
        }
    }
    Ok(())
}

/// When the current list was made. A missing or broken list is `None`: the run makes
/// a new one either way.
fn created(files: &CuratedFiles) -> Option<String> {
    files.load().ok().flatten().map(|curated| curated.created)
}

/// The log: every line with a time, both to the terminal and to the file.
struct Log {
    file: Mutex<File>,
}

impl Log {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    fn line(&self, message: &str) {
        let now = jiff::Zoned::now().strftime("%Y-%m-%d %H:%M:%S").to_string();
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        for line in message.split('\n') {
            let line = format!("{now}  {line}\n");
            print!("{line}");
            if let Err(e) = file.write_all(line.as_bytes()) {
                log::warn!("cannot write the curate log: {e}");
            }
        }
        let _ = std::io::stdout().flush();
    }
}

/// Turns one line of `claude -p --output-format stream-json` into readable log lines.
/// A line that is not JSON (one of Claude's own messages) is kept as it is.
fn describe(line: &str) -> Vec<String> {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return vec![line.to_owned()];
    };
    let str_at = |pointer: &str| event.pointer(pointer).and_then(Value::as_str);
    match event["type"].as_str() {
        Some("system") if str_at("/subtype") == Some("init") => {
            vec![format!("start: model {}", str_at("/model").unwrap_or("?"))]
        }
        Some("assistant") => contents(&event).filter_map(describe_assistant).collect(),
        Some("user") => contents(&event)
            .filter(|c| c["type"] == "tool_result")
            .map(describe_result)
            .collect(),
        Some("result") => {
            let seconds = event["duration_ms"].as_u64().unwrap_or(0) / 1000;
            let cost = event["total_cost_usd"].as_f64().unwrap_or(0.0);
            vec![format!(
                "end: {}, {} turns, {seconds} s, ${cost:.2}",
                str_at("/subtype").unwrap_or("?"),
                event["num_turns"].as_u64().unwrap_or(0),
            )]
        }
        _ => Vec::new(),
    }
}

fn contents(event: &Value) -> impl Iterator<Item = &Value> {
    event
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn describe_assistant(content: &Value) -> Option<String> {
    let input = &content["input"];
    let field = |name: &str| input[name].as_str().unwrap_or("?");
    match content["type"].as_str()? {
        "text" => Some(format!("claude: {}", content["text"].as_str()?)),
        "tool_use" => match content["name"].as_str()? {
            "Bash" => Some(format!("$ {}", field("command"))),
            "WebSearch" => Some(format!("search: {}", field("query"))),
            "Read" => Some(format!("read: {}", field("file_path"))),
            "ToolSearch" => None,
            "Write" => {
                let candidates = serde_json::from_str::<Vec<Value>>(field("content"))
                    .map_or_else(|_| "?".to_owned(), |list| list.len().to_string());
                Some(format!(
                    "write: {} ({candidates} candidates)",
                    field("file_path")
                ))
            }
            name => Some(format!("{name}: {}", truncate(&input.to_string(), 200))),
        },
        _ => None,
    }
}

fn describe_result(content: &Value) -> String {
    let output = match &content["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
        _ => String::new(),
    };
    if content["is_error"] == true {
        return format!("  ! {}", truncate(&output, 300));
    }
    match report(&output) {
        Some(report) => describe_report(&report),
        None => format!("  ok ({} chars)", output.chars().count()),
    }
}

/// The JSON report of `deck curate submit` in a tool's output, if there is one.
fn report(output: &str) -> Option<Value> {
    let json = &output[output.find('{')?..=output.rfind('}')?];
    let report: Value = serde_json::from_str(json).ok()?;
    report.get("missing").is_some().then_some(report)
}

fn describe_report(report: &Value) -> String {
    let count = |name: &str| report[name].as_array().map_or(0, Vec::len);
    let number = |name: &str| {
        report[name]
            .as_u64()
            .map_or("?".to_owned(), |n| n.to_string())
    };
    let mut text = format!(
        "  accepted {}, unused {}, missing {}, searched {}, searches left {}",
        count("accepted"),
        count("unused"),
        number("missing"),
        number("searched"),
        number("searches_left"),
    );
    for rejected in report["rejected"].as_array().into_iter().flatten() {
        let field = |name: &str| rejected[name].as_str().unwrap_or("?");
        text.push_str(&format!(
            "\n    rejected {}: {} – {}",
            field("reason"),
            field("artist"),
            field("album")
        ));
    }
    text
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_start_and_end() {
        assert_eq!(
            describe(r#"{"type":"system","subtype":"init","model":"claude-opus-5-5"}"#),
            ["start: model claude-opus-5-5"]
        );
        assert_eq!(
            describe(
                r#"{"type":"result","subtype":"success","num_turns":15,
                    "duration_ms":218345,"total_cost_usd":1.1049}"#
            ),
            ["end: success, 15 turns, 218 s, $1.10"]
        );
        assert!(describe(r#"{"type":"system","subtype":"hook"}"#).is_empty());
    }

    #[test]
    fn describes_tool_calls() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"Reading the profile."},
            {"type":"tool_use","name":"Bash","input":{"command":"deck taste"}},
            {"type":"tool_use","name":"ToolSearch","input":{"query":"x"}},
            {"type":"tool_use","name":"WebSearch","input":{"query":"Gene Clark No Other"}},
            {"type":"tool_use","name":"Write","input":{"file_path":"/w/round-1.json",
                "content":"[{\"artist\":\"a\"},{\"artist\":\"b\"}]"}},
            {"type":"tool_use","name":"Glob","input":{"pattern":"*"}}]}}"#;
        assert_eq!(
            describe(line),
            [
                "claude: Reading the profile.",
                "$ deck taste",
                "search: Gene Clark No Other",
                "write: /w/round-1.json (2 candidates)",
                r#"Glob: {"pattern":"*"}"#,
            ]
        );
    }

    #[test]
    fn describes_results_and_reports() {
        let report = r#"{\"accepted\":[{},{}],\"rejected\":[{\"reason\":\"not_found\",\"artist\":\"Gene Clark\",\"album\":\"No Other\"}],\"unused\":[],\"missing\":18,\"searched\":3,\"searches_left\":27}"#;
        let line = format!(
            r#"{{"type":"user","message":{{"content":[
                {{"type":"tool_result","content":"{report}"}},
                {{"type":"tool_result","content":[{{"type":"text","text":"abc"}}]}},
                {{"type":"tool_result","is_error":true,"content":"Permission denied"}}]}}}}"#
        );
        assert_eq!(
            describe(&line),
            [
                "  accepted 2, unused 0, missing 18, searched 3, searches left 27\n    \
                 rejected not_found: Gene Clark – No Other",
                "  ok (3 chars)",
                "  ! Permission denied",
            ]
        );
    }

    #[test]
    fn keeps_other_lines() {
        assert_eq!(describe("Error: not signed in"), ["Error: not signed in"]);
    }

    #[test]
    fn slug_replaces_everything_but_letters_and_digits() {
        assert_eq!(
            project_slug(Path::new("/Users/me/.cache/deck/curate")),
            "-Users-me--cache-deck-curate"
        );
    }

    #[test]
    fn claude_may_only_submit() {
        let tools = allowed_tools(Path::new("/w"));
        assert!(tools.contains(&"Bash(deck curate submit:*)".to_owned()));
        assert!(!tools.iter().any(|t| t == "Bash(deck curate:*)"));
    }

    #[test]
    fn finds_commands_in_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("claude"), "").unwrap();
        let path = std::env::join_paths([Path::new("/nonexistent"), dir.path()]).unwrap();
        assert_eq!(
            find_in_path("claude", &path),
            Some(dir.path().join("claude"))
        );
        assert_eq!(find_in_path("jq", &path), None);
    }

    #[test]
    fn removes_only_candidate_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("round-1.json"), "[]").unwrap();
        std::fs::write(dir.path().join(".start"), "").unwrap();
        remove_candidates(dir.path()).unwrap();
        assert!(!dir.path().join("round-1.json").exists());
        assert!(dir.path().join(".start").exists());
    }
}
