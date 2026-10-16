//! Shared helpers for the integration test targets. Each target uses a subset of
//! these helpers, so dead-code warnings here are expected and silenced.
#![allow(dead_code)]

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Environment keys that must be neutralised so runs stay deterministic.
const ISOLATED_VARS: &[&str] = &[
    "ROUTER_LLM_URL",
    "ROUTER_LLM_KEY",
    "ROUTER_LLM_MODEL",
    "ROUTER_TIMEOUT_MS",
    "ROUTER_BASE_OVERHEAD_TOKENS",
    "ROUTER_MAX_TOKENS",
    "ROUTER_HARNESS_BIN",
];

/// Run the built binary in `cwd` with `envs`, optionally feeding stdin.
/// Returns (stdout, stderr, exit_code).
pub fn run(cwd: &str, args: &[&str], envs: &[(&str, &str)], stdin: Option<&str>) -> (String, String, i32) {
    let bin = env!("CARGO_BIN_EXE_agent-router");
    let mut cmd = Command::new(bin);
    cmd.current_dir(cwd).args(args);
    for k in ISOLATED_VARS {
        cmd.env_remove(k);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn agent-router");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait for child");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Fresh temp repo seeded with one agent and one skill manifest.
pub fn fixture_repo(tag: &str) -> String {
    let root = std::env::temp_dir().join(format!("agent-router-it-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".agents")).expect("mkdir agents");
    std::fs::create_dir_all(root.join("skills/tdd")).expect("mkdir skills");
    std::fs::write(
        root.join(".agents/rust-architect.md"),
        "---\nname: rust-architect\ndescription: Systems architect\ntriggers: [rust, borrow, lifetime]\n---\nBody\n",
    )
    .expect("write agent");
    std::fs::write(
        root.join("skills/tdd/SKILL.md"),
        "---\nname: tdd-workflow\ndescription: Red-Green-Refactor\ntriggers: [test, tdd]\n---\nBody\n",
    )
    .expect("write skill");
    root.to_string_lossy().into_owned()
}

/// Minimal OpenAI-compatible mock built purely on std::net (no extra crates).
pub struct MockLlm {
    pub url: String,
    hits: Arc<AtomicUsize>,
    stop: Arc<std::sync::Mutex<bool>>,
}

impl MockLlm {
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        *self.stop.lock().unwrap() = true;
    }
}

/// Spawn a mock that answers every POST with `content` as the assistant message.
pub fn spawn_mock_llm(content: String) -> MockLlm {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock llm");
    let port = listener.local_addr().expect("mock addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(std::sync::Mutex::new(false));

    let (hits_t, stop_t) = (hits.clone(), stop.clone());
    std::thread::spawn(move || {
        let _ = listener.set_nonblocking(true);
        loop {
            if *stop_t.lock().unwrap() {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Read until the head terminator; the body is irrelevant to the reply.
                    let mut buf = Vec::new();
                    let mut scratch = [0u8; 2048];
                    loop {
                        let n = stream.read(&mut scratch).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&scratch[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                        if buf.len() > 262_144 {
                            break;
                        }
                    }
                    hits_t.fetch_add(1, Ordering::SeqCst);
                    let payload = serde_json::json!({
                        "choices": [{ "message": { "role": "assistant", "content": content } }]
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    MockLlm {
        url: format!("http://127.0.0.1:{}/v1/chat/completions", port),
        hits,
        stop,
    }
}
