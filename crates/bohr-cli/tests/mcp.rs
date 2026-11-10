//! `bohr mcp` against the command line: every tool answers what `--json` writes for its command.
//!
//! A client here is a child process spoken to on its standard input, as an agent's client speaks
//! to it. What is compared is the tool's structured content against the object the same command
//! writes when run by hand, field for field; the only fields left out are the ones that measure
//! the host rather than the run.

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use serde_json::{Value, json};

struct Client {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next: u64,
}

impl Client {
    fn start(dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_bohr"))
            .arg("mcp")
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the server starts");
        let input = child.stdin.take().expect("its input");
        let output = BufReader::new(child.stdout.take().expect("its output"));
        let mut client = Self { child, input, output, next: 0 };
        let reply = client.ask("initialize", json!({ "protocolVersion": "2025-06-18" }));
        assert_eq!(reply["result"]["serverInfo"]["name"], "bohr", "{reply}");
        writeln!(
            client.input,
            "{}",
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )
        .expect("a notification");
        client
    }

    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let request =
            json!({ "jsonrpc": "2.0", "id": self.next, "method": method, "params": params });
        writeln!(self.input, "{request}").expect("a request");
        self.input.flush().expect("sent");
        let mut line = String::new();
        self.output.read_line(&mut line).expect("a reply");
        let reply: Value = serde_json::from_str(&line).expect("a JSON reply");
        assert_eq!(reply["id"], self.next, "a reply to another request: {reply}");
        reply
    }

    /// A tool's structured content.
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let reply = self.ask("tools/call", json!({ "name": tool, "arguments": arguments }));
        reply["result"]["structuredContent"].clone()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The object `bohr --json <args>` writes in `dir`.
fn by_hand(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_bohr"))
        .arg("--json")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("the binary runs");
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{args:?}: {e}: {text}"))
}

/// A directory holding copies of the stored bundles.
fn workdir(case: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bohr-mcp-{case}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("findings")).expect("a directory");
    let stored = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/bundles");
    for entry in std::fs::read_dir(stored).expect("stored bundles").flatten() {
        if entry.path().extension().is_some_and(|e| e == "bohr") {
            std::fs::copy(entry.path(), dir.join("findings").join(entry.file_name()))
                .expect("a copy");
        }
    }
    dir
}

/// Every tool the server lists, and the two that follow a job.
#[test]
fn the_tools_are_the_commands() {
    let dir = workdir("list");
    let mut client = Client::start(&dir);
    let reply = client.ask("tools/list", json!({}));
    let names: Vec<&str> = reply["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for name in
        ["run", "survey", "explore", "replay", "reduce", "report", "findings", "status", "cancel"]
    {
        assert!(names.contains(&name), "no tool {name}: {names:?}");
    }
}

/// What an agent is told is what the command line writes, for the tools no backend is needed for.
#[test]
fn an_agent_gets_what_the_command_line_gives() {
    let dir = workdir("same");
    let mut client = Client::start(&dir);
    assert_eq!(
        client.call("findings", json!({ "dir": "findings" })),
        by_hand(&dir, &["findings", "findings"])
    );
    assert_eq!(
        client
            .call("report", json!({ "bundle": "findings/two-tasks-seed0.bohr", "out": "a.html" })),
        by_hand(&dir, &["report", "findings/two-tasks-seed0.bohr", "-o", "a.html"])
    );
    // A refusal is an answer too, and the same one.
    assert_eq!(
        client.call("run", json!({ "program": ["x"], "seed": 1, "config": "missing.toml" })),
        by_hand(&dir, &["run", "--seed", "1", "--config", "missing.toml", "--", "x"])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A run, a replay and a reduction on a real backend, the reduction followed through its handle;
/// and a question about a running job answered in under a tenth of a second.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod real {
    use super::*;

    fn guest(name: &str) -> String {
        format!("{}/../../testdata/guests/build/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    /// Fields that measure the host — processor time, memory, wall time — and not the run.
    fn of_the_run(mut value: Value) -> Value {
        fn strip(value: &mut Value) {
            if let Value::Object(fields) = value {
                fields.retain(|key, _| {
                    !matches!(
                        key.as_str(),
                        "peak_rss_kib"
                            | "cpu_user_ms"
                            | "cpu_system_ms"
                            | "guest_cpu_ms"
                            | "seconds"
                            | "elapsed_ms"
                            | "wall_ms"
                    )
                });
                fields.values_mut().for_each(strip);
            } else if let Value::Array(items) = value {
                items.iter_mut().for_each(strip);
            }
        }
        strip(&mut value);
        value
    }

    /// The host clock is read, which nothing in the engine may do: a status's latency is real time.
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn a_run_a_replay_and_a_reduction_are_the_same_through_either() {
        let lease = guest("lease_clock");
        let config =
            format!("{}/../../testdata/bugs/lease_clock/scenario.toml", env!("CARGO_MANIFEST_DIR"));
        if !Path::new(&lease).exists() {
            eprintln!("skipped: run `make guests`");
            return;
        }
        let dir = workdir("real");
        let mut client = Client::start(&dir);
        let through = client.call(
            "run",
            json!({ "program": [lease], "seed": 1, "config": config, "save": "a.bohr" }),
        );
        let direct = by_hand(
            &dir,
            &["run", "--seed", "1", "--config", &config, "--save", "b.bohr", "--", &lease],
        );
        let (mut through, mut direct) = (of_the_run(through), of_the_run(direct));
        through["saved"] = Value::Null;
        direct["saved"] = Value::Null;
        assert_eq!(through, direct);
        assert_eq!(
            client.call("replay", json!({ "bundles": ["a.bohr"], "verify_only": true })),
            by_hand(&dir, &["replay", "--verify-only", "a.bohr"])
        );

        let started =
            client.call("reduce", json!({ "bundle": "a.bohr", "out": "a-min.bohr", "budget": 12 }));
        let handle = started["handle"].as_str().expect("a handle").to_owned();
        let asked = std::time::Instant::now();
        let first = client.call("status", json!({ "handle": handle }));
        assert!(asked.elapsed().as_millis() < 100, "a status took {:?}", asked.elapsed());
        assert_eq!(first["handle"], handle.as_str());
        let finished = loop {
            let status = client.call("status", json!({ "handle": handle }));
            if status["running"] == false {
                break status;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        let direct = by_hand(&dir, &["reduce", "a.bohr", "-o", "b-min.bohr", "--budget", "12"]);
        let (mut through, mut direct) =
            (of_the_run(finished["result"].clone()), of_the_run(direct));
        for value in [&mut through, &mut direct] {
            value["out"] = Value::Null;
        }
        assert_eq!(through, direct);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
