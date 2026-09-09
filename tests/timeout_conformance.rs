//! Measures timeout precedence through real CLI, generated-agent and tool-helper processes.

#[allow(dead_code)]
mod support;

use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::{assert_success, output_text, Fixture};

const TOOL: &str = include_str!("fixtures/timeout_conformance/tool.rs");

fn command(fixture: &Fixture, program: impl AsRef<std::ffi::OsStr>, cwd: &Path) -> Command {
    let mut paths = vec![Path::new(env!("CARGO_BIN_EXE_cargo-ai"))
        .parent()
        .unwrap()
        .to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let mut command = fixture.command(program, cwd);
    command
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("CARGO_NET_OFFLINE", "true")
        .env_remove("CARGO_AI_AGENT_ACTION_DEPTH")
        .env_remove("CARGO_AI_AGENT_ACTION_MAX_DEPTH")
        .env_remove("CARGO_AI_AGENT_MAX_RUNTIME_SECS")
        .env_remove("CARGO_AI_AGENT_RUNTIME_STARTED_AT_MS")
        .env_remove("CARGO_AI_AGENT_RUNTIME_DEADLINE_MS")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    command
}

fn cli(fixture: &Fixture, cwd: &Path, args: &[&str]) -> Output {
    command(fixture, env!("CARGO_BIN_EXE_cargo-ai"), cwd)
        .arg("--no-update-check")
        .args(args)
        .output()
        .expect("isolated CLI should start")
}

fn definition() -> Value {
    let executable = if cfg!(windows) {
        "./child.exe"
    } else {
        "./child"
    };
    let mut actions = Vec::new();
    for (mode, artifact, bridge, explicit) in [
        ("json", "./child.json", false, false),
        ("executable", executable, false, false),
        ("bridge_json", "./child.json", true, false),
        ("bridge_executable", executable, true, false),
        ("json_explicit", "./child.json", false, true),
        ("executable_explicit", executable, false, true),
    ] {
        let mut step = if bridge {
            json!({"kind":"tool", "name":"timeout_bridge", "params":{
                "artifact":artifact,
                "profile":{"var":"runtime.child_profile"},
                "child_mode":{"var":"runtime.child_mode"}
            }})
        } else {
            json!({"kind":"agent", "artifact":artifact,
                "run_vars":{"mode":{"var":"runtime.child_mode"}}})
        };
        if explicit {
            step["profile"] = json!("short");
        }
        actions.push(json!({
            "name":mode, "logic":{"==":[{"var":"runtime.mode"},mode]}, "run":[step]
        }));
    }
    json!({
        "agent_definition_schema_version":"2026-09-09.r1",
        "inputs":[{"type":"text","name":"request","text":"Return the fixture status."}],
        "runtime_vars":{
            "mode":{"type":"string","default":"none"},
            "child_profile":{"type":"string","default":""},
            "child_mode":{"type":"string","default":"none"}
        },
        "agent_schema":{"type":"object","properties":{"status":{"type":"string"}}},
        "actions":actions
    })
}

fn create_profiles(fixture: &Fixture, project: &Path) {
    for name in ["parent", "fallback", "short"] {
        assert_success(
            &cli(
                fixture,
                project,
                &[
                    "profile",
                    "add",
                    name,
                    "--server",
                    "openai",
                    "--model",
                    "timeout-fixture",
                    "--auth",
                    "api_key",
                ],
            ),
            "create fake profile",
        );
        let mut child = command(fixture, env!("CARGO_BIN_EXE_cargo-ai"), project)
            .args(["--no-update-check", "profile", "set", name, "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("fixture-{name}-token").as_bytes())
            .unwrap();
        assert_success(
            &child.wait_with_output().unwrap(),
            "store fake profile credential",
        );
    }
}

fn configure(fixture: &Fixture, project: &Path, url: &str, project_timeout: Option<i64>) {
    let config_path = fixture.cargo_ai_home.join("config.toml");
    let mut config: toml::Value =
        toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    config.as_table_mut().unwrap().insert(
        "default_profile".to_string(),
        toml::Value::String("fallback".to_string()),
    );
    config.as_table_mut().unwrap().insert(
        "update_check".to_string(),
        toml::Value::Table(toml::map::Map::from_iter([(
            "mode".to_string(),
            toml::Value::String("off".to_string()),
        )])),
    );
    for profile in config["profile"].as_array_mut().unwrap() {
        let timeout = match profile["name"].as_str().unwrap() {
            "parent" => 3,
            "fallback" => 2,
            "short" => 1,
            _ => unreachable!(),
        };
        let table = profile.as_table_mut().unwrap();
        table.insert("url".to_string(), toml::Value::String(url.to_string()));
        table.insert("timeout_in_sec".to_string(), toml::Value::Integer(timeout));
    }
    fs::write(config_path, toml::to_string_pretty(&config).unwrap()).unwrap();
    let metadata_path = project.join(".cargo-ai/project.toml");
    let mut metadata: toml::Value =
        toml::from_str(&fs::read_to_string(&metadata_path).unwrap()).unwrap();
    metadata.as_table_mut().unwrap().remove("runtime");
    if let Some(timeout) = project_timeout {
        metadata.as_table_mut().unwrap().insert(
            "runtime".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "defaults".to_string(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "inference_timeout_in_sec".to_string(),
                    toml::Value::Integer(timeout),
                )])),
            )])),
        );
    }
    fs::write(metadata_path, toml::to_string_pretty(&metadata).unwrap()).unwrap();
}

#[derive(Clone)]
struct Request {
    profile: String,
    received: Instant,
}

struct DelayedProvider {
    url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Request>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl DelayedProvider {
    fn new(delays_ms: Vec<u64>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("isolated loopback should bind");
        listener.set_nonblocking(true).unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_requests = requests.clone();
        let worker_stop = stop.clone();
        let delays_ms = Arc::new(delays_ms);
        let worker = thread::spawn(move || {
            let mut handlers = Vec::new();
            while !worker_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let handler_requests = worker_requests.clone();
                        let handler_stop = worker_stop.clone();
                        let delays_ms = delays_ms.clone();
                        handlers.push(thread::spawn(move || {
                            let Some(request) = read_request(&mut stream, &handler_stop) else {
                                return;
                            };
                            assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
                            let profile = ["parent", "fallback", "short"]
                                .into_iter()
                                .find(|name| request.contains(&format!("fixture-{name}-token")))
                                .unwrap_or("explicit-token")
                                .to_string();
                            let received = Instant::now();
                            let mut recorded = handler_requests.lock().unwrap();
                            let delay = *delays_ms.get(recorded.len()).expect("no unexpected provider call");
                            recorded.push(Request { profile, received });
                            drop(recorded);
                            while received.elapsed() < Duration::from_millis(delay) {
                                if handler_stop.load(Ordering::SeqCst) { return; }
                                thread::sleep(Duration::from_millis(5));
                            }
                            let body = support::openai_success_response("ready").to_string();
                            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                            // A timed-out client has deliberately stopped reading.
                            let _ = stream.write_all(response.as_bytes());
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                }
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        Self {
            url,
            stop,
            requests,
            worker: Some(worker),
        }
    }

    fn finish(mut self, output: &Output) -> Vec<Request> {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap_or_else(|error| {
            panic!(
                "loopback server failed: {error:?}; command output: {}",
                output_text(output)
            )
        });
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for DelayedProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_request(stream: &mut TcpStream, stop: &AtomicBool) -> Option<String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let started = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) || started.elapsed() >= Duration::from_secs(5) {
            assert!(
                bytes.is_empty(),
                "incomplete fixture request: {} bytes",
                bytes.len()
            );
            eprintln!("timeout-fixture: closed idle connection without an HTTP request");
            return None;
        }
        let count = match stream.read(&mut buffer) {
            Ok(0) => {
                assert!(
                    bytes.is_empty(),
                    "peer closed an incomplete fixture request: {} bytes",
                    bytes.len()
                );
                return None;
            }
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(error) => panic!(
                "fixture request read failed after {} bytes: {error}",
                bytes.len()
            ),
        };
        bytes.extend_from_slice(&buffer[..count]);
        assert!(
            bytes.len() < 1024 * 1024,
            "fixture request should be bounded"
        );
        if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                break;
            }
        }
    }
    Some(String::from_utf8(bytes).unwrap())
}

fn run_command(
    fixture: &Fixture,
    project: &Path,
    executable: Option<&Path>,
    runtime_budget: &str,
) -> Command {
    let mut cmd = command(
        fixture,
        executable.unwrap_or(Path::new(env!("CARGO_BIN_EXE_cargo-ai"))),
        project,
    );
    if executable.is_none() {
        cmd.args(["--no-update-check", "run", "--config", "agent.json"]);
    }
    cmd.args([
        "--profile",
        "parent",
        "--render-mode",
        "append-only",
        "--max-runtime-in-sec",
        runtime_budget,
    ]);
    cmd
}

fn observe(
    label: &str,
    output: &Output,
    finished: Instant,
    requests: Vec<Request>,
    expected_profiles: &[&str],
    success: bool,
    minimum_ms: u64,
    maximum_ms: u64,
    diagnostic: Option<&str>,
) {
    let text = output_text(&output);
    assert_eq!(output.status.success(), success, "{label}: {text}");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.profile.as_str())
            .collect::<Vec<_>>(),
        expected_profiles,
        "{label}: provider selection: {text}"
    );
    let elapsed = finished
        .duration_since(
            requests
                .last()
                .expect("fixture should reach provider")
                .received,
        )
        .as_millis();
    assert!(
        (u128::from(minimum_ms)..=u128::from(maximum_ms)).contains(&elapsed),
        "{label}: response wait {elapsed}ms outside {minimum_ms}..={maximum_ms}ms: {text}"
    );
    if let Some(diagnostic) = diagnostic {
        assert!(text.contains(diagnostic), "{label}: {text}");
    }
    let handoff = requests
        .last()
        .unwrap()
        .received
        .duration_since(requests[0].received)
        .as_millis();
    eprintln!("timeout-observation {label}: success={success}, last_profile={}, provider_wait_ms={elapsed}, handoff_ms={handoff}", expected_profiles.last().unwrap());
}

fn observe_shared_runtime_budget(
    label: &str,
    output: &Output,
    finished: Instant,
    requests: Vec<Request>,
    child_profile: &str,
    budget_secs: u64,
) {
    let text = output_text(output);
    assert!(!output.status.success(), "{label}: {text}");
    assert!(
        text.contains(&format!("max-runtime-in-sec {budget_secs}")),
        "{label}: {text}"
    );

    // The shared deadline includes child startup. Separate reachability controls
    // require the child request; cancellation may happen before it is sent.
    assert!((1..=2).contains(&requests.len()), "{label}: {text}");
    let expected = ["parent", child_profile];
    let profiles = requests
        .iter()
        .map(|request| request.profile.as_str())
        .collect::<Vec<_>>();
    assert_eq!(profiles, &expected[..requests.len()], "{label}: {text}");

    // Measure the whole remaining invocation, including the child handoff,
    // rather than restarting the measured wait at the final provider request.
    let elapsed_ms = finished.duration_since(requests[0].received).as_millis();
    assert!(
        elapsed_ms <= u128::from(budget_secs * 1000 + 1800),
        "{label}: shared budget elapsed {elapsed_ms}ms: {text}"
    );
    eprintln!(
        "timeout-observation {label}: success=false, profiles={profiles:?}, shared_wait_ms={elapsed_ms}"
    );
}

#[test]
fn json_child_reachability_and_shared_budget_are_separate() {
    let fixture = Fixture::new("timeout-json-budget");
    let project = fixture.root.join("project");
    assert_success(
        &cli(
            &fixture,
            &fixture.root,
            &["new", project.to_str().unwrap(), "--vcs", "none"],
        ),
        "scaffold focused timeout project",
    );
    let mut source = definition();
    source["actions"]
        .as_array_mut()
        .unwrap()
        .retain(|action| action["name"].as_str() == Some("json"));
    for name in ["agent.json", "child.json"] {
        fs::write(
            project.join(name),
            serde_json::to_vec_pretty(&source).unwrap(),
        )
        .unwrap();
    }
    create_profiles(&fixture, &project);

    let server = DelayedProvider::new(vec![0, 0]);
    configure(&fixture, &project, &server.url, Some(6));
    let output = run_command(&fixture, &project, None, "15")
        .args(["--run-var", "mode=json"])
        .output()
        .unwrap();
    observe(
        "focused/json/reachability",
        &output,
        Instant::now(),
        server.finish(&output),
        &["parent", "parent"],
        true,
        0,
        2400,
        None,
    );

    for budget_secs in [1, 4] {
        let server = DelayedProvider::new(vec![0, 6400]);
        configure(&fixture, &project, &server.url, Some(6));
        let output = run_command(&fixture, &project, None, &budget_secs.to_string())
            .args(["--run-var", "mode=json"])
            .output()
            .unwrap();
        observe_shared_runtime_budget(
            &format!("focused/json/shared_runtime_budget/{budget_secs}"),
            &output,
            Instant::now(),
            server.finish(&output),
            "parent",
            budget_secs,
        );
    }
}

#[test]
fn timeout_precedence_and_child_paths_remain_explicit_at_process_boundaries() {
    let fixture = Fixture::new("timeout-process");
    let project = fixture.root.join("project");
    assert_success(
        &cli(
            &fixture,
            &fixture.root,
            &["new", project.to_str().unwrap(), "--vcs", "none"],
        ),
        "scaffold timeout project",
    );
    let source = serde_json::to_vec_pretty(&definition()).unwrap();
    fs::write(project.join("agent.json"), &source).unwrap();
    fs::write(project.join("child.json"), &source).unwrap();
    create_profiles(&fixture, &project);
    configure(
        &fixture,
        &project,
        "http://127.0.0.1:9/v1/chat/completions",
        None,
    );
    assert_success(
        &cli(&fixture, &project, &["add", "tool", "timeout_bridge"]),
        "scaffold bridge tool",
    );
    fs::write(project.join("tools/timeout_bridge/src/tool.rs"), TOOL).unwrap();
    assert_success(
        &cli(&fixture, &project, &["tools", "build", "timeout_bridge"]),
        "build bridge tool",
    );
    assert_success(
        &cli(
            &fixture,
            &project,
            &["hatch", "timeout_parent", "--config", "agent.json"],
        ),
        "hatch timeout fixture",
    );
    let executable = project.join(if cfg!(windows) {
        "timeout_parent.exe"
    } else {
        "timeout_parent"
    });
    let child_executable = project.join(if cfg!(windows) { "child.exe" } else { "child" });
    fs::copy(&executable, &child_executable).unwrap();
    eprintln!("Timeout fixture built; measuring interpreted and generated invocation paths.");

    for runtime in [None, Some(executable.as_path())] {
        let runtime_name = if runtime.is_some() {
            "generated"
        } else {
            "interpreted"
        };
        for (name, project_timeout, cli_timeout, delay, cutoff) in [
            ("cli_precedence", Some(4), Some("1"), 1400, 1000),
            ("project_precedence", Some(4), None, 4400, 4000),
            ("profile_precedence", None, None, 3400, 3000),
        ] {
            let server = DelayedProvider::new(vec![delay]);
            configure(&fixture, &project, &server.url, project_timeout);
            let mut cmd = run_command(&fixture, &project, runtime, "15");
            if let Some(timeout) = cli_timeout {
                cmd.args(["--inference-timeout-in-sec", timeout]);
            }
            let output = cmd.output().unwrap();
            let finished = Instant::now();
            observe(
                &format!("{runtime_name}/{name}"),
                &output,
                finished,
                server.finish(&output),
                &["parent"],
                false,
                cutoff - 250,
                cutoff + 1200,
                None,
            );
        }

        for builtin in [false, true] {
            let delay = if builtin { 3400 } else { 2400 };
            let server = DelayedProvider::new(vec![delay]);
            configure(&fixture, &project, &server.url, None);
            let mut cmd = command(
                &fixture,
                runtime.unwrap_or(Path::new(env!("CARGO_BIN_EXE_cargo-ai"))),
                &project,
            );
            if runtime.is_none() {
                cmd.args(["--no-update-check", "run", "--config", "agent.json"]);
            }
            cmd.args(["--render-mode", "append-only", "--max-runtime-in-sec", "15"]);
            if builtin {
                let config_path = fixture.cargo_ai_home.join("config.toml");
                let mut config: toml::Value =
                    toml::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
                config.as_table_mut().unwrap().remove("default_profile");
                fs::write(config_path, toml::to_string_pretty(&config).unwrap()).unwrap();
                cmd.args([
                    "--server",
                    "openai",
                    "--model",
                    "timeout-fixture",
                    "--url",
                    &server.url,
                    "--token",
                    "fixture-direct-token",
                ]);
            }
            let output = cmd.output().unwrap();
            let finished = Instant::now();
            observe(
                &format!(
                    "{runtime_name}/{}",
                    if builtin {
                        "builtin_lower_bound"
                    } else {
                        "default_profile"
                    }
                ),
                &output,
                finished,
                server.finish(&output),
                &[if builtin {
                    "explicit-token"
                } else {
                    "fallback"
                }],
                builtin,
                if builtin { 3200 } else { 1750 },
                if builtin { 5000 } else { 3200 },
                None,
            );
        }

        let server = DelayedProvider::new(vec![0, 6400]);
        configure(&fixture, &project, &server.url, Some(6));
        let output = run_command(&fixture, &project, runtime, "1")
            .args(["--run-var", "mode=json"])
            .output()
            .unwrap();
        let finished = Instant::now();
        let requests = server.finish(&output);
        if requests.is_empty() {
            assert!(!output.status.success());
            assert!(output_text(&output).contains("max-runtime-in-sec"));
            eprintln!("timeout-observation {runtime_name}/early_tree_budget: rejected before provider request");
        } else {
            let profiles = vec!["parent"; requests.len()];
            assert!(requests.len() <= 2);
            observe(
                &format!("{runtime_name}/early_tree_budget"),
                &output,
                finished,
                requests,
                &profiles,
                false,
                0,
                2400,
                Some("max-runtime-in-sec"),
            );
        }

        for mode in ["json", "executable", "bridge_json", "bridge_executable"] {
            let inherited = mode.ends_with("json");
            for (name, project_timeout, explicit, delay, success, cutoff) in [
                (
                    "parent_cli_is_local",
                    None,
                    false,
                    2400,
                    inherited,
                    if inherited { 2400 } else { 2000 },
                ),
                ("project_default", Some(4), false, 3400, true, 3400),
                ("explicit_child_profile", None, true, 1400, false, 1000),
            ] {
                let server = DelayedProvider::new(vec![0, delay]);
                configure(&fixture, &project, &server.url, project_timeout);
                let mut cmd = run_command(&fixture, &project, runtime, "15");
                let selected_mode = if explicit && !mode.starts_with("bridge") {
                    format!("{mode}_explicit")
                } else {
                    mode.to_string()
                };
                cmd.args([
                    "--inference-timeout-in-sec",
                    if explicit { "4" } else { "1" },
                    "--run-var",
                    &format!("mode={selected_mode}"),
                ]);
                if explicit {
                    cmd.args(["--run-var", "child_profile=short"]);
                }
                let output = cmd.output().unwrap();
                let finished = Instant::now();
                let child_profile = if explicit {
                    "short"
                } else if inherited {
                    "parent"
                } else {
                    "fallback"
                };
                observe(
                    &format!("{runtime_name}/{mode}/{name}"),
                    &output,
                    finished,
                    server.finish(&output),
                    &["parent", child_profile],
                    success,
                    cutoff - 250,
                    cutoff + 1600,
                    None,
                );
            }

            let server = DelayedProvider::new(vec![0, 6400]);
            configure(&fixture, &project, &server.url, Some(6));
            let mut budget_cmd = run_command(&fixture, &project, runtime, "3");
            budget_cmd.args(["--run-var", &format!("mode={mode}")]);
            let output = budget_cmd.output().unwrap();
            let finished = Instant::now();
            observe_shared_runtime_budget(
                &format!("{runtime_name}/{mode}/shared_runtime_budget"),
                &output,
                finished,
                server.finish(&output),
                if inherited { "parent" } else { "fallback" },
                3,
            );

            let server = DelayedProvider::new(vec![0, 0]);
            configure(&fixture, &project, &server.url, None);
            let mut cmd = run_command(&fixture, &project, runtime, "15");
            cmd.args([
                "--max-agent-depth",
                "1",
                "--run-var",
                &format!("mode={mode}"),
                "--run-var",
                "child_mode=json",
            ]);
            let output = cmd.output().unwrap();
            let finished = Instant::now();
            observe(
                &format!("{runtime_name}/{mode}/shared_depth"),
                &output,
                finished,
                server.finish(&output),
                &["parent", if inherited { "parent" } else { "fallback" }],
                false,
                0,
                2400,
                Some("max-agent-depth"),
            );
        }
    }

    let tool_manifest: Value = serde_json::from_slice(
        &fs::read(project.join(".cargo-ai/tools/timeout_bridge/tool.json")).unwrap(),
    )
    .unwrap();
    let artifacts = tool_manifest["artifacts"].as_object().unwrap();
    assert_eq!(artifacts.len(), 1, "fixture builds only the native target");
    let tool_relative = artifacts.values().next().unwrap()["path"]
        .as_str()
        .expect("built tool should expose executable path");
    let tool_binary = project
        .join(".cargo-ai/tools/timeout_bridge")
        .join(tool_relative);
    for artifact in [
        "./child.json",
        if cfg!(windows) {
            "./child.exe"
        } else {
            "./child"
        },
    ] {
        for old_context in [true, false] {
            let server = DelayedProvider::new(vec![1400]);
            configure(&fixture, &project, &server.url, None);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let mut bridge = json!({"current_depth":0,"max_depth":2,"runtime_budget":{"max_runtime_secs":15,"started_at_ms":now,"deadline_ms":now+15000},"profile_name":"parent","action_execution":"sequential"});
            if !old_context {
                bridge["artifact_root"] = json!(project);
            }
            let request = json!({"protocol_version":1,"params":{"artifact":artifact,"profile":"","child_mode":"none"},"runtime_context":{"agent_bridge":bridge}});
            let mut process = command(&fixture, &tool_binary, &project)
                .arg("invoke")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            process
                .stdin
                .take()
                .unwrap()
                .write_all(&serde_json::to_vec(&request).unwrap())
                .unwrap();
            let output = process.wait_with_output().unwrap();
            let finished = Instant::now();
            observe(
                &format!(
                    "tool_helper/{artifact}/{}",
                    if old_context {
                        "legacy_context"
                    } else {
                        "current_context"
                    }
                ),
                &output,
                finished,
                server.finish(&output),
                &[if artifact.ends_with("json") {
                    "parent"
                } else {
                    "fallback"
                }],
                true,
                1200,
                3000,
                None,
            );
        }
    }
    assert_eq!(fs::read(project.join("agent.json")).unwrap(), source);
    assert_eq!(fs::read(project.join("child.json")).unwrap(), source);
}
