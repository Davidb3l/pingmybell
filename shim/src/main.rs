//! PingMyBell hook shim.
//!
//! Invoked by agent hooks: `pingmybell-shim claude <session-start|stop|notification|session-end>`.
//! Reads the hook JSON from stdin, maps it to a normalized event
//! (ARCHITECTURE.md §4), and POSTs it to the local ingest server discovered
//! via `~/.pingmybell/{port,token}`.
//!
//! Hard invariant (PRD AC-2.4): fail open. Any error — missing files, app not
//! running, bad JSON, network trouble, panic — must end with exit 0 and no
//! stdout, so a broken or absent PingMyBell never blocks or alters agent
//! behavior. Only step-4 `pretool` decisions will ever print to stdout.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

const STDIN_CAP_BYTES: u64 = 5 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const IO_TIMEOUT: Duration = Duration::from_millis(1500);
/// /v1/approval parks up to 110 s server-side; this must exceed that but
/// stay inside the 120 s PreToolUse hook timeout.
const APPROVAL_READ_TIMEOUT: Duration = Duration::from_secs(115);

fn main() {
    // Swallow panic output too: nothing we do may leak onto the hook's
    // stdout/stderr or produce a nonzero exit.
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(run);
    // Implicit exit 0 on every path.
}

fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 || args[0] != "claude" {
        return;
    }

    let mut input = String::new();
    if std::io::stdin()
        .take(STDIN_CAP_BYTES)
        .read_to_string(&mut input)
        .is_err()
    {
        return;
    }
    let hook: Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return,
    };

    if args[1] == "pretool" {
        run_pretool(&hook);
        return;
    }

    let Some(body) = map_claude_hook(&args[1], &hook) else {
        return;
    };
    post_event(&body.to_string(), "/v1/event", IO_TIMEOUT);
}

/// Blocking approval flow (FR-6): POST to /v1/approval and wait for the
/// user's decision. On a decision, print the PreToolUse output JSON to
/// stdout — the ONLY case where the shim ever writes to stdout. On 204,
/// timeout, or any error: print nothing, exit 0, and Claude Code falls
/// through to its own terminal prompt (AC-6.3).
fn run_pretool(hook: &Value) {
    let Some(session_id) = hook["session_id"].as_str().filter(|s| !s.is_empty()) else {
        return;
    };
    let tool_name = hook["tool_name"].as_str().unwrap_or_default();
    if tool_name.is_empty() {
        return;
    }
    let body = json!({
        "agent": "claude-code",
        "event": "permission_request",
        "session_id": session_id,
        "cwd": hook["cwd"].as_str().unwrap_or_default(),
        "summary": null,
        "transcript_path": hook["transcript_path"].as_str(),
        "tool": { "name": tool_name, "input": hook["tool_input"] },
        "terminal": null,
    });

    // The server parks this request for up to 110 s; stay comfortably inside
    // the 120 s hook timeout.
    let Some(response) = post_event(&body.to_string(), "/v1/approval", APPROVAL_READ_TIMEOUT)
    else {
        return;
    };
    let Some(decision) = parse_decision(&response) else {
        return;
    };

    let output = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": format!("PingMyBell: {decision} from overlay"),
        }
    });
    println!("{output}");
}

/// Extract the decision from a raw HTTP response; None for 204/anything odd.
fn parse_decision(raw: &str) -> Option<&'static str> {
    let status_line = raw.lines().next()?;
    if !status_line.contains("200") {
        return None;
    }
    let body = raw.split("\r\n\r\n").nth(1)?;
    let parsed: Value = serde_json::from_str(body.trim()).ok()?;
    match parsed["decision"].as_str()? {
        "allow" => Some("allow"),
        "deny" => Some("deny"),
        "ask" => Some("ask"),
        _ => None,
    }
}

/// Map a Claude Code hook payload to a normalized event. Returns None when
/// the payload is unusable (fail open).
fn map_claude_hook(subcommand: &str, hook: &Value) -> Option<Value> {
    let session_id = hook["session_id"].as_str().filter(|s| !s.is_empty())?;
    let cwd = hook["cwd"].as_str().unwrap_or_default();
    let transcript_path = hook["transcript_path"].as_str();

    let (event, summary, terminal) = match subcommand {
        "session-start" => ("session_start", None, Some(terminal_info())),
        // Stop carries last_assistant_message (verified against claude
        // 2.1.198); core falls back to transcript_path when absent.
        "stop" => (
            "turn_complete",
            hook["last_assistant_message"].as_str(),
            None,
        ),
        "notification" => {
            // Notifications fire for more than permission/idle prompts (e.g.
            // auth success). Only attention-worthy ones become events; when
            // the type field is absent we keep the notification (AC-2.3 over
            // false negatives).
            if !notification_needs_attention(hook) {
                return None;
            }
            ("needs_attention", hook["message"].as_str(), None)
        }
        "session-end" => ("session_end", None, None),
        _ => return None,
    };

    Some(json!({
        "agent": "claude-code",
        "event": event,
        "session_id": session_id,
        "cwd": cwd,
        "summary": summary,
        "transcript_path": transcript_path,
        "tool": null,
        "terminal": terminal,
    }))
}

fn notification_needs_attention(hook: &Value) -> bool {
    match hook["notification_type"].as_str() {
        // Unknown/absent type: err toward speaking (missing a permission
        // prompt is worse than an extra callout).
        None => true,
        Some(t) => t.contains("permission") || t.contains("idle") || t.contains("elicitation"),
    }
}

fn terminal_info() -> Value {
    #[cfg(unix)]
    let ppid = unsafe { libc::getppid() } as i64;
    #[cfg(not(unix))]
    let ppid = 0i64;

    json!({
        "pid": std::process::id(),
        "ppid_chain": [ppid],
        "tty": tty_name(),
        "tmux_pane": std::env::var("TMUX_PANE").ok(),
        "term_program": std::env::var("TERM_PROGRAM").ok(),
        "hwnd": null,
    })
}

#[cfg(unix)]
fn tty_name() -> Option<String> {
    // Hook stdin is a pipe; stderr (then stdout) may still be the terminal.
    for fd in [2, 1, 0] {
        let ptr = unsafe { libc::ttyname(fd) };
        if !ptr.is_null() {
            let name = unsafe { std::ffi::CStr::from_ptr(ptr) };
            return Some(name.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(not(unix))]
fn tty_name() -> Option<String> {
    None
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    let home = std::env::var_os("HOME");
    #[cfg(not(unix))]
    let home = std::env::var_os("USERPROFILE");
    home.map(PathBuf::from)
}

/// Discover the ingest server, POST the body to `path`, and return the raw
/// HTTP response. Every failure returns None — the caller ignores it (fail
/// open). `read_timeout` bounds how long we wait for the response: short for
/// fire-and-forget events, long for the blocking approval poll.
fn post_event(body: &str, path: &str, read_timeout: Duration) -> Option<String> {
    let dir = home_dir()?.join(".pingmybell");
    let port: u16 = std::fs::read_to_string(dir.join("port"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let token = std::fs::read_to_string(dir.join("token")).ok()?;
    let token = token.trim();

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_read_timeout(Some(read_timeout)).ok()?;

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    // Read to connection close (the server sends Connection: close); capped
    // small — responses are a status line, headers, and a tiny JSON body.
    let mut raw = String::new();
    let _ = stream.take(64 * 1024).read_to_string(&mut raw);
    (!raw.is_empty()).then_some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop_hook() -> Value {
        json!({
            "session_id": "sess-1",
            "cwd": "/tmp/proj",
            "transcript_path": "/tmp/t.jsonl",
            "hook_event_name": "Stop",
            "stop_hook_active": false,
            "last_assistant_message": "All done."
        })
    }

    #[test]
    fn stop_maps_to_turn_complete_with_summary() {
        let e = map_claude_hook("stop", &stop_hook()).unwrap();
        assert_eq!(e["event"], "turn_complete");
        assert_eq!(e["agent"], "claude-code");
        assert_eq!(e["summary"], "All done.");
        assert_eq!(e["transcript_path"], "/tmp/t.jsonl");
        assert_eq!(e["session_id"], "sess-1");
    }

    #[test]
    fn session_start_captures_terminal() {
        let hook = json!({"session_id": "s", "cwd": "/tmp", "hook_event_name": "SessionStart"});
        let e = map_claude_hook("session-start", &hook).unwrap();
        assert_eq!(e["event"], "session_start");
        assert!(e["terminal"].is_object());
        assert!(e["terminal"]["pid"].is_number());
    }

    #[test]
    fn notification_maps_message_to_summary() {
        let hook = json!({"session_id": "s", "cwd": "/tmp", "message": "Claude needs your permission to use Bash"});
        let e = map_claude_hook("notification", &hook).unwrap();
        assert_eq!(e["event"], "needs_attention");
        assert_eq!(e["summary"], "Claude needs your permission to use Bash");
    }

    #[test]
    fn missing_session_id_or_unknown_subcommand_fails_open() {
        assert!(map_claude_hook("stop", &json!({"cwd": "/tmp"})).is_none());
        assert!(map_claude_hook("mystery", &stop_hook()).is_none());
    }

    #[test]
    fn parse_decision_handles_200_204_and_garbage() {
        let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\n\r\n{\"decision\":\"allow\"}";
        assert_eq!(parse_decision(ok), Some("allow"));
        let deny = "HTTP/1.1 200 OK\r\n\r\n{\"decision\":\"deny\"}";
        assert_eq!(parse_decision(deny), Some("deny"));
        let no_content = "HTTP/1.1 204 No Content\r\n\r\n";
        assert_eq!(parse_decision(no_content), None);
        assert_eq!(parse_decision("HTTP/1.1 200 OK\r\n\r\nnot json"), None);
        assert_eq!(
            parse_decision("HTTP/1.1 200 OK\r\n\r\n{\"decision\":\"self-destruct\"}"),
            None
        );
        assert_eq!(parse_decision(""), None);
    }

    #[test]
    fn benign_notification_types_are_dropped() {
        let benign = json!({"session_id": "s", "cwd": "/tmp", "message": "Signed in", "notification_type": "auth_success"});
        assert!(map_claude_hook("notification", &benign).is_none());

        let perm = json!({"session_id": "s", "cwd": "/tmp", "message": "Needs approval", "notification_type": "permission_prompt"});
        assert!(map_claude_hook("notification", &perm).is_some());

        let untyped = json!({"session_id": "s", "cwd": "/tmp", "message": "Waiting for input"});
        assert!(
            map_claude_hook("notification", &untyped).is_some(),
            "absent type must err toward speaking"
        );
    }
}
