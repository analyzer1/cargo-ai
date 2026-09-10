//! Reusable process-level provider smoke coverage.

#[path = "support/provider_cache.rs"]
mod provider_cache;
#[path = "support/qualification_policy.rs"]
#[allow(dead_code)]
mod qualification_policy;
#[path = "support/qualification_report.rs"]
mod qualification_report;

use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);
const TEST_TOKEN: &str = "anthropic-provider-smoke-token";
const GEMINI_TEST_TOKEN: &str = "gemini-provider-smoke-token";
const MISTRAL_TEST_TOKEN: &str = "mistral-provider-smoke-token";
const XAI_TEST_TOKEN: &str = "xai-provider-smoke-token";
const OPENAI_TEST_TOKEN: &str = "openai-provider-smoke-token";

fn fixture_base_dir(runner_temp: Option<std::ffi::OsString>) -> PathBuf {
    runner_temp
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

fn fixture_dir_name(process_id: u32, sequence: u64) -> String {
    format!("cps-{process_id:x}-{sequence:x}")
}

fn create_fixture_root(base: &Path) -> PathBuf {
    fs::create_dir_all(base).expect("provider smoke fixture base should be created");
    loop {
        let sequence = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = base.join(fixture_dir_name(std::process::id(), sequence));
        match fs::create_dir(&root) {
            Ok(()) => return root,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!(
                "provider smoke fixture root {} should be created: {error}",
                root.display()
            ),
        }
    }
}

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    definition: PathBuf,
    image: PathBuf,
    usage: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base = fixture_base_dir(std::env::var_os("RUNNER_TEMP"));
        let root = create_fixture_root(&base);
        let home = root.join("h");
        fs::create_dir_all(&home).expect("isolated Cargo AI Home should be created");
        let definition = root.join("anthropic_smoke.json");
        fs::write(&definition, definition_json()).expect("definition should be written");
        let image = root.join("pixel.png");
        fs::write(&image, [137, 80, 78, 71, 13, 10, 26, 10])
            .expect("image fixture should be written");
        let usage = root.join("usage.ndjson");
        Self {
            root,
            home,
            definition,
            image,
            usage,
        }
    }

    fn isolated_command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("CARGO_AI_HOME", &self.home)
            .env("CARGO_AI_DISABLE_KEYCHAIN", "1");
        command
    }
}

#[test]
fn fixture_base_prefers_nonempty_runner_temp() {
    let runner_temp = std::ffi::OsString::from("runner-temp");
    assert_eq!(
        fixture_base_dir(Some(runner_temp.clone())),
        PathBuf::from(runner_temp)
    );
    assert_eq!(
        fixture_base_dir(Some(std::ffi::OsString::new())),
        std::env::temp_dir()
    );
    assert_eq!(fixture_base_dir(None), std::env::temp_dir());
}

#[test]
fn fixture_directory_name_is_compact_and_deterministic() {
    assert_eq!(fixture_dir_name(0x2a, 0xff), "cps-2a-ff");
    assert!(fixture_dir_name(u32::MAX, u64::MAX).len() <= 29);
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn definition_json() -> &'static str {
    r#"{
  "agent_definition_schema_version": "2026-03-03.r1",
  "inputs": [{"type":"text","text":"Return a short status."}],
  "agent_schema": {
    "type": "object",
    "properties": {"status":{"type":"string"}},
    "required": ["status"],
    "additionalProperties": false
  },
  "actions": []
}"#
}

fn success_response() -> String {
    serde_json::json!({
        "content": [{"type": "text", "text": "{\"status\":\"ok\"}"}],
        "usage": {"input_tokens": 12, "output_tokens": 5}
    })
    .to_string()
}

fn gemini_success_response() -> String {
    serde_json::json!({
        "status": "completed",
        "steps": [{
            "type": "model_output",
            "content": [{"type": "text", "text": "{\"status\":\"ok\"}"}]
        }],
        "usage": {
            "total_input_tokens": 13,
            "total_output_tokens": 5,
            "total_thought_tokens": 2,
            "total_tokens": 20
        }
    })
    .to_string()
}

fn mistral_success_response(output: &str) -> String {
    serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": output}}],
        "usage": {"prompt_tokens": 14, "completion_tokens": 6, "total_tokens": 20}
    })
    .to_string()
}

fn xai_success_response(output: &str) -> String {
    serde_json::json!({
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": output}]
        }],
        "usage": {"input_tokens": 15, "output_tokens": 6, "total_tokens": 21}
    })
    .to_string()
}

fn openai_success_response(output: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-provider-smoke",
        "object": "chat.completion",
        "created": 1,
        "model": "openai-smoke",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": output},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 16, "completion_tokens": 6, "total_tokens": 22}
    })
    .to_string()
}

fn ollama_success_response(output: &str) -> String {
    serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": output}}],
        "prompt_eval_count": 8,
        "eval_count": 3
    })
    .to_string()
}

struct MockServer {
    url: String,
    request: thread::JoinHandle<String>,
}

impl MockServer {
    fn success() -> Self {
        Self::respond_after(Duration::ZERO, 200, success_response())
    }

    fn gemini_success() -> Self {
        Self::respond_after_at(
            "/v1beta/interactions",
            Duration::ZERO,
            200,
            gemini_success_response(),
        )
    }

    fn mistral_success() -> Self {
        Self::respond_after_at(
            "/v1/chat/completions",
            Duration::ZERO,
            200,
            mistral_success_response(r#"{"status":"ok"}"#),
        )
    }

    fn xai_success() -> Self {
        Self::respond_after_at(
            "/v1/responses",
            Duration::ZERO,
            200,
            xai_success_response(r#"{"status":"ok"}"#),
        )
    }

    fn openai_success() -> Self {
        Self::respond_after_at(
            "/v1/chat/completions",
            Duration::ZERO,
            200,
            openai_success_response(r#"{"status":"ok"}"#),
        )
    }

    fn ollama_success() -> Self {
        Self::respond_after_at(
            "/v1/chat/completions",
            Duration::ZERO,
            200,
            ollama_success_response(r#"{"status":"ok"}"#),
        )
    }

    fn respond_after(delay: Duration, status: u16, body: String) -> Self {
        Self::respond_after_at("/v1/messages", delay, status, body)
    }

    fn respond_after_at(path: &str, delay: Duration, status: u16, body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock listener should bind");
        let address = listener.local_addr().expect("mock address should resolve");
        let request = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock should accept one request");
            let request = read_http_request(&mut stream);
            thread::sleep(delay);
            let reason = if status == 200 { "OK" } else { "Error" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            request
        });
        Self {
            url: format!("http://{address}{path}"),
            request,
        }
    }

    fn finish(self) -> String {
        self.request.join().expect("mock server should finish")
    }
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout should configure");
    let started = Instant::now();
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut expected_len = None;
    loop {
        let count = match stream.read(&mut buffer) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && started.elapsed() < Duration::from_secs(30) =>
            {
                continue;
            }
            Err(error) => panic!("request should be readable: {error}"),
        };
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(header_end) = find_header_end(&bytes) {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            expected_len = Some(header_end + 4 + content_length);
        }
        if expected_len.is_some_and(|length| bytes.len() >= length) {
            break;
        }
    }
    String::from_utf8(bytes).expect("request should be UTF-8")
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn run_args(fixture: &Fixture, url: &str) -> Vec<String> {
    vec![
        "--server".into(),
        "anthropic".into(),
        "--model".into(),
        "claude-smoke".into(),
        "--url".into(),
        url.into(),
        "--token".into(),
        TEST_TOKEN.into(),
        "--max-output-tokens".into(),
        "128".into(),
        "--input-text".into(),
        "Describe the image and return status ok.".into(),
        "--input-image".into(),
        fixture.image.display().to_string(),
        "--usage-log".into(),
        fixture.usage.display().to_string(),
        "--render-mode".into(),
        "append-only".into(),
    ]
}

fn assert_success(output: &Output, request: &str, usage_path: &Path) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "provider smoke failed\n{stdout}\n{stderr}"
    );
    let normalized_request = request.to_ascii_lowercase();
    assert!(normalized_request.contains("post /v1/messages http/1.1"));
    assert!(normalized_request.contains("x-api-key: anthropic-provider-smoke-token"));
    assert!(normalized_request.contains("anthropic-version: 2023-06-01"));
    let body = request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("request should contain a body");
    let body: Value = serde_json::from_str(body).expect("request body should be JSON");
    assert_eq!(body["model"], "claude-smoke");
    assert_eq!(body["max_tokens"], 128);
    assert!(
        body.get("temperature").is_none(),
        "Anthropic requests must omit model-deprecated sampling controls"
    );
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    assert!(body["messages"][0]["content"]
        .as_array()
        .expect("content should be an array")
        .iter()
        .any(|part| part["type"] == "image"));

    let events = fs::read_to_string(usage_path).expect("usage log should exist");
    let provider_event = events
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["event_type"] == "provider_request_completed")
        .expect("provider usage event should be recorded");
    assert_eq!(provider_event["provider"]["server"], "anthropic");
    assert_eq!(provider_event["usage"]["input_tokens"], 12);
    assert_eq!(provider_event["usage"]["output_tokens"], 5);
    assert_eq!(provider_event["usage"]["total_tokens"], 17);
    assert!(!events.contains(TEST_TOKEN));
}

fn gemini_run_args(fixture: &Fixture, url: &str) -> Vec<String> {
    vec![
        "--server".into(),
        "gemini".into(),
        "--model".into(),
        "gemini-smoke".into(),
        "--url".into(),
        url.into(),
        "--token".into(),
        GEMINI_TEST_TOKEN.into(),
        "--max-output-tokens".into(),
        "128".into(),
        "--input-text".into(),
        "Describe the image and return status ok.".into(),
        "--input-image".into(),
        fixture.image.display().to_string(),
        "--usage-log".into(),
        fixture.usage.display().to_string(),
        "--render-mode".into(),
        "append-only".into(),
    ]
}

fn assert_gemini_success(output: &Output, request: &str, usage_path: &Path) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Gemini smoke failed\n{stdout}\n{stderr}"
    );
    let normalized_request = request.to_ascii_lowercase();
    assert!(normalized_request.contains("post /v1beta/interactions http/1.1"));
    assert!(normalized_request.contains("x-goog-api-key: gemini-provider-smoke-token"));
    let body = request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("request should contain a body");
    let body: Value = serde_json::from_str(body).expect("request body should be JSON");
    assert_eq!(body["model"], "gemini-smoke");
    assert_eq!(body["store"], false);
    assert_eq!(body["generation_config"]["max_output_tokens"], 128);
    assert_eq!(body["response_format"]["type"], "text");
    assert_eq!(body["response_format"]["mime_type"], "application/json");
    assert!(body["input"]
        .as_array()
        .expect("input should be an array")
        .iter()
        .any(|part| part["type"] == "image"));

    let events = fs::read_to_string(usage_path).expect("usage log should exist");
    let provider_event = events
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["event_type"] == "provider_request_completed")
        .expect("provider usage event should be recorded");
    assert_eq!(provider_event["provider"]["server"], "gemini");
    assert_eq!(provider_event["usage"]["input_tokens"], 13);
    assert_eq!(provider_event["usage"]["output_tokens"], 5);
    assert_eq!(provider_event["usage"]["total_tokens"], 20);
    assert!(!events.contains(GEMINI_TEST_TOKEN));
}

fn hosted_run_args(
    provider: &str,
    model: &str,
    token: &str,
    fixture: &Fixture,
    url: &str,
) -> Vec<String> {
    vec![
        "--server".into(),
        provider.into(),
        "--model".into(),
        model.into(),
        "--url".into(),
        url.into(),
        "--token".into(),
        token.into(),
        "--max-output-tokens".into(),
        "128".into(),
        "--usage-log".into(),
        fixture.usage.display().to_string(),
        "--render-mode".into(),
        "append-only".into(),
    ]
}

fn assert_hosted_success(
    provider: &str,
    model: &str,
    token: &str,
    output: &Output,
    request: &str,
    usage_path: &Path,
) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{provider} smoke failed\n{stdout}\n{stderr}"
    );
    let normalized_request = request.to_ascii_lowercase();
    assert!(normalized_request.contains(&format!("authorization: bearer {token}")));
    let body = request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("request should contain a body");
    let body: Value = serde_json::from_str(body).expect("request body should be JSON");
    assert_eq!(body["model"], model);
    match provider {
        "mistral" => {
            assert!(normalized_request.contains("post /v1/chat/completions http/1.1"));
            assert_eq!(body["max_tokens"], 128);
            assert_eq!(body["response_format"]["type"], "json_schema");
            assert_eq!(body["response_format"]["json_schema"]["strict"], true);
            assert_eq!(
                body["response_format"]["json_schema"]["schema"]["additionalProperties"],
                false
            );
        }
        "xai" => {
            assert!(normalized_request.contains("post /v1/responses http/1.1"));
            assert_eq!(body["store"], false);
            assert_eq!(body["max_output_tokens"], 128);
            assert_eq!(body["text"]["format"]["type"], "json_schema");
            assert_eq!(body["text"]["format"]["strict"], true);
            assert_eq!(
                body["text"]["format"]["schema"]["additionalProperties"],
                false
            );
        }
        _ => panic!("unexpected hosted provider {provider}"),
    }

    let events = fs::read_to_string(usage_path).expect("usage log should exist");
    let provider_event = events
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["event_type"] == "provider_request_completed")
        .expect("provider usage event should be recorded");
    assert_eq!(provider_event["provider"]["server"], provider);
    assert!(!events.contains(token));
}

fn run_interpreted_hosted_smoke(
    fixture: &Fixture,
    provider: &str,
    model: &str,
    token: &str,
    mock: MockServer,
) {
    let mut command = fixture.isolated_command(env!("CARGO_BIN_EXE_cargo-ai"));
    command
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args(hosted_run_args(provider, model, token, fixture, &mock.url));
    let output = command
        .output()
        .expect("interpreted hosted CLI should start");
    let request = mock.finish();
    assert_hosted_success(provider, model, token, &output, &request, &fixture.usage);
}

fn run_generated_hosted_smoke(
    fixture: &Fixture,
    binary_name: &str,
    provider: &str,
    model: &str,
    token: &str,
    mock: MockServer,
) {
    let output_dir = fixture.root.join("dist");
    let hatch = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "hatch", binary_name, "--config"])
        .arg(&fixture.definition)
        .args(["--output-dir"])
        .arg(&output_dir)
        .arg("--force")
        .output()
        .expect("hosted provider hatch should start");
    if fixture.home.join("batch-seed-marker").exists() {
        assert!(
            String::from_utf8_lossy(&hatch.stdout).contains("Reused warmed template"),
            "batch case should reuse its copied seed"
        );
        eprintln!(
            "generated-provider hatch: {}",
            String::from_utf8_lossy(&hatch.stderr)
        );
    }
    assert!(
        hatch.status.success(),
        "{provider} hatch failed\n{}\n{}",
        String::from_utf8_lossy(&hatch.stdout),
        String::from_utf8_lossy(&hatch.stderr)
    );
    let executable = output_dir.join(if cfg!(windows) {
        format!("{binary_name}.exe")
    } else {
        binary_name.to_string()
    });
    let output = fixture
        .isolated_command(&executable)
        .args(hosted_run_args(provider, model, token, fixture, &mock.url))
        .output()
        .expect("generated hosted agent should start");
    let request = mock.finish();
    assert_hosted_success(provider, model, token, &output, &request, &fixture.usage);
}

fn openai_compatible_run_args(
    provider: &str,
    model: &str,
    token: Option<&str>,
    fixture: &Fixture,
    url: &str,
) -> Vec<String> {
    let mut args = vec![
        "--server".into(),
        provider.into(),
        "--model".into(),
        model.into(),
        "--url".into(),
        url.into(),
        "--max-output-tokens".into(),
        "128".into(),
        "--usage-log".into(),
        fixture.usage.display().to_string(),
        "--render-mode".into(),
        "append-only".into(),
    ];
    if let Some(token) = token {
        args.extend(["--token".into(), token.into()]);
    }
    args
}

fn assert_openai_compatible_success(
    provider: &str,
    model: &str,
    token: Option<&str>,
    output: &Output,
    request: &str,
    usage_path: &Path,
) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{provider} smoke failed\n{stdout}\n{stderr}"
    );
    let normalized_request = request.to_ascii_lowercase();
    assert!(normalized_request.contains("post /v1/chat/completions http/1.1"));
    match token {
        Some(token) => {
            assert!(normalized_request.contains(&format!("authorization: bearer {token}")))
        }
        None => assert!(!normalized_request.contains("authorization:")),
    }
    let body = request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("request should contain a body");
    let body: Value = serde_json::from_str(body).expect("request body should be JSON");
    assert_eq!(body["model"], model);
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    assert_eq!(
        body["response_format"]["json_schema"]["schema"]["additionalProperties"],
        false
    );

    let events = fs::read_to_string(usage_path).expect("usage log should exist");
    let provider_event = events
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["event_type"] == "provider_request_completed")
        .expect("provider usage event should be recorded");
    assert_eq!(provider_event["provider"]["server"], provider);
    if provider == "ollama" {
        assert_eq!(provider_event["usage"]["input_tokens"], 8);
        assert_eq!(provider_event["usage"]["output_tokens"], 3);
        assert_eq!(provider_event["usage"]["total_tokens"], 11);
    } else {
        assert_eq!(provider_event["usage"]["input_tokens"], 16);
        assert_eq!(provider_event["usage"]["output_tokens"], 6);
        assert_eq!(provider_event["usage"]["total_tokens"], 22);
    }
    if let Some(token) = token {
        assert!(!events.contains(token));
    }
}

fn run_interpreted_openai_compatible_smoke(
    fixture: &Fixture,
    provider: &str,
    model: &str,
    token: Option<&str>,
    mock: MockServer,
) {
    let output = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args(openai_compatible_run_args(
            provider, model, token, fixture, &mock.url,
        ))
        .output()
        .expect("interpreted OpenAI-compatible CLI should start");
    let request = mock.finish();
    assert_openai_compatible_success(provider, model, token, &output, &request, &fixture.usage);
}

fn run_generated_openai_compatible_smoke(
    fixture: &Fixture,
    binary_name: &str,
    provider: &str,
    model: &str,
    token: Option<&str>,
    mock: MockServer,
) {
    let output_dir = fixture.root.join("dist");
    let hatch = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "hatch", binary_name, "--config"])
        .arg(&fixture.definition)
        .args(["--output-dir"])
        .arg(&output_dir)
        .arg("--force")
        .output()
        .expect("OpenAI-compatible hatch should start");
    if fixture.home.join("batch-seed-marker").exists() {
        assert!(
            String::from_utf8_lossy(&hatch.stdout).contains("Reused warmed template"),
            "batch case should reuse its copied seed"
        );
        eprintln!(
            "generated-provider hatch: {}",
            String::from_utf8_lossy(&hatch.stderr)
        );
    }
    assert!(
        hatch.status.success(),
        "{provider} hatch failed\n{}\n{}",
        String::from_utf8_lossy(&hatch.stdout),
        String::from_utf8_lossy(&hatch.stderr)
    );
    let executable = output_dir.join(if cfg!(windows) {
        format!("{binary_name}.exe")
    } else {
        binary_name.to_string()
    });
    let output = fixture
        .isolated_command(&executable)
        .args(openai_compatible_run_args(
            provider, model, token, fixture, &mock.url,
        ))
        .output()
        .expect("generated OpenAI-compatible agent should start");
    let request = mock.finish();
    assert_openai_compatible_success(provider, model, token, &output, &request, &fixture.usage);
}

#[test]
fn interpreted_anthropic_smoke_isolated_and_deterministic() {
    let fixture = Fixture::new();
    let mock = MockServer::success();
    let mut command = fixture.isolated_command(env!("CARGO_BIN_EXE_cargo-ai"));
    command
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args(run_args(&fixture, &mock.url));
    let output = command.output().expect("interpreted CLI should start");
    let request = mock.finish();
    assert_success(&output, &request, &fixture.usage);
}

#[test]
#[ignore = "run explicitly in the provider smoke CI lane"]
fn generated_anthropic_smoke_isolated_and_deterministic() {
    generated_anthropic_case(&Fixture::new());
}

fn generated_anthropic_case(fixture: &Fixture) {
    let output_dir = fixture.root.join("dist");
    let hatch = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args([
            "--no-update-check",
            "hatch",
            "anthropic_provider_smoke",
            "--config",
        ])
        .arg(&fixture.definition)
        .args(["--output-dir"])
        .arg(&output_dir)
        .arg("--force")
        .output()
        .expect("hatch should start");
    if fixture.home.join("batch-seed-marker").exists() {
        assert!(
            String::from_utf8_lossy(&hatch.stdout).contains("Reused warmed template"),
            "batch case should reuse its copied seed"
        );
        eprintln!(
            "generated-provider hatch: {}",
            String::from_utf8_lossy(&hatch.stderr)
        );
    }
    assert!(
        hatch.status.success(),
        "hatch failed\n{}\n{}",
        String::from_utf8_lossy(&hatch.stdout),
        String::from_utf8_lossy(&hatch.stderr)
    );

    let executable = output_dir.join(if cfg!(windows) {
        "anthropic_provider_smoke.exe"
    } else {
        "anthropic_provider_smoke"
    });
    let mock = MockServer::success();
    let output = fixture
        .isolated_command(&executable)
        .args(run_args(&fixture, &mock.url))
        .output()
        .expect("generated agent should start");
    let request = mock.finish();
    assert_success(&output, &request, &fixture.usage);
}

#[test]
fn interpreted_gemini_smoke_isolated_and_deterministic() {
    let fixture = Fixture::new();
    let mock = MockServer::gemini_success();
    let mut command = fixture.isolated_command(env!("CARGO_BIN_EXE_cargo-ai"));
    command
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args(gemini_run_args(&fixture, &mock.url));
    let output = command
        .output()
        .expect("interpreted Gemini CLI should start");
    let request = mock.finish();
    assert_gemini_success(&output, &request, &fixture.usage);
}

#[test]
#[ignore = "run explicitly in the provider smoke CI lane"]
fn generated_gemini_smoke_isolated_and_deterministic() {
    generated_gemini_case(&Fixture::new());
}

fn generated_gemini_case(fixture: &Fixture) {
    let output_dir = fixture.root.join("dist");
    let hatch = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args([
            "--no-update-check",
            "hatch",
            "gemini_provider_smoke",
            "--config",
        ])
        .arg(&fixture.definition)
        .args(["--output-dir"])
        .arg(&output_dir)
        .arg("--force")
        .output()
        .expect("hatch should start");
    if fixture.home.join("batch-seed-marker").exists() {
        assert!(
            String::from_utf8_lossy(&hatch.stdout).contains("Reused warmed template"),
            "batch case should reuse its copied seed"
        );
        eprintln!(
            "generated-provider hatch: {}",
            String::from_utf8_lossy(&hatch.stderr)
        );
    }
    assert!(
        hatch.status.success(),
        "Gemini hatch failed\n{}\n{}",
        String::from_utf8_lossy(&hatch.stdout),
        String::from_utf8_lossy(&hatch.stderr)
    );

    let executable = output_dir.join(if cfg!(windows) {
        "gemini_provider_smoke.exe"
    } else {
        "gemini_provider_smoke"
    });
    let mock = MockServer::gemini_success();
    let output = fixture
        .isolated_command(&executable)
        .args(gemini_run_args(&fixture, &mock.url))
        .output()
        .expect("generated Gemini agent should start");
    let request = mock.finish();
    assert_gemini_success(&output, &request, &fixture.usage);
}

#[test]
fn interpreted_mistral_smoke_isolated_and_deterministic() {
    let fixture = Fixture::new();
    run_interpreted_hosted_smoke(
        &fixture,
        "mistral",
        "mistral-smoke",
        MISTRAL_TEST_TOKEN,
        MockServer::mistral_success(),
    );
}

#[test]
#[ignore = "run explicitly in the provider smoke CI lane"]
fn generated_mistral_smoke_isolated_and_deterministic() {
    generated_mistral_case(&Fixture::new());
}

fn generated_mistral_case(fixture: &Fixture) {
    run_generated_hosted_smoke(
        &fixture,
        "mistral_provider_smoke",
        "mistral",
        "mistral-smoke",
        MISTRAL_TEST_TOKEN,
        MockServer::mistral_success(),
    );
}

#[test]
fn interpreted_xai_smoke_isolated_and_deterministic() {
    let fixture = Fixture::new();
    run_interpreted_hosted_smoke(
        &fixture,
        "xai",
        "grok-smoke",
        XAI_TEST_TOKEN,
        MockServer::xai_success(),
    );
}

#[test]
#[ignore = "run explicitly in the provider smoke CI lane"]
fn generated_xai_smoke_isolated_and_deterministic() {
    generated_xai_case(&Fixture::new());
}

fn generated_xai_case(fixture: &Fixture) {
    run_generated_hosted_smoke(
        &fixture,
        "xai_provider_smoke",
        "xai",
        "grok-smoke",
        XAI_TEST_TOKEN,
        MockServer::xai_success(),
    );
}

#[test]
fn interpreted_openai_smoke_isolated_and_deterministic() {
    let fixture = Fixture::new();
    run_interpreted_openai_compatible_smoke(
        &fixture,
        "openai",
        "openai-smoke",
        Some(OPENAI_TEST_TOKEN),
        MockServer::openai_success(),
    );
}

#[test]
#[ignore = "run explicitly in the provider smoke CI lane"]
fn generated_openai_smoke_isolated_and_deterministic() {
    generated_openai_case(&Fixture::new());
}

fn generated_openai_case(fixture: &Fixture) {
    run_generated_openai_compatible_smoke(
        &fixture,
        "openai_provider_smoke",
        "openai",
        "openai-smoke",
        Some(OPENAI_TEST_TOKEN),
        MockServer::openai_success(),
    );
}

#[test]
fn interpreted_ollama_smoke_isolated_and_deterministic() {
    let fixture = Fixture::new();
    run_interpreted_openai_compatible_smoke(
        &fixture,
        "ollama",
        "ollama-smoke",
        None,
        MockServer::ollama_success(),
    );
}

#[test]
#[ignore = "run explicitly in the provider smoke CI lane"]
fn generated_ollama_smoke_isolated_and_deterministic() {
    generated_ollama_case(&Fixture::new());
}

fn generated_ollama_case(fixture: &Fixture) {
    run_generated_openai_compatible_smoke(
        &fixture,
        "ollama_provider_smoke",
        "ollama",
        "ollama-smoke",
        None,
        MockServer::ollama_success(),
    );
}

#[test]
#[ignore = "run explicitly in the provider smoke CI lane"]
fn generated_provider_batch_isolated_and_deterministic() {
    let seed = Fixture::new();
    let identity =
        provider_cache::CacheIdentity::current(Path::new(env!("CARGO_BIN_EXE_cargo-ai")));
    let started = Instant::now();
    let hatch = seed
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args([
            "--no-update-check",
            "hatch",
            "neutral_provider_seed",
            "--config",
        ])
        .arg(&seed.definition)
        .arg("--output-dir")
        .arg(seed.root.join("dist"))
        .arg("--force")
        .output()
        .unwrap();
    assert!(
        hatch.status.success(),
        "neutral seed failed: {}\n{}",
        String::from_utf8_lossy(&hatch.stdout),
        String::from_utf8_lossy(&hatch.stderr)
    );
    let cache = provider_cache::SeedCache::capture(&seed.home, identity.clone()).unwrap();
    eprintln!(
        "generated-provider neutral-seed: {:.2}s",
        started.elapsed().as_secs_f64()
    );
    let cases: [(&str, fn(&Fixture)); 6] = [
        ("anthropic", generated_anthropic_case),
        ("gemini", generated_gemini_case),
        ("mistral", generated_mistral_case),
        ("xai", generated_xai_case),
        ("openai", generated_openai_case),
        ("ollama", generated_ollama_case),
    ];
    let mut completed = Vec::new();
    for (provider, case) in cases {
        let fixture = Fixture::new();
        let copied = Instant::now();
        cache.copy_into(&fixture.home, &identity).unwrap();
        eprintln!(
            "generated-provider {provider} copy: {:.2}s",
            copied.elapsed().as_secs_f64()
        );
        fs::write(fixture.home.join("batch-seed-marker"), "neutral").unwrap();
        let mut definition: Value = serde_json::from_str(definition_json()).unwrap();
        definition["inputs"][0]["text"] =
            Value::String(format!("Return a short status. provider_case_{provider}"));
        fs::write(
            &fixture.definition,
            serde_json::to_vec(&definition).unwrap(),
        )
        .unwrap();
        let execution = Instant::now();
        case(&fixture);
        fs::write(fixture.home.join("provider-case-output"), provider).unwrap();
        cache.unchanged().unwrap();
        completed.push(provider);
        eprintln!(
            "generated-provider {provider}: passed, assembly/execution {:.2}s",
            execution.elapsed().as_secs_f64()
        );
    }
    assert_eq!(
        completed,
        ["anthropic", "gemini", "mistral", "xai", "openai", "ollama"]
    );
    eprintln!(
        "generated-provider batch: 6/6 passed in {:.2}s",
        started.elapsed().as_secs_f64()
    );
}

fn marker_definition(fixture: &Fixture) -> PathBuf {
    let marker = fixture.root.join("action-ran.txt");
    let (program, args, platform) = if cfg!(windows) {
        (
            "cmd",
            vec![
                Value::String("/C".to_string()),
                Value::String(format!("echo ran>\"{}\"", marker.display())),
            ],
            "windows",
        )
    } else {
        (
            "/bin/sh",
            vec![
                Value::String("-c".to_string()),
                Value::String(format!("printf ran > '{}'", marker.display())),
            ],
            if cfg!(target_os = "macos") {
                "macos"
            } else {
                "linux"
            },
        )
    };
    let definition = fixture.root.join("fail_closed.json");
    fs::write(
        &definition,
        serde_json::to_vec_pretty(&serde_json::json!({
            "agent_definition_schema_version": "2026-03-03.r1",
            "inputs": [{"type": "text", "text": "Return status."}],
            "agent_schema": {
                "type": "object",
                "properties": {"status": {"type": "string"}},
                "required": ["status"],
                "additionalProperties": false
            },
            "actions": [{
                "name": "must_not_run",
                "logic": {"==": [1, 1]},
                "run": [{"kind": "exec", "program": program, "args": args, "platform": platform}]
            }]
        }))
        .expect("marker definition should serialize"),
    )
    .expect("marker definition should be written");
    definition
}

fn hosted_response(provider: &str, output: &str) -> String {
    match provider {
        "mistral" => mistral_success_response(output),
        "xai" => xai_success_response(output),
        _ => panic!("unexpected hosted provider {provider}"),
    }
}

fn hosted_path(provider: &str) -> &'static str {
    match provider {
        "mistral" => "/v1/chat/completions",
        "xai" => "/v1/responses",
        _ => panic!("unexpected hosted provider {provider}"),
    }
}

fn hosted_test_values(provider: &str) -> (&'static str, &'static str) {
    match provider {
        "mistral" => ("mistral-smoke", MISTRAL_TEST_TOKEN),
        "xai" => ("grok-smoke", XAI_TEST_TOKEN),
        _ => panic!("unexpected hosted provider {provider}"),
    }
}

fn run_hosted_failure(fixture: &Fixture, provider: &str, mock: MockServer) -> Output {
    let (model, token) = hosted_test_values(provider);
    let mut command = fixture.isolated_command(env!("CARGO_BIN_EXE_cargo-ai"));
    command
        .args(["--no-update-check", "run", "--config"])
        .arg(marker_definition(fixture))
        .args([
            "--server",
            provider,
            "--model",
            model,
            "--url",
            &mock.url,
            "--token",
            token,
            "--render-mode",
            "append-only",
        ]);
    let output = command.output().expect("hosted failure smoke should start");
    let _ = mock.finish();
    assert!(
        !output.status.success(),
        "invalid provider output must fail"
    );
    assert!(
        !fixture.root.join("action-ran.txt").exists(),
        "downstream action must not run after invalid output"
    );
    output
}

fn assert_hosted_fail_closed_matrix(provider: &str) {
    for (name, returned_json, expected) in [
        ("wrong-type", r#"{"status":7}"#, "required JSON schema"),
        ("missing-required", r#"{}"#, "required JSON schema"),
        (
            "unexpected-field",
            r#"{"status":"ok","extra":true}"#,
            "required JSON schema",
        ),
    ] {
        let fixture = Fixture::new();
        let mock = MockServer::respond_after_at(
            hosted_path(provider),
            Duration::ZERO,
            200,
            hosted_response(provider, returned_json),
        );
        let output = run_hosted_failure(&fixture, provider, mock);
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains(expected),
            "unexpected {provider} {name} diagnostic:\n{text}"
        );
    }

    let fixture = Fixture::new();
    let mock = MockServer::respond_after_at(
        hosted_path(provider),
        Duration::ZERO,
        200,
        hosted_response(provider, "not-json"),
    );
    let output = run_hosted_failure(&fixture, provider, mock);
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("required JSON schema"),
        "unexpected malformed diagnostic:\n{text}"
    );

    let fixture = Fixture::new();
    let provider_message = "selected model rejected json_schema";
    let body = if provider == "xai" {
        serde_json::json!({"error": {"message": provider_message}, "debug": "secret-debug"})
    } else {
        serde_json::json!({"message": provider_message, "debug": "secret-debug"})
    };
    let mock =
        MockServer::respond_after_at(hosted_path(provider), Duration::ZERO, 400, body.to_string());
    let output = run_hosted_failure(&fixture, provider, mock);
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains(provider_message),
        "provider rejection should be preserved:\n{text}"
    );
    assert!(
        !text.contains("secret-debug"),
        "unselected provider fields must be redacted"
    );
}

#[test]
fn mistral_invalid_returns_fail_closed_before_actions() {
    assert_hosted_fail_closed_matrix("mistral");
}

#[test]
fn xai_invalid_returns_fail_closed_before_actions() {
    assert_hosted_fail_closed_matrix("xai");
}

fn assert_hosted_timeout_and_capability_failures(provider: &str) {
    let fixture = Fixture::new();
    let (model, token) = hosted_test_values(provider);
    let mock = MockServer::respond_after_at(
        hosted_path(provider),
        Duration::from_secs(2),
        200,
        hosted_response(provider, r#"{"status":"ok"}"#),
    );
    let timeout = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args([
            "--server",
            provider,
            "--model",
            model,
            "--url",
            &mock.url,
            "--token",
            token,
            "--inference-timeout-in-sec",
            "1",
        ])
        .output()
        .expect("hosted timeout smoke should start");
    let _ = mock.finish();
    let timeout_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&timeout.stdout),
        String::from_utf8_lossy(&timeout.stderr)
    );
    assert!(!timeout.status.success());
    assert!(timeout_text.to_ascii_lowercase().contains("timed out"));

    let image_failure = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args([
            "--server",
            provider,
            "--model",
            model,
            "--token",
            token,
            "--input-image",
        ])
        .arg(&fixture.image)
        .output()
        .expect("hosted image capability smoke should start");
    let image_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&image_failure.stdout),
        String::from_utf8_lossy(&image_failure.stderr)
    );
    assert!(!image_failure.status.success());
    assert!(
        image_text
            .to_ascii_lowercase()
            .contains("image inputs are not supported"),
        "unexpected {provider} image diagnostic:\n{image_text}"
    );

    let file = fixture.root.join("document.pdf");
    fs::write(&file, b"not a real PDF").expect("file fixture should be written");
    let file_failure = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args([
            "--server",
            provider,
            "--model",
            model,
            "--token",
            token,
            "--input-file",
        ])
        .arg(&file)
        .output()
        .expect("hosted file capability smoke should start");
    let file_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&file_failure.stdout),
        String::from_utf8_lossy(&file_failure.stderr)
    );
    assert!(!file_failure.status.success());
    assert!(
        file_text
            .to_ascii_lowercase()
            .contains("file inputs are not supported"),
        "unexpected {provider} file diagnostic:\n{file_text}"
    );

    let image_action_definition = fixture.root.join("unsupported_image_action.json");
    fs::write(
        &image_action_definition,
        r#"{
  "agent_definition_schema_version": "2026-03-03.r1",
  "inputs": [{"name":"request","type":"text","text":"Create an image."}],
  "agent_schema": {"type":"object","properties":{}},
  "actions": [{
    "name": "unsupported_image_generation",
    "logic": {"==":[1,1]},
    "run": [{
      "kind": "generate_image",
      "model": "image-model",
      "prompt": ["Create an image."],
      "path": ["./output.png"]
    }]
  }]
}"#,
    )
    .expect("image action definition should be written");
    let generate_failure = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "run", "--config"])
        .arg(&image_action_definition)
        .args([
            "--server",
            provider,
            "--model",
            model,
            "--token",
            token,
            "--render-mode",
            "append-only",
        ])
        .output()
        .expect("hosted generate_image capability smoke should start");
    let generate_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&generate_failure.stdout),
        String::from_utf8_lossy(&generate_failure.stderr)
    );
    assert!(!generate_failure.status.success());
    assert!(
        generate_text.contains("generate_image is not supported"),
        "unexpected {provider} generate_image diagnostic:\n{generate_text}"
    );
}

#[test]
fn mistral_timeout_and_capability_failures_are_actionable() {
    assert_hosted_timeout_and_capability_failures("mistral");
}

#[test]
fn xai_timeout_and_capability_failures_are_actionable() {
    assert_hosted_timeout_and_capability_failures("xai");
}

#[test]
fn gemini_timeout_and_file_failures_are_actionable() {
    let fixture = Fixture::new();
    let mock = MockServer::respond_after_at(
        "/v1beta/interactions",
        Duration::from_secs(2),
        200,
        gemini_success_response(),
    );
    let timeout = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args([
            "--server",
            "gemini",
            "--model",
            "gemini-smoke",
            "--url",
            &mock.url,
            "--token",
            GEMINI_TEST_TOKEN,
            "--inference-timeout-in-sec",
            "1",
        ])
        .output()
        .expect("Gemini timeout smoke should start");
    let _ = mock.finish();
    let timeout_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&timeout.stdout),
        String::from_utf8_lossy(&timeout.stderr)
    );
    assert!(!timeout.status.success());
    assert!(timeout_text.to_ascii_lowercase().contains("timed out"));

    let file = fixture.root.join("document.pdf");
    fs::write(&file, b"not a real PDF").expect("file fixture should be written");
    let file_failure = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args([
            "--server",
            "gemini",
            "--model",
            "gemini-smoke",
            "--token",
            GEMINI_TEST_TOKEN,
            "--input-file",
        ])
        .arg(&file)
        .output()
        .expect("Gemini file capability smoke should start");
    let file_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&file_failure.stdout),
        String::from_utf8_lossy(&file_failure.stderr)
    );
    assert!(!file_failure.status.success());
    assert!(
        file_text
            .to_ascii_lowercase()
            .contains("file inputs are not supported by the google gemini adapter"),
        "unexpected Gemini file-capability diagnostic:\n{file_text}"
    );
}

#[test]
fn anthropic_timeout_and_file_failures_are_actionable() {
    let fixture = Fixture::new();
    let mock = MockServer::respond_after(Duration::from_secs(2), 200, success_response());
    let timeout = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args([
            "--server",
            "anthropic",
            "--model",
            "claude-smoke",
            "--url",
            &mock.url,
            "--token",
            TEST_TOKEN,
            "--inference-timeout-in-sec",
            "1",
        ])
        .output()
        .expect("timeout smoke should start");
    let _ = mock.finish();
    let timeout_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&timeout.stdout),
        String::from_utf8_lossy(&timeout.stderr)
    );
    assert!(!timeout.status.success());
    assert!(timeout_text.to_ascii_lowercase().contains("timed out"));

    let file = fixture.root.join("document.pdf");
    fs::write(&file, b"not a real PDF").expect("file fixture should be written");
    let file_failure = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args([
            "--server",
            "anthropic",
            "--model",
            "claude-smoke",
            "--token",
            TEST_TOKEN,
            "--input-file",
        ])
        .arg(&file)
        .output()
        .expect("file capability smoke should start");
    let file_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&file_failure.stdout),
        String::from_utf8_lossy(&file_failure.stderr)
    );
    assert!(!file_failure.status.success());
    assert!(
        file_text
            .to_ascii_lowercase()
            .contains("file inputs are not supported by the anthropic adapter"),
        "unexpected file-capability diagnostic:\n{file_text}"
    );

    let image_action_definition = fixture.root.join("anthropic_image_action.json");
    fs::write(
        &image_action_definition,
        r#"{
  "agent_definition_schema_version": "2026-03-03.r1",
  "inputs": [{"name":"request","type":"text","text":"Create an image."}],
  "agent_schema": {"type":"object","properties":{}},
  "actions": [{
    "name": "unsupported_image_generation",
    "logic": {"==":[1,1]},
    "run": [{
      "kind": "generate_image",
      "model": "claude-smoke",
      "prompt": ["Create an image."],
      "path": ["./output.png"]
    }]
  }]
}"#,
    )
    .expect("image action definition should be written");
    let image_failure = fixture
        .isolated_command(env!("CARGO_BIN_EXE_cargo-ai"))
        .args(["--no-update-check", "run", "--config"])
        .arg(&image_action_definition)
        .args([
            "--server",
            "anthropic",
            "--model",
            "claude-smoke",
            "--token",
            TEST_TOKEN,
            "--render-mode",
            "append-only",
        ])
        .output()
        .expect("image capability smoke should start");
    let image_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&image_failure.stdout),
        String::from_utf8_lossy(&image_failure.stderr)
    );
    assert!(!image_failure.status.success());
    assert!(
        image_text.contains("generate_image is not supported by the Anthropic adapter"),
        "unexpected image-capability diagnostic:\n{image_text}"
    );
}

#[test]
#[ignore = "requires ANTHROPIC_API_KEY and ANTHROPIC_MODEL"]
fn live_anthropic_smoke_uses_isolated_stdin_credentials() {
    run_live_hosted_smoke("anthropic", "ANTHROPIC_API_KEY", "ANTHROPIC_MODEL");
}

#[test]
#[ignore = "requires GEMINI_API_KEY and GEMINI_MODEL"]
fn live_gemini_smoke_uses_isolated_stdin_credentials() {
    run_live_hosted_smoke("gemini", "GEMINI_API_KEY", "GEMINI_MODEL");
}

fn run_live_hosted_smoke(provider: &str, key_env: &str, model_env: &str) {
    let api_key = std::env::var(key_env).unwrap_or_else(|_| panic!("{key_env} is required"));
    let model = std::env::var(model_env).unwrap_or_else(|_| panic!("{model_env} is required"));
    let report =
        qualification_report::Context::from_environment().expect("valid qualification mode");
    let fixture = Fixture::new();
    let run = run_profile_probe(&fixture, provider, &model, &api_key, None);
    if let Some(report) = report {
        report
            .write(
                provider,
                &model,
                &format!("{provider}-api"),
                &api_key,
                run.status.code(),
                &fixture.usage,
            )
            .expect("complete sanitized qualification report");
    } else {
        strict_live_probe(&run).expect("live provider smoke failed");
        let events = fs::read_to_string(&fixture.usage).expect("usage log should exist");
        assert!(events.contains(&format!("\"server\":\"{provider}\"")));
        assert!(!events.contains(&api_key));
    }
}

fn strict_live_probe(run: &Output) -> Result<(), &'static str> {
    if run.status.success() {
        Ok(())
    } else {
        Err("live provider smoke failed")
    }
}

fn run_profile_probe(
    fixture: &Fixture,
    provider: &str,
    model: &str,
    api_key: &str,
    url: Option<&str>,
) -> Output {
    assert!(
        !api_key.is_empty() && !model.trim().is_empty(),
        "probe credentials/model must be configured"
    );
    let cli = env!("CARGO_BIN_EXE_cargo-ai");
    let profile = format!("{provider}-api");
    let add = fixture
        .isolated_command(cli)
        .args([
            "--no-update-check",
            "profile",
            "add",
            &profile,
            "--server",
            provider,
            "--model",
            &model,
            "--auth",
            "api_key",
            "--max-output-tokens",
            "128",
        ])
        .output()
        .expect("hosted profile add should start");
    assert!(
        add.status.success(),
        "{provider} profile add should succeed"
    );

    let mut set = fixture.isolated_command(cli);
    let mut child = set
        .args(["--no-update-check", "profile", "set", &profile, "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hosted profile set should start");
    child
        .stdin
        .take()
        .expect("profile set stdin should exist")
        .write_all(api_key.as_bytes())
        .expect("hosted API key should be written to isolated profile stdin");
    let set_output = child.wait_with_output().expect("profile set should finish");
    assert!(
        set_output.status.success(),
        "{provider} token setup should succeed"
    );

    let mut command = fixture.isolated_command(cli);
    command
        .env_remove("CARGO_AI_USAGE_ROOT_RUN_ID")
        .env_remove("CARGO_AI_USAGE_PARENT_AGENT_RUN_ID");
    command
        .args(["--no-update-check", "run", "--config"])
        .arg(&fixture.definition)
        .args(["--profile", &profile, "--usage-log"])
        .arg(&fixture.usage);
    if let Some(url) = url {
        command.args(["--url", url]);
    }
    command.output().expect("profile probe should start")
}

#[test]
#[ignore = "requires MISTRAL_API_KEY and MISTRAL_MODEL"]
fn live_mistral_smoke_uses_isolated_stdin_credentials() {
    run_live_hosted_smoke("mistral", "MISTRAL_API_KEY", "MISTRAL_MODEL");
}

#[test]
#[ignore = "requires XAI_API_KEY and XAI_MODEL"]
fn live_xai_smoke_uses_isolated_stdin_credentials() {
    run_live_hosted_smoke("xai", "XAI_API_KEY", "XAI_MODEL");
}

#[test]
#[ignore = "requires OPENAI_API_KEY and OPENAI_MODEL"]
fn live_openai_smoke_uses_isolated_stdin_credentials() {
    run_live_hosted_smoke("openai", "OPENAI_API_KEY", "OPENAI_MODEL");
}

#[test]
fn qualification_reports_validate_real_probes_and_keep_strict_diagnostics() {
    let successes: [(&str, fn() -> MockServer); 5] = [
        ("anthropic", MockServer::success),
        ("gemini", MockServer::gemini_success),
        ("mistral", MockServer::mistral_success),
        ("xai", MockServer::xai_success),
        ("openai", MockServer::openai_success),
    ];
    for (provider, server) in successes {
        qualification_probe_case(provider, server(), "pass");
    }
    for (status, expected) in [
        (429, "rate_limited"),
        (401, "failure"),
        (400, "failure"),
        (500, "failure"),
    ] {
        qualification_probe_case(
            "mistral",
            MockServer::respond_after_at(
                "/v1/chat/completions",
                Duration::ZERO,
                status,
                r#"{"message":"private-response-sentinel"}"#.into(),
            ),
            expected,
        );
    }
    qualification_probe_case(
        "mistral",
        MockServer::respond_after_at(
            "/v1/chat/completions",
            Duration::ZERO,
            200,
            mistral_success_response(r#"{"status":false}"#),
        ),
        "failure",
    );
}

fn qualification_probe_case(provider: &str, mock: MockServer, expected: &str) {
    let fixture = Fixture::new();
    let model = "qualification-mock-model";
    let token = "qualification-fake-secret-sentinel";
    let output = run_profile_probe(&fixture, provider, model, token, Some(&mock.url));
    let request = mock.finish();
    assert!(request.contains(model));
    assert!(request.contains(token));
    let report = qualification_report::Context {
        path: fixture.root.join("report.json"),
        candidate: "a".repeat(40),
        run_id: "123".into(),
        run_attempt: "2".into(),
        probe_id: "b".repeat(32),
    };
    assert_eq!(
        report
            .write(
                provider,
                model,
                &format!("{provider}-api"),
                token,
                output.status.code(),
                &fixture.usage
            )
            .unwrap(),
        expected
    );
    let saved = fs::read(&report.path).unwrap();
    for forbidden in [token, model, "private-response-sentinel", "cps-"] {
        assert!(!String::from_utf8_lossy(&saved).contains(forbidden));
    }
    assert!(report
        .write(
            provider,
            model,
            &format!("{provider}-api"),
            token,
            output.status.code(),
            &fixture.usage
        )
        .is_err());
    assert_eq!(fs::read(&report.path).unwrap(), saved);
    if expected == "pass" {
        assert!(strict_live_probe(&output).is_ok());
    } else {
        assert!(strict_live_probe(&output).is_err());
    }
    // Exercise the actual producer/consumer boundary without a real workflow or service.
    let raw = fs::read(&report.path).unwrap();
    let decision = qualification_policy::evaluate(
        provider,
        &raw,
        &qualification_policy::Identity {
            candidate: &"a".repeat(40),
            run_id: "123",
            run_attempt: "2",
            probe_id: Some(&"b".repeat(32)),
        },
        "success",
        "true",
    )
    .unwrap();
    assert_eq!(
        decision.accepted,
        expected == "pass" || expected == "rate_limited"
    );

    if expected == "pass" && provider == "mistral" {
        let raw = fs::read_to_string(&fixture.usage).unwrap();
        let events: Vec<Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let serialize = |items: &[Value]| {
            items
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let classify = |value: &str, code| {
            qualification_report::classify(value, provider, model, &format!("{provider}-api"), code)
        };
        assert!(classify(&raw, None).is_err());
        assert!(classify(&raw, Some(2)).is_err());
        assert!(classify(&raw, Some(1)).is_err());
        assert!(classify(&serialize(&events[..4]), Some(0)).is_err());
        assert!(classify(&(raw.clone() + &raw), Some(0)).is_err());
        assert!(classify("invalid", Some(0)).is_err());
        for (index, field, value) in [
            (2, "root_run_id", Value::String("stale".into())),
            (2, "agent_run_id", Value::String("stale".into())),
            (2, "status", Value::String("failed".into())),
            (4, "status", Value::String("failed".into())),
        ] {
            let mut changed = events.clone();
            changed[index][field] = value;
            assert!(classify(&serialize(&changed), Some(0)).is_err());
        }
        let mut changed = events.clone();
        changed[2]["provider"]["server"] = Value::String("xai".into());
        assert!(classify(&serialize(&changed), Some(0)).is_err());
    }
}
