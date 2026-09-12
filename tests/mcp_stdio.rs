//! The stdio transport owns stdout, and this is the test that says so out loud.
//!
//! `docs/MCP.md` asks for it by name, under "Recorded traps": a single stray `println!` in a
//! library module turns into a protocol parse error on the client side with no useful
//! diagnostic, so the server is worth running for real and asserting that stdout carries only
//! well-formed JSON-RPC. It earned its place before it was written — the first build of
//! `session-search mcp` answered nothing at all,
//! because `cli::run` held `stdout().lock()` across `dispatch` and the transport's first write,
//! from another thread, parked on it forever. No panic, no log line, no output: exactly the
//! failure this file exists to make loud.
//!
//! It drives the real binary rather than the `Server` type, because both halves of the bug lived
//! outside it — one in argument dispatch, one in the transport — and a test that constructs the
//! server directly reaches neither.

#![cfg(feature = "mcp")]

use std::io::Write;
use std::process::{Command, Stdio};

/// Initialize, then list the tools. Written as one blob because stdin is closed straight after:
/// EOF is a clean shutdown, and a server that needed to be killed to give up its output would be
/// hiding a flush bug.
const SESSION: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","#,
    r#""capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    "\n",
    r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    "\n",
);

fn run_session() -> (String, String) {
    let dir = tempfile::tempdir().expect("temp index dir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_session-search"))
        .arg("--index")
        .arg(dir.path())
        .arg("mcp")
        // No refresh: this test is about the transport, and indexing whatever transcripts happen
        // to exist on the machine running it would make it neither hermetic nor fast.
        .arg("--no-refresh")
        .args(["--refresh-secs", "0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the server");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(SESSION.as_bytes())
        .expect("writing the session");
    let out = child.wait_with_output().expect("server exits on stdin EOF");
    assert!(out.status.success(), "server exited with {}", out.status);
    (
        String::from_utf8(out.stdout).expect("stdout is UTF-8"),
        String::from_utf8(out.stderr).expect("stderr is UTF-8"),
    )
}

#[test]
fn stdout_carries_only_well_formed_json_rpc() {
    let (stdout, _) = run_session();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 2, "one response per request: {stdout:?}");
    for line in &lines {
        let value: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|err| panic!("not JSON-RPC: {err}: {line}"));
        assert_eq!(
            value.get("jsonrpc").and_then(|v| v.as_str()),
            Some("2.0"),
            "{line}"
        );
        assert!(value.get("error").is_none(), "protocol error: {line}");
    }
}

#[test]
fn the_server_names_itself_and_describes_its_index() {
    let (stdout, _) = run_session();
    let init: serde_json::Value =
        serde_json::from_str(stdout.lines().next().expect("a first response")).expect("JSON");
    let info = &init["result"]["serverInfo"];
    // `ServerInfo::new` calls `Implementation::from_build_env()`, whose `env!` expands inside the
    // rmcp crate: the default on the wire is `{"name":"rmcp","version":"3.2.0"}`. A server that
    // introduces itself as its own transport library is not a cosmetic problem — it is what the
    // host shows the user when it asks whether to trust the tools.
    assert_eq!(info["name"], "session-search");
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    let instructions = init["result"]["instructions"]
        .as_str()
        .expect("instructions are served");
    assert!(
        instructions.contains("Route by the shape of the answer"),
        "{instructions}"
    );
    assert!(
        !instructions.contains('{'),
        "an unsubstituted placeholder reached a client: {instructions}"
    );
}

#[test]
fn every_tool_is_listed_with_a_schema_a_model_can_read() {
    let (stdout, _) = run_session();
    let listed: serde_json::Value =
        serde_json::from_str(stdout.lines().nth(1).expect("a second response")).expect("JSON");
    let tools = listed["result"]["tools"].as_array().expect("a tool array");
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "aggregate",
            "get_output",
            "get_turn",
            "search_sessions",
            "search_turns"
        ]
    );

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let input = &tool["inputSchema"];
        // An input schema whose root is not an object panics rmcp's router at startup rather
        // than at build time, so the shape is worth asserting even though it compiled.
        assert_eq!(input["type"], "object", "{name} input schema");
        // Nothing required but `aggregate.field`: `{"query":"SIGBUS"}` must be a valid
        // `search_turns` call, which is what `#[serde(default)]` on `Filters` and on each
        // request struct buys. `field` is the exception because there is no field worth
        // counting by default — a schema that let the call through would only buy the caller a
        // refusal naming a field nobody wrote.
        match name {
            "aggregate" => assert_eq!(
                input["required"],
                serde_json::json!(["field"]),
                "aggregate must declare the one argument it cannot answer without"
            ),
            _ => assert!(input.get("required").is_none(), "{name} demands arguments"),
        }
        // The descriptions are the deliverable — issue #28, "the schemas are the actual work".
        // A property without one is a filter a model will guess at.
        for (property, schema) in input["properties"].as_object().expect("properties") {
            assert!(
                schema.get("description").is_some() || schema.get("$ref").is_some(),
                "{name}.{property} has no description"
            );
        }
        // An output schema is only derived when the return type literally reads `Json<T>`; a
        // type alias hiding it yields none, silently.
        assert_eq!(
            tool["outputSchema"]["type"], "object",
            "{name} output schema"
        );
        assert!(
            tool["description"].as_str().is_some_and(|d| d.len() > 400),
            "{name} has no real description"
        );
    }

    // The two closed vocabularies are constrained, not merely described: a model that sends
    // `kind: "toolcall"` should be stopped by the schema, not by a silent zero.
    let filters = &tools
        .iter()
        .find(|t| t["name"] == "search_turns")
        .expect("search_turns")["inputSchema"]["properties"];
    assert_eq!(
        filters["kind"]["enum"],
        serde_json::json!(["message", "tool_call", null])
    );
    assert_eq!(
        filters["role"]["enum"],
        serde_json::json!(["user", "assistant", "system", "attachment", null])
    );
}

#[test]
fn diagnostics_go_to_stderr_without_colour() {
    // The index directory is empty, so `index_stats` warns. That warning must land on stderr —
    // and without ANSI, since the CLI's colour decision must not reach a pipe the client is
    // parsing.
    let (stdout, stderr) = run_session();
    assert!(
        !stdout.contains('\u{1b}'),
        "an escape sequence reached stdout"
    );
    assert!(
        !stderr.contains('\u{1b}'),
        "colour is on for a non-tty stderr"
    );
    assert!(
        stderr.contains("nothing indexed yet"),
        "the empty-index warning went somewhere else: {stderr:?}"
    );
}
