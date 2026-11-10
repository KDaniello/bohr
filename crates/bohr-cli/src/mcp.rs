//! `bohr mcp`: serves the commands as Model Context Protocol tools over standard input and output.
//!
//! Each tool runs this binary with `--json` and returns the object it wrote, so an agent gets
//! exactly what a pipeline reads. A call costs one process.
//!
//! `explore` and `reduce` take minutes, so they return a handle at once. `status` reports the
//! command's last progress line and, when it ends, its object; `cancel` kills its process group.
//! Each request runs on its own thread, so a long `run` does not delay a `status`.
//!
//! The server runs only what the command line can run, and its jobs end with it. Messages are
//! JSON-RPC, one per line.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::{Value, json};

use crate::CliError;

/// The protocol revision this server speaks.
const PROTOCOL: &str = "2025-06-18";

/// One command started in the background.
struct Job {
    child: Option<Child>,
    /// The child's process identifier, kept after the child is taken to be waited on.
    pid: u32,
    /// The last line the command wrote to standard error: its progress, or why it stopped.
    progress: String,
    /// What it wrote to standard output once it ended, and the code it ended with.
    finished: Option<(String, i32)>,
}

type Jobs = Arc<Mutex<BTreeMap<String, Job>>>;

/// Serves until standard input closes.
pub(crate) fn serve() -> Result<u8, CliError> {
    let jobs: Jobs = Arc::default();
    let next = Arc::new(AtomicU64::new(0));
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let mut answering = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| CliError::Internal(format!("reading a request: {e}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let (jobs, next, out) = (Arc::clone(&jobs), Arc::clone(&next), Arc::clone(&out));
        answering.push(std::thread::spawn(move || {
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(request) => answer(&request, &jobs, &next),
                Err(e) => Some(failure(&Value::Null, -32700, &format!("not JSON: {e}"))),
            };
            if let (Some(reply), Ok(mut out)) = (reply, out.lock()) {
                let _ = writeln!(out, "{reply}").and_then(|()| out.flush());
            }
        }));
        answering.retain(|thread| !thread.is_finished());
    }
    // Nobody is left to ask for what the jobs found, so nothing should go on spending on them.
    let left = jobs.lock().map(|mut jobs| std::mem::take(&mut *jobs)).unwrap_or_default();
    for job in left.values() {
        end(job.pid);
    }
    for thread in answering {
        let _ = thread.join();
    }
    Ok(crate::exit::CLEAN)
}

/// The reply to one message; none for a notification.
fn answer(request: &Value, jobs: &Jobs, next: &AtomicU64) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request["method"].as_str().unwrap_or("");
    let params = &request["params"];
    let result = match method {
        "initialize" => json!({
            "protocolVersion": PROTOCOL,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "bohr", "version": env!("CARGO_PKG_VERSION") },
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match call(name, &arguments, jobs, next) {
                // A command that failed answered with its error object, which is an error here too.
                Ok(object) => content(&object, object["schema"] == "bohr.error/1"),
                Err(message) => content(&json!({ "error": message }), true),
            }
        }
        other => return Some(failure(&id, -32601, &format!("no method named {other}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn failure(id: &Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A tool's result: the object, as structured content and as the text of it.
fn content(object: &Value, error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": object.to_string() }],
        "structuredContent": object,
        "isError": error,
    })
}

/// Carries out one tool call: the command it stands for, or a question about a job.
fn call(name: &str, arguments: &Value, jobs: &Jobs, next: &AtomicU64) -> Result<Value, String> {
    match name {
        "status" => status(arguments, jobs),
        "cancel" => cancel(arguments, jobs),
        _ => {
            let args = command_line(name, arguments)?;
            if matches!(name, "explore" | "reduce") {
                let handle = format!("{name}-{}", next.fetch_add(1, Ordering::Relaxed) + 1);
                start(&handle, &args, jobs)?;
                Ok(json!({ "handle": handle, "running": true }))
            } else {
                let output = bohr(&args).output().map_err(|e| format!("starting bohr: {e}"))?;
                parse(&String::from_utf8_lossy(&output.stdout)).map_err(|missing| {
                    format!(
                        "{missing}; it said: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )
                })
            }
        }
    }
}

/// This binary, told to answer in JSON, in a process group of its own.
fn bohr(args: &[String]) -> Command {
    let mut command =
        Command::new(std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("bohr")));
    command.arg("--json").args(args).stdin(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command
}

/// Ends a job and everything it started: its whole process group.
fn end(pid: u32) {
    #[cfg(unix)]
    let _ = Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).status();
    #[cfg(not(unix))]
    let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).status();
}

/// The object a command wrote, or what it wrote where that is not one.
fn parse(stdout: &str) -> Result<Value, String> {
    let last = stdout.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("");
    serde_json::from_str(last).map_err(|_| format!("bohr wrote no JSON object: {stdout:?}"))
}

fn start(handle: &str, args: &[String], jobs: &Jobs) -> Result<(), String> {
    let mut child = bohr(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("starting bohr: {e}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let job = Job { pid: child.id(), child: Some(child), progress: String::new(), finished: None };
    jobs.lock().map_err(|_| "the job table is poisoned".to_owned())?.insert(handle.to_owned(), job);

    // Progress is the last thing the command said on standard error. A live line is redrawn with a
    // carriage return, so both ends of a line count.
    if let Some(stderr) = stderr {
        let (jobs, handle) = (Arc::clone(jobs), handle.to_owned());
        std::thread::spawn(move || {
            let mut line = Vec::new();
            for byte in BufReader::new(stderr).bytes() {
                let Ok(byte) = byte else { break };
                if byte != b'\r' && byte != b'\n' {
                    line.push(byte);
                    continue;
                }
                let text = String::from_utf8_lossy(&line).trim().to_owned();
                if !text.is_empty()
                    && let Ok(mut jobs) = jobs.lock()
                    && let Some(job) = jobs.get_mut(&handle)
                {
                    job.progress = text;
                }
                line.clear();
            }
        });
    }
    let (jobs, handle) = (Arc::clone(jobs), handle.to_owned());
    std::thread::spawn(move || {
        let mut out = String::new();
        if let Some(mut stdout) = stdout {
            let _ = stdout.read_to_string(&mut out);
        }
        let child = jobs.lock().ok().and_then(|mut jobs| jobs.get_mut(&handle)?.child.take());
        let code =
            child.and_then(|mut child| child.wait().ok()).and_then(|s| s.code()).unwrap_or(-1);
        if let Ok(mut jobs) = jobs.lock()
            && let Some(job) = jobs.get_mut(&handle)
        {
            job.finished = Some((out, code));
        }
    });
    Ok(())
}

fn status(arguments: &Value, jobs: &Jobs) -> Result<Value, String> {
    let handle = arguments["handle"].as_str().ok_or("status needs a handle")?;
    let jobs = jobs.lock().map_err(|_| "the job table is poisoned".to_owned())?;
    let job = jobs.get(handle).ok_or_else(|| format!("no job named {handle}"))?;
    Ok(match &job.finished {
        None => json!({ "handle": handle, "running": true, "progress": job.progress }),
        Some((out, code)) => json!({
            "handle": handle,
            "running": false,
            "progress": job.progress,
            "exit": code,
            "result": parse(out).unwrap_or(Value::Null),
        }),
    })
}

fn cancel(arguments: &Value, jobs: &Jobs) -> Result<Value, String> {
    let handle = arguments["handle"].as_str().ok_or("cancel needs a handle")?;
    let jobs = jobs.lock().map_err(|_| "the job table is poisoned".to_owned())?;
    let job = jobs.get(handle).ok_or_else(|| format!("no job named {handle}"))?;
    let running = job.finished.is_none();
    if running {
        end(job.pid);
    }
    Ok(json!({ "handle": handle, "cancelled": running }))
}

/// The tools, each with the arguments its command takes.
fn tools() -> Vec<Value> {
    let text = json!({ "type": "string" });
    let number = json!({ "type": "integer", "minimum": 0 });
    let flag = json!({ "type": "boolean" });
    let words = json!({ "type": "array", "items": { "type": "string" } });
    let tool = |name: &str, about: &str, properties: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": about,
            "inputSchema": { "type": "object", "properties": properties, "required": required },
        })
    };
    vec![
        tool(
            "run",
            "Run a program deterministically and say what it came to: `bohr run --json`.",
            json!({ "program": words, "seed": number, "config": text, "save": text,
                    "repeat": number, "max_events": number, "scheduler": text }),
            &["program"],
        ),
        tool(
            "survey",
            "List the system calls a program makes that bohr does not model: `bohr survey --json`.",
            json!({ "program": words, "seed": number, "config": text, "max_events": number }),
            &["program"],
        ),
        tool(
            "explore",
            "Start a campaign searching for a finding; answers with a handle for `status`.",
            json!({ "program": words, "config": text, "runs": number, "time": text,
                    "jobs": number, "findings": text, "master_seed": number }),
            &[],
        ),
        tool(
            "replay",
            "Replay stored bundles and check each reaches its recorded digest.",
            json!({ "bundles": words, "verify_only": flag, "binaries": text }),
            &["bundles"],
        ),
        tool(
            "reduce",
            "Start reducing a finding to the smallest run that still reproduces it; answers with a \
             handle for `status`.",
            json!({ "bundle": text, "out": text, "budget": number, "keep": text }),
            &["bundle", "out"],
        ),
        tool(
            "report",
            "Write a stored run as a page, a disclosure draft (`disclosure`) or SARIF (`sarif`).",
            json!({ "bundle": text, "out": text, "disclosure": flag, "sarif": flag, "redact": flag }),
            &["bundle"],
        ),
        tool(
            "findings",
            "Group the findings in a directory by what went wrong, smallest bundle first.",
            json!({ "dir": text }),
            &["dir"],
        ),
        tool(
            "status",
            "How far a started campaign or reduction got, and its result once it has ended.",
            json!({ "handle": text }),
            &["handle"],
        ),
        tool(
            "cancel",
            "End a started campaign or reduction, and everything it started.",
            json!({ "handle": text }),
            &["handle"],
        ),
    ]
}

/// The command line for a tool call.
///
/// Values are joined to their flags and positionals follow `--`, so `-x.bohr` stays a file name.
fn command_line(name: &str, arguments: &Value) -> Result<Vec<String>, String> {
    let mut args = vec![name.to_owned()];
    let mut option = |flag: &str, key: &str| match &arguments[key] {
        Value::String(text) => args.push(format!("{flag}={text}")),
        Value::Number(number) => args.push(format!("{flag}={number}")),
        Value::Bool(true) => args.push(flag.to_owned()),
        _ => {}
    };
    let words = |key: &str| -> Vec<String> {
        arguments[key]
            .as_array()
            .map(|items| items.iter().filter_map(|w| w.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    let positional: Vec<String> = match name {
        "run" | "survey" => {
            option("--seed", "seed");
            option("--config", "config");
            option("--max-events", "max_events");
            if name == "run" {
                option("--save", "save");
                option("--repeat", "repeat");
                option("--scheduler", "scheduler");
            }
            let program = words("program");
            if program.is_empty() {
                return Err(format!("{name} needs a program"));
            }
            program
        }
        "explore" => {
            option("--config", "config");
            option("--runs", "runs");
            option("--time", "time");
            option("--jobs", "jobs");
            option("--findings", "findings");
            option("--master-seed", "master_seed");
            words("program")
        }
        "replay" => {
            option("--verify-only", "verify_only");
            option("--binaries", "binaries");
            let bundles = words("bundles");
            if bundles.is_empty() {
                return Err("replay needs at least one bundle".to_owned());
            }
            bundles
        }
        "reduce" => {
            option("--out", "out");
            option("--budget", "budget");
            option("--keep", "keep");
            vec![required(arguments, "bundle")?]
        }
        "report" => {
            option("--out", "out");
            option("--disclosure", "disclosure");
            option("--sarif", "sarif");
            option("--redact", "redact");
            vec![required(arguments, "bundle")?]
        }
        "findings" => vec![required(arguments, "dir")?],
        other => return Err(format!("no tool named {other}")),
    };
    if !positional.is_empty() {
        args.push("--".to_owned());
        args.extend(positional);
    }
    Ok(args)
}

fn required(arguments: &Value, key: &str) -> Result<String, String> {
    arguments[key].as_str().map(str::to_owned).ok_or_else(|| format!("{key} is required"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call maps to the command line a person would type, and a leading dash stays data.
    #[test]
    fn a_call_is_the_command_it_stands_for() {
        let args =
            command_line("run", &json!({ "program": ["./a", "-x"], "seed": 7, "save": "r.bohr" }))
                .expect("a command line");
        assert_eq!(args, ["run", "--seed=7", "--save=r.bohr", "--", "./a", "-x"]);
        let args = command_line("replay", &json!({ "bundles": ["-a.bohr"], "verify_only": true }))
            .expect("a command line");
        assert_eq!(args, ["replay", "--verify-only", "--", "-a.bohr"], "a bundle read as a flag");
        assert!(command_line("run", &json!({})).is_err(), "a run with no program was accepted");
        assert!(command_line("sudo", &json!({})).is_err());
    }

    /// Every listed tool is callable.
    #[test]
    fn every_listed_tool_is_callable() {
        for tool in tools() {
            let name = tool["name"].as_str().expect("a name");
            if matches!(name, "status" | "cancel") {
                continue;
            }
            let error = command_line(name, &json!({})).err().unwrap_or_default();
            assert!(!error.starts_with("no tool named"), "{name} is listed and not callable");
        }
    }

    /// The handshake names the protocol and the server; an unknown method gets -32601.
    #[test]
    fn the_handshake_and_a_wrong_method() {
        let jobs: Jobs = Arc::default();
        let next = AtomicU64::new(0);
        let ask = |request: Value| answer(&request, &jobs, &next);
        let reply = ask(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })).expect("one");
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL);
        assert_eq!(reply["result"]["serverInfo"]["name"], "bohr");
        let reply = ask(json!({ "jsonrpc": "2.0", "id": 2, "method": "nope" })).expect("one");
        assert_eq!(reply["error"]["code"], -32601);
        assert!(ask(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
    }
}
