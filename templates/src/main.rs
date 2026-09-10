mod runtime_data;
mod args;
mod web_resources;
mod config;
mod credentials;
#[path = "../definition_validation.rs"]
mod definition_validation;
mod providers;
mod usage_log;

use jsonlogic::apply;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{self, IsTerminal, Write};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};

use config::loader::{config_path, find_profile, load_config};
use config::schema::{Profile, ProfileAuthMode, SecretStoreMode};
use providers::{
    provider_error_messages, validate_provider_content_parts, validate_provider_request,
    AuthenticationPolicy, ProviderError, ProviderKind,
};

include!(concat!(env!("OUT_DIR"), "/agent_model.rs"));

const INFRA_BASE_URL: &str = "https://api.cargo-ai.org";
const OPENAI_ACCOUNT_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const OPENAI_REFRESH_BUFFER_SEC: i64 = 30;
const KEYCHAIN_SERVICE: &str = "cargo-ai";
const ACCOUNT_ACCESS_TOKEN_STORAGE_KEY: &str = "account/access_token";
const ACCOUNT_REFRESH_TOKEN_STORAGE_KEY: &str = "account/refresh_token";
const AGENT_ACTION_DEPTH_ENV: &str = "CARGO_AI_AGENT_ACTION_DEPTH";
const AGENT_ACTION_MAX_DEPTH_ENV: &str = "CARGO_AI_AGENT_ACTION_MAX_DEPTH";
const AGENT_ACTION_MAX_RUNTIME_SECS_ENV: &str = "CARGO_AI_AGENT_MAX_RUNTIME_SECS";
const AGENT_ACTION_RUNTIME_STARTED_AT_MS_ENV: &str = "CARGO_AI_AGENT_RUNTIME_STARTED_AT_MS";
const AGENT_ACTION_RUNTIME_DEADLINE_MS_ENV: &str = "CARGO_AI_AGENT_RUNTIME_DEADLINE_MS";
const DEFAULT_AGENT_ACTION_MAX_DEPTH: u32 = 5;
const DEFAULT_INFERENCE_TIMEOUT_IN_SEC: u64 = 60;
const DEFAULT_AGENT_ACTION_MAX_RUNTIME_SECS: u64 = 600;
const PROJECT_METADATA_RELATIVE_PATH: &str = ".cargo-ai/project.toml";
const PROJECT_TOOLS_RELATIVE_PATH: &str = ".cargo-ai/tools";
const TOOL_MANIFEST_FILE_NAME: &str = "tool.json";
const TOOL_PROTOCOL_VERSION: u32 = 1;
const SUPPORTED_FILE_EXTENSIONS_MESSAGE: &str =
    "pdf, docx, csv, xla, xlb, xlc, xlm, xls, xlsx, xlt, xlw, tsv, iif, doc, dot, odt, rtf, pot, ppa, pps, ppt, pptx, pwz, wiz";
const ACTION_LANE_OUTPUT_BUFFER_LIMIT: usize = 6;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

tokio::task_local! {
    static ACTION_OUTPUT: ActionOutput;
}

#[derive(Clone)]
struct ActionOutput {
    inner: Arc<Mutex<ActionOutputState>>,
    live_refresh_stop: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionOutputMode {
    AppendOnly,
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestedActionRenderMode {
    Auto,
    Live,
    AppendOnly,
}

struct ActionOutputState {
    mode: ActionOutputMode,
    startup_notice: Option<&'static str>,
    action_execution: ActionExecutionMode,
    run_started_at: Instant,
    run_finished_after: Option<Duration>,
    header_rendered: bool,
    rendered_lines: usize,
    lanes: BTreeMap<usize, ActionLaneState>,
    last_using_line: Option<String>,
}

#[derive(Clone)]
struct ActionLaneState {
    action_name: String,
    status: ActionLaneStatus,
    lane_started_at: Option<Instant>,
    lane_finished_after: Option<Duration>,
    current_step: Option<String>,
    step_started_at: Option<Instant>,
    last_message: Option<String>,
    output_lines: VecDeque<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionLaneStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Aborted,
    Notice,
    LogicError,
    Skipped,
}

enum ChildArtifactInvocation {
    DirectExecutable(PathBuf),
    CargoSubcommand,
    StandaloneCargoAi,
}

impl ActionOutput {
    fn new(
        action_execution: ActionExecutionMode,
        requested_mode: RequestedActionRenderMode,
        run_started_at: Instant,
    ) -> Self {
        let (mode, startup_notice) =
            resolve_action_render_mode_for_capability(requested_mode, live_dashboard_supported());
        Self::new_for_mode_with_notice(action_execution, mode, startup_notice, run_started_at)
    }

    #[cfg(test)]
    fn new_for_mode(action_execution: ActionExecutionMode, mode: ActionOutputMode) -> Self {
        Self::new_for_mode_with_notice(action_execution, mode, None, Instant::now())
    }

    fn new_for_mode_with_notice(
        action_execution: ActionExecutionMode,
        mode: ActionOutputMode,
        startup_notice: Option<&'static str>,
        run_started_at: Instant,
    ) -> Self {
        let inner = Arc::new(Mutex::new(ActionOutputState {
            mode,
            startup_notice,
            action_execution,
            run_started_at,
            run_finished_after: None,
            header_rendered: false,
            rendered_lines: 0,
            lanes: BTreeMap::new(),
            last_using_line: None,
        }));
        let live_refresh_stop = Arc::new(AtomicBool::new(false));
        maybe_spawn_live_action_refresh(inner.clone(), live_refresh_stop.clone(), mode);
        Self {
            inner,
            live_refresh_stop,
        }
    }

    fn print_execution_header(&self) {
        self.with_state(|state| {
            if let Some(notice) = state.startup_notice.take() {
                println!("{notice}");
            }
            if state.mode == ActionOutputMode::AppendOnly {
                if state.header_rendered {
                    return;
                }
                println!("{}", action_execution_header(state.action_execution));
            } else {
                render_live_dashboard(state);
            }
            state.header_rendered = true;
        });
    }

    fn seed_using_line(&self, using_line: &str) {
        self.with_state(|state| {
            state.last_using_line = Some(using_line.to_string());
        });
    }

    fn action_started(&self, action_index: usize, action_name: &str) {
        self.with_state(|state| {
            let lane = ensure_lane_state(state, action_index, action_name);
            lane.status = ActionLaneStatus::Running;
            lane.lane_started_at = Some(Instant::now());
            lane.lane_finished_after = None;
            lane.last_message = Some("started".to_string());
            if state.mode == ActionOutputMode::AppendOnly {
                println!("{}", format_action_line(action_index, action_name, "started"));
            } else {
                render_live_dashboard(state);
            }
        });
    }

    fn action_step_started(
        &self,
        action_index: usize,
        action_name: &str,
        step_kind: &str,
        step_number: usize,
        step_count: usize,
    ) {
        self.with_state(|state| {
            let lane = ensure_lane_state(state, action_index, action_name);
            lane.status = ActionLaneStatus::Running;
            lane.current_step = Some(format!("{}/{} {}", step_number, step_count, step_kind));
            lane.step_started_at = Some(Instant::now());
            lane.last_message = Some(waiting_message_for_step_kind(step_kind).to_string());
            if state.mode == ActionOutputMode::AppendOnly {
                println!(
                    "{}",
                    format_action_line(
                        action_index,
                        action_name,
                        format!(
                            "step {}/{} {} started; {}",
                            step_number,
                            step_count,
                            step_kind,
                            waiting_message_for_step_kind(step_kind)
                        )
                        .as_str(),
                    )
                );
            } else {
                render_live_dashboard(state);
            }
        });
    }

    fn action_line(&self, action_index: usize, action_name: &str, message: &str) {
        self.with_state(|state| {
            emit_action_line_locked(state, action_index, action_name, message);
        });
    }

    fn action_using_line_if_changed(
        &self,
        action_index: usize,
        action_name: &str,
        using_line: &str,
    ) {
        self.with_state(|state| {
            if state.last_using_line.as_deref() == Some(using_line) {
                return;
            }
            state.last_using_line = Some(using_line.to_string());
            if state.mode == ActionOutputMode::Live {
                return;
            }
            emit_action_line_locked(state, action_index, action_name, using_line);
        });
    }

    fn action_success(&self, action_index: usize, action_name: &str, summary: &str) {
        self.with_state(|state| {
            let append_only = state.mode == ActionOutputMode::AppendOnly;
            let append_message = {
                let lane = ensure_lane_state(state, action_index, action_name);
                lane.status = ActionLaneStatus::Completed;
                lane.current_step = None;
                lane.step_started_at = None;
                lane.lane_finished_after = lane
                    .lane_started_at
                    .map(|started_at| started_at.elapsed());
                lane.last_message = Some(summary.to_string());
                if append_only {
                    Some(format!(
                        "{} · {}",
                        summary,
                        format_elapsed_duration(
                            lane.lane_finished_after.unwrap_or_else(|| Duration::from_secs(0))
                        )
                    ))
                } else {
                    None
                }
            };
            if let Some(message) = append_message {
                println!(
                    "{}",
                    format_action_line(action_index, action_name, message.as_str())
                );
            } else {
                render_live_dashboard(state);
            }
        });
    }

    fn action_failed(&self, action_index: usize, action_name: &str, error: &str) {
        self.with_state(|state| {
            let append_only = state.mode == ActionOutputMode::AppendOnly;
            let append_message = {
                let lane = ensure_lane_state(state, action_index, action_name);
                lane.status = ActionLaneStatus::Failed;
                lane.current_step = None;
                lane.step_started_at = None;
                lane.lane_finished_after = lane
                    .lane_started_at
                    .map(|started_at| started_at.elapsed());
                lane.last_message = compact_action_output_line(error)
                    .map(|line| format!("failed: {}", line))
                    .or_else(|| Some("failed".to_string()));
                push_lane_output_message(lane, error);
                if append_only {
                    Some(format!(
                        "failed · {}",
                        format_elapsed_duration(
                            lane.lane_finished_after.unwrap_or_else(|| Duration::from_secs(0))
                        )
                    ))
                } else {
                    None
                }
            };
            if let Some(message) = append_message {
                println!(
                    "{}",
                    format_action_line(action_index, action_name, message.as_str())
                );
            } else {
                render_live_dashboard(state);
            }
        });
    }

    fn action_aborted(&self, action_index: usize, action_name: &str, error: &str) {
        self.with_state(|state| {
            let append_only = state.mode == ActionOutputMode::AppendOnly;
            let append_message = {
                let lane = ensure_lane_state(state, action_index, action_name);
                lane.status = ActionLaneStatus::Aborted;
                lane.current_step = None;
                lane.step_started_at = None;
                lane.lane_finished_after = lane
                    .lane_started_at
                    .map(|started_at| started_at.elapsed());
                lane.last_message = compact_action_output_line(error)
                    .map(|line| format!("abort requested: {}", line))
                    .or_else(|| Some("abort requested".to_string()));
                push_lane_output_message(lane, error);
                if append_only {
                    Some(format!(
                        "abort requested · {}: {}",
                        format_elapsed_duration(
                            lane.lane_finished_after.unwrap_or_else(|| Duration::from_secs(0))
                        ),
                        error
                    ))
                } else {
                    None
                }
            };
            if let Some(message) = append_message {
                println!(
                    "{}",
                    format_action_line(action_index, action_name, message.as_str())
                );
            } else {
                render_live_dashboard(state);
            }
        });
    }

    fn action_stopped_by_abort(&self, action_index: usize, action_name: &str) {
        self.with_state(|state| {
            if state.mode == ActionOutputMode::AppendOnly {
                return;
            }

            let lane = ensure_lane_state(state, action_index, action_name);
            lane.status = ActionLaneStatus::Aborted;
            lane.current_step = None;
            lane.step_started_at = None;
            lane.lane_finished_after = lane
                .lane_started_at
                .map(|started_at| started_at.elapsed());
            lane.last_message = Some("stopped after invocation abort.".to_string());
            render_live_dashboard(state);
        });
    }

    fn finish(&self) {
        self.live_refresh_stop.store(true, Ordering::Relaxed);
        self.with_state(|state| {
            if state.mode == ActionOutputMode::Live {
                state.run_finished_after = Some(state.run_started_at.elapsed());
                render_live_dashboard(state);
                let _ = writeln!(io::stdout());
                let _ = io::stdout().flush();
                state.rendered_lines = 0;
            }
        });
    }

    fn with_state(&self, update: impl FnOnce(&mut ActionOutputState)) {
        let mut state = self
            .inner
            .lock()
            .expect("action output lock should succeed");
        update(&mut state);
    }
}

impl ActionOutputState {
    fn snapshot_lines(&self) -> Vec<String> {
        let mut lines = vec![run_header_line(
            self.action_execution,
            match self.mode {
                ActionOutputMode::AppendOnly => None,
                ActionOutputMode::Live => self.run_elapsed_duration(),
            },
        )];

        for (lane_index, lane) in &self.lanes {
            lines.push(String::new());
            lines.push(format!(
                "{} {}",
                action_lane_prefix(*lane_index, lane.action_name.as_str()),
                lane_status_label(lane)
            ));
            lines.push(format!("  step: {}", lane_step_label(lane)));
            if let Some(last_message) = lane_last_message(lane) {
                lines.push(format!("  last: {last_message}"));
            }
        }

        lines
    }

    fn run_elapsed_duration(&self) -> Option<Duration> {
        self.run_finished_after
            .or_else(|| Some(self.run_started_at.elapsed()))
    }
}

impl ActionLaneStatus {
    fn display_name(self) -> &'static str {
        match self {
            ActionLaneStatus::Pending => "pending",
            ActionLaneStatus::Running => "running",
            ActionLaneStatus::Completed => "completed",
            ActionLaneStatus::Failed => "failed",
            ActionLaneStatus::Aborted => "aborted",
            ActionLaneStatus::Notice => "notice",
            ActionLaneStatus::LogicError => "logic error",
            ActionLaneStatus::Skipped => "skipped",
        }
    }
}

fn live_dashboard_supported() -> bool {
    io::stdout().is_terminal()
        && std::env::var("TERM").map(|term| term != "dumb").unwrap_or(true)
        && std::env::var_os("CI").is_none()
}

fn resolve_action_render_mode_for_capability(
    requested_mode: RequestedActionRenderMode,
    live_supported: bool,
) -> (ActionOutputMode, Option<&'static str>) {
    match requested_mode {
        RequestedActionRenderMode::Auto => {
            if live_supported {
                (ActionOutputMode::Live, None)
            } else {
                (ActionOutputMode::AppendOnly, None)
            }
        }
        RequestedActionRenderMode::Live => {
            if live_supported {
                (ActionOutputMode::Live, None)
            } else {
                (
                    ActionOutputMode::AppendOnly,
                    Some(
                        "! Requested --render-mode live, but live output is unavailable here; using append-only output.",
                    ),
                )
            }
        }
        RequestedActionRenderMode::AppendOnly => (ActionOutputMode::AppendOnly, None),
    }
}

fn ensure_lane_state<'a>(
    state: &'a mut ActionOutputState,
    action_index: usize,
    action_name: &str,
) -> &'a mut ActionLaneState {
    state
        .lanes
        .entry(action_index)
        .or_insert_with(|| ActionLaneState {
            action_name: action_name.to_string(),
            status: ActionLaneStatus::Pending,
            lane_started_at: None,
            lane_finished_after: None,
            current_step: None,
            step_started_at: None,
            last_message: None,
            output_lines: VecDeque::new(),
        })
}

fn emit_action_line_locked(
    state: &mut ActionOutputState,
    action_index: usize,
    action_name: &str,
    message: &str,
) {
    if state.mode == ActionOutputMode::AppendOnly {
        for line in split_action_output_lines(message) {
            println!("{}", format_action_line(action_index, action_name, line.as_str()));
        }
        return;
    }

    if !should_surface_live_dashboard_message(message) {
        return;
    }

    let lane = ensure_lane_state(state, action_index, action_name);
    lane.last_message = compact_action_output_line(message);
    push_lane_output_message(lane, message);
    if lane.status == ActionLaneStatus::Pending {
        lane.status = inferred_lane_status(message);
    } else if lane.status == ActionLaneStatus::Running {
        lane.status = match inferred_lane_status(message) {
            ActionLaneStatus::Notice => ActionLaneStatus::Running,
            other => other,
        };
    }
    render_live_dashboard(state);
}

fn maybe_spawn_live_action_refresh(
    inner: Arc<Mutex<ActionOutputState>>,
    stop: Arc<AtomicBool>,
    mode: ActionOutputMode,
) {
    if mode != ActionOutputMode::Live || cfg!(test) {
        return;
    }

    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };

    handle.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;

        loop {
            interval.tick().await;
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let mut state = inner.lock().expect("action output lock should succeed");
            if state.mode != ActionOutputMode::Live {
                break;
            }
            render_live_dashboard(&mut state);
        }
    });
}

fn inferred_lane_status(message: &str) -> ActionLaneStatus {
    if message.starts_with("logic evaluation failed:") {
        ActionLaneStatus::LogicError
    } else if message.contains("no run steps matched")
        || message.contains("unsupported step kind")
    {
        ActionLaneStatus::Skipped
    } else {
        ActionLaneStatus::Notice
    }
}

fn split_action_output_lines(message: &str) -> Vec<String> {
    message
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn compact_action_output_line(message: &str) -> Option<String> {
    split_action_output_lines(message).into_iter().next()
}

fn should_surface_live_dashboard_message(message: &str) -> bool {
    compact_action_output_line(message)
        .map(|line| {
            !line.starts_with("using: ") && !line.contains("resolved dynamic child-agent ")
        })
        .unwrap_or(false)
}

fn push_lane_output_message(lane: &mut ActionLaneState, message: &str) {
    for line in split_action_output_lines(message) {
        if lane.output_lines.len() == ACTION_LANE_OUTPUT_BUFFER_LIMIT {
            lane.output_lines.pop_front();
        }
        lane.output_lines.push_back(line);
    }
}

fn lane_step_label(lane: &ActionLaneState) -> String {
    match lane.status {
        ActionLaneStatus::Completed => "✓ done".to_string(),
        ActionLaneStatus::Failed | ActionLaneStatus::LogicError => "x failed".to_string(),
        ActionLaneStatus::Aborted => "! aborted".to_string(),
        ActionLaneStatus::Skipped => "skipped".to_string(),
        _ => lane
            .current_step
            .clone()
            .unwrap_or_else(|| "-".to_string()),
    }
}

fn lane_last_message(lane: &ActionLaneState) -> Option<&str> {
    let message = lane.last_message.as_deref()?;

    if lane.status == ActionLaneStatus::Completed
        && matches!(message, "completed" | "completed.")
    {
        return None;
    }

    Some(message)
}

fn lane_status_label(lane: &ActionLaneState) -> String {
    if let Some(elapsed) = lane_elapsed_duration(lane) {
        match lane.status {
            ActionLaneStatus::Running => {
                return format!("running · {}", format_elapsed_duration(elapsed));
            }
            ActionLaneStatus::Completed => {
                return format!("completed · {}", format_elapsed_duration(elapsed));
            }
            ActionLaneStatus::Failed | ActionLaneStatus::LogicError => {
                return format!("failed · {}", format_elapsed_duration(elapsed));
            }
            ActionLaneStatus::Aborted => {
                return format!("aborted · {}", format_elapsed_duration(elapsed));
            }
            _ => {}
        }
    }

    lane.status.display_name().to_string()
}

fn lane_elapsed_duration(lane: &ActionLaneState) -> Option<Duration> {
    lane.lane_finished_after
        .or_else(|| lane.lane_started_at.map(|started_at| started_at.elapsed()))
}

fn run_header_line(action_execution: ActionExecutionMode, elapsed: Option<Duration>) -> String {
    match elapsed {
        Some(elapsed) => format!(
            "{} · {}",
            action_execution_header(action_execution),
            format_elapsed_duration(elapsed)
        ),
        None => action_execution_header(action_execution).to_string(),
    }
}

fn format_elapsed_duration(duration: Duration) -> String {
    let elapsed_ms = duration.as_millis();

    if elapsed_ms < 1_000 {
        return format!("{elapsed_ms}ms");
    }

    if elapsed_ms < 10_000 {
        if elapsed_ms % 1_000 == 0 {
            return format!("{}s", duration.as_secs());
        }
        return format!("{:.1}s", duration.as_secs_f64());
    }

    let total_secs = duration.as_secs();
    if total_secs < 60 {
        return format!("{total_secs}s");
    }

    let hours = total_secs / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        if seconds == 0 {
            if minutes == 0 {
                return format!("{hours}h");
            }
            return format!("{hours}h {minutes}m");
        }
        return format!("{hours}h {minutes}m {seconds}s");
    }

    if seconds == 0 {
        format!("{minutes}m")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

fn waiting_message_for_step_kind(step_kind: &str) -> &'static str {
    if step_kind.eq_ignore_ascii_case("exec") {
        "waiting for command to finish..."
    } else if step_kind.eq_ignore_ascii_case("agent") {
        "waiting for child agent to finish..."
    } else if step_kind.eq_ignore_ascii_case("generate_image") {
        "waiting for provider response..."
    } else if step_kind.eq_ignore_ascii_case("email_me") {
        "waiting for mail response..."
    } else {
        "waiting for step to finish..."
    }
}

fn render_live_dashboard(state: &mut ActionOutputState) {
    if cfg!(test) {
        state.rendered_lines = state.snapshot_lines().len();
        return;
    }

    let snapshot = state.snapshot_lines();
    let mut stdout = io::stdout();

    if state.rendered_lines > 0 {
        let _ = write!(stdout, "\r");
        if state.rendered_lines > 1 {
            let _ = write!(stdout, "\x1b[{}A", state.rendered_lines - 1);
        }
        let _ = write!(stdout, "\x1b[J");
    }

    let _ = write!(stdout, "{}", snapshot.join("\n"));
    let _ = stdout.flush();
    state.rendered_lines = snapshot.len();
}

fn unknown_server_messages(server: &str) -> Vec<String> {
    let display_server = if server.trim().is_empty() {
        "(not set)"
    } else {
        server
    };

    vec![
        format!("❌ Unknown AI server '{}'.", display_server),
        "Use `--server anthropic`, `--server gemini`, `--server mistral`, `--server ollama`, `--server openai`, or `--server xai`.".to_string(),
        "Hint: Set `--server` explicitly or configure a default profile with a supported server."
            .to_string(),
        "Example: cargo ai run --server xai --model <grok-model> --input-text \"What is 2 + 2?\""
            .to_string(),
    ]
}

#[derive(Debug, Clone)]
struct SelectedProfile {
    name: String,
    auth_mode: ProfileAuthMode,
    legacy_token: Option<String>,
}

#[derive(Debug, Clone)]
struct ResolvedOpenAiToken {
    token: String,
    uses_account_session: bool,
}

#[derive(Debug, Clone, Copy)]
struct InvocationRuntimeBudget {
    max_runtime_secs: u64,
    started_at_ms: u64,
    deadline_ms: u64,
}

#[derive(Debug, Clone)]
struct ActionProviderContext {
    project_data: Option<runtime_data::DataRoot>,
    provider: ProviderKind,
    profile_name: Option<String>,
    auth_mode: String,
    model: String,
    url: String,
    token: String,
    inference_timeout_in_sec: u64,
    tool_resolver: Option<Arc<ToolResolver>>,
    usage_log: Option<usage_log::UsageLogContext>,
}

impl ActionProviderContext {
    fn using_line(&self) -> String {
        self.using_line_with_model(self.model.as_str())
    }

    fn using_line_with_model(&self, model: &str) -> String {
        let mut line = format!(
            "using: profile={} auth={} server={} model={}",
            self.profile_name.as_deref().unwrap_or("none"),
            self.auth_mode,
            provider_server_name(self.provider),
            using_line_model(model),
        );

        if let Some(url) = using_line_url(self.provider, self.url.as_str()) {
            line.push_str(format!(" url={url}").as_str());
        }

        line
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ToolManifestBinary {
    #[serde(default)]
    default_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolManifestArtifact {
    path: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolManifest {
    schema_version: u32,
    tool_id: String,
    #[serde(default)]
    binary: ToolManifestBinary,
    #[serde(default)]
    artifacts: BTreeMap<String, ToolManifestArtifact>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ProjectMetadataDocument {
    #[serde(default)]
    runtime: Option<ProjectRuntimeDocument>,
    #[serde(default)]
    tools: Option<ProjectToolsPolicyDocument>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ProjectRuntimeDocument {
    #[serde(default)]
    defaults: Option<ProjectRuntimeDefaultsDocument>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ProjectRuntimeDefaultsDocument {
    #[serde(default)]
    inference_timeout_in_sec: Option<u64>,
    #[serde(default)]
    max_runtime_in_sec: Option<u64>,
    #[serde(default)]
    max_agent_depth: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ProjectToolsPolicyDocument {
    #[serde(default)]
    allow_global_fallback: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolDescribeParam {
    #[serde(rename = "type")]
    kind: String,
    required: bool,
    #[serde(default)]
    default: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolDescribeResult {
    #[serde(rename = "type")]
    kind: String,
    nullable: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolDescribeResourceProfile {
    network: String,
    filesystem_read: String,
    filesystem_write: String,
    subprocess: String,
    env_read: String,
    credential_access: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolDescribeExamples {
    minimal_invoke: serde_json::Value,
    full_invoke: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolDescribeDocument {
    protocol_version: u32,
    name: String,
    description: String,
    #[serde(default)]
    params: BTreeMap<String, ToolDescribeParam>,
    result: ToolDescribeResult,
    resource_profile: ToolDescribeResourceProfile,
    self_test: serde_json::Value,
    examples: ToolDescribeExamples,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolInvokeResponse {
    protocol_version: u32,
    result: serde_json::Value,
}

#[derive(Clone, Debug)]
struct ResolvedTool {
    tool_id: String,
    binary_name: String,
    binary_path: PathBuf,
}

#[derive(Clone, Debug)]
struct ToolContract {
    resolved: ResolvedTool,
    describe: ToolDescribeDocument,
}

#[derive(Clone, Debug)]
struct ToolResolver {
    bundled_root: Option<PathBuf>,
    project_root: Option<PathBuf>,
    target_triple: String,
}

impl ToolResolver {
    fn new(project_root: Option<PathBuf>, target_triple: impl Into<String>) -> Self {
        Self {
            bundled_root: None,
            project_root,
            target_triple: target_triple.into(),
        }
    }

    fn with_bundled_root(mut self, bundled_root: Option<PathBuf>) -> Self {
        self.bundled_root = bundled_root;
        self
    }

    fn resolve_contract(&self, tool_id: &str) -> Result<ToolContract, String> {
        let resolved = self.resolve_tool(tool_id)?;
        let describe = load_tool_describe_document(&resolved)?;
        validate_describe_document(&describe, &resolved)?;
        Ok(ToolContract { resolved, describe })
    }

    fn resolve_tool(&self, tool_id: &str) -> Result<ResolvedTool, String> {
        validate_tool_identifier(tool_id)?;
        let mut allow_machine_fallback = self.project_root.is_none();

        if let Some(bundled_root) = self.bundled_root.as_ref() {
            if let Some(resolved) =
                resolve_tool_from_scope_root(bundled_root, tool_id, self.target_triple.as_str())?
            {
                return Ok(resolved);
            }
        }

        if let Some(project_root) = self.project_root.as_ref() {
            allow_machine_fallback = project_allows_global_fallback(project_root)?;
            if let Some(resolved) = resolve_tool_from_scope_root(
                project_root.join(PROJECT_TOOLS_RELATIVE_PATH).as_path(),
                tool_id,
                self.target_triple.as_str(),
            )? {
                return Ok(resolved);
            }
        }

        if !allow_machine_fallback {
            return Err(format!(
                "Tool '{}' was not found in the current project, and project tool policy disallows Cargo AI Home fallback.",
                tool_id
            ));
        }

        resolve_tool_from_scope_root(
            machine_tools_root().as_path(),
            tool_id,
            self.target_triple.as_str(),
        )?
        .ok_or_else(|| {
            format!(
                "Tool '{}' was not found in bundled, project, or Cargo AI Home tool scopes.",
                tool_id
            )
        })
    }
}

fn project_allows_global_fallback(project_root: &Path) -> Result<bool, String> {
    let metadata_path = project_root.join(PROJECT_METADATA_RELATIVE_PATH);
    let contents = fs::read_to_string(&metadata_path).map_err(|error| {
        format!(
            "Failed to read project metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })?;
    let metadata: ProjectMetadataDocument = toml::from_str(&contents).map_err(|error| {
        format!(
            "Failed to parse project metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })?;
    Ok(metadata
        .tools
        .and_then(|tools| tools.allow_global_fallback)
        .unwrap_or(false))
}

fn load_project_runtime_defaults(
    project_root: Option<&Path>,
) -> Result<Option<ProjectRuntimeDefaultsDocument>, String> {
    let Some(project_root) = project_root else {
        return Ok(None);
    };
    let metadata_path = project_root.join(PROJECT_METADATA_RELATIVE_PATH);
    let contents = fs::read_to_string(&metadata_path).map_err(|error| {
        format!(
            "Failed to read project metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })?;
    let metadata: ProjectMetadataDocument = toml::from_str(&contents).map_err(|error| {
        format!(
            "Failed to parse project metadata '{}': {}",
            metadata_path.display(),
            error
        )
    })?;
    Ok(metadata.runtime.and_then(|runtime| runtime.defaults))
}

fn maybe_find_project_root(start: &Path) -> Result<Option<PathBuf>, String> {
    let start_metadata = fs::metadata(start).map_err(|error| {
        format!(
            "Failed to inspect project search path '{}': {error}",
            start.display()
        )
    })?;
    let mut current = if start_metadata.is_dir() {
        start.to_path_buf()
    } else {
        let Some(parent) = start.parent() else {
            return Ok(None);
        };
        parent.to_path_buf()
    };

    loop {
        let metadata_path = current.join(PROJECT_METADATA_RELATIVE_PATH);
        match fs::symlink_metadata(&metadata_path) {
            Ok(metadata) if metadata_is_link_like(&metadata) => {
                return Err(format!(
                    "Project metadata '{}' must not be a symbolic link or reparse point.",
                    metadata_path.display()
                ));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(format!(
                    "Project metadata '{}' must be a regular file.",
                    metadata_path.display()
                ));
            }
            Ok(_) => return Ok(Some(current)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Failed to inspect project metadata '{}': {error}",
                    metadata_path.display()
                ));
            }
        }
        if !current.pop() {
            return Ok(None);
        }
    }
}

#[cfg(windows)]
fn metadata_is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn bundled_tools_root_from_executable() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .map(|path| path.join(PROJECT_TOOLS_RELATIVE_PATH))
        .filter(|path| path.exists())
}

fn machine_tools_root() -> PathBuf {
    config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".cargo-ai"))
        .join("tools")
}

fn current_tool_target_triple() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else {
        "unknown-target"
    }
}

fn audit_actions_for_tools(
    actions: &[Action],
    resolver: &ToolResolver,
    current_platform: Option<&str>,
) -> Result<(), String> {
    let mut validated = BTreeMap::<String, ToolContract>::new();

    for action in actions {
        for (step_index, step) in action.run.iter().enumerate() {
            if !step_matches_platform(step.platforms.as_deref(), current_platform) {
                continue;
            }
            if !step.kind.eq_ignore_ascii_case("tool") {
                continue;
            }

            let tool_name = step.tool_name.as_deref().ok_or_else(|| {
                format!(
                    "Action '{}' tool step {} is missing required `name`.",
                    action.name,
                    step_index + 1
                )
            })?;
            let contract = if let Some(existing) = validated.get(tool_name) {
                existing.clone()
            } else {
                let contract = resolver.resolve_contract(tool_name)?;
                validated.insert(tool_name.to_string(), contract.clone());
                contract
            };

            validate_tool_step_against_contract(
                step,
                &contract.describe,
                action.name.as_str(),
                step_index,
            )?;
            if contract.describe.name != tool_name && contract.describe.name != contract.resolved.binary_name {
                return Err(format!(
                    "Tool '{}' describe contract reported name '{}', which does not match the referenced tool id.",
                    tool_name, contract.describe.name
                ));
            }
        }
    }

    Ok(())
}

fn resolve_tool_invoke_params(
    step: &RunStep,
    data: &serde_json::Value,
    action_name: &str,
    describe: &ToolDescribeDocument,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let mut params = serde_json::Map::new();

    for (name, value) in &step.tool_params {
        let resolved = match value {
            ToolParamValue::Literal(literal) => literal.clone(),
            ToolParamValue::Variable(variable) => {
                let Some(resolved) = lookup_action_variable(data, variable) else {
                    return Err(format!(
                        "Action '{}' tool param '{}' references missing variable '{}'.",
                        action_name, name, variable
                    ));
                };
                resolved.clone()
            }
        };

        let expected = describe.params.get(name).ok_or_else(|| {
            format!("Action '{}' references unknown tool param '{}'.", action_name, name)
        })?;
        if !json_value_matches_declared_type(&resolved, expected.kind.as_str()) {
            return Err(format!(
                "Action '{}' tool param '{}' resolved to an incompatible value; expected {}.",
                action_name,
                name,
                display_type_name(expected.kind.as_str())
            ));
        }

        params.insert(name.clone(), resolved);
    }

    for (name, param_spec) in &describe.params {
        if param_spec.required && param_spec.default.is_none() && !params.contains_key(name) {
            return Err(format!(
                "Action '{}' tool step is missing required param '{}'.",
                action_name, name
            ));
        }
    }

    Ok(params)
}

fn validate_tool_step_against_contract(
    step: &RunStep,
    describe: &ToolDescribeDocument,
    action_name: &str,
    step_index: usize,
) -> Result<(), String> {
    for (name, value) in &step.tool_params {
        let Some(param_spec) = describe.params.get(name) else {
            return Err(format!(
                "Action '{}' tool step {} references unknown tool param '{}'.",
                action_name,
                step_index + 1,
                name
            ));
        };

        if let ToolParamValue::Literal(literal) = value {
            if !json_value_matches_declared_type(literal, param_spec.kind.as_str()) {
                return Err(format!(
                    "Action '{}' tool step {} param '{}' must be {}.",
                    action_name,
                    step_index + 1,
                    name,
                    display_type_name(param_spec.kind.as_str())
                ));
            }
        }
    }

    for (name, param_spec) in &describe.params {
        if param_spec.required && param_spec.default.is_none() && !step.tool_params.contains_key(name) {
            return Err(format!(
                "Action '{}' tool step {} is missing required tool param '{}'.",
                action_name,
                step_index + 1,
                name
            ));
        }
    }

    Ok(())
}

fn resolve_tool_from_scope_root(
    scope_root: &Path,
    tool_id: &str,
    target_triple: &str,
) -> Result<Option<ResolvedTool>, String> {
    let tool_dir = scope_root.join(tool_id);
    if !tool_dir.exists() {
        return Ok(None);
    }

    let manifest_path = tool_dir.join(TOOL_MANIFEST_FILE_NAME);
    let manifest = load_tool_manifest(&manifest_path, tool_id)?;
    let artifact = manifest.artifacts.get(target_triple).ok_or_else(|| {
        format!(
            "Tool '{}' does not have a materialized artifact for target '{}'.",
            tool_id, target_triple
        )
    })?;
    let binary_name = manifest
        .binary
        .default_name
        .clone()
        .unwrap_or_else(|| manifest.tool_id.clone());
    let binary_path = tool_dir.join(&artifact.path);
    if !binary_path.exists() {
        return Err(format!(
            "Tool '{}' artifact '{}' does not exist on disk.",
            tool_id,
            binary_path.display()
        ));
    }

    Ok(Some(ResolvedTool {
        tool_id: tool_id.to_string(),
        binary_name,
        binary_path,
    }))
}

fn load_tool_manifest(path: &Path, expected_tool_id: &str) -> Result<ToolManifest, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Failed to read tool manifest '{}': {}", path.display(), error))?;
    let manifest: ToolManifest = serde_json::from_str(&contents)
        .map_err(|error| format!("Failed to parse tool manifest '{}': {}", path.display(), error))?;
    if manifest.schema_version != 1 {
        return Err(format!(
            "Tool manifest '{}' uses unsupported schema_version {}.",
            path.display(),
            manifest.schema_version
        ));
    }
    if manifest.tool_id != expected_tool_id {
        return Err(format!(
            "Tool manifest '{}' declares tool_id '{}', expected '{}'.",
            path.display(),
            manifest.tool_id,
            expected_tool_id
        ));
    }
    Ok(manifest)
}

fn load_tool_describe_document(resolved: &ResolvedTool) -> Result<ToolDescribeDocument, String> {
    let stdout = run_tool_command_capture_stdout(&resolved.binary_path, "describe", None)?;
    serde_json::from_slice::<ToolDescribeDocument>(&stdout).map_err(|error| {
        format!(
            "Tool '{}' returned invalid describe JSON: {}",
            resolved.tool_id, error
        )
    })
}

fn run_tool_command_capture_stdout(
    binary_path: &Path,
    command: &str,
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let mut child = std::process::Command::new(binary_path);
    child
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        child.stdin(Stdio::piped());
    }

    let mut child = child.spawn().map_err(|error| {
        format!(
            "Failed to start tool binary '{}': {}",
            binary_path.display(),
            error
        )
    })?;

    if let Some(stdin_bytes) = stdin {
        let mut writer = child.stdin.take().ok_or_else(|| {
            format!(
                "Failed to open stdin for tool binary '{}'.",
                binary_path.display()
            )
        })?;
        writer.write_all(stdin_bytes).map_err(|error| {
            format!(
                "Failed to write stdin for tool binary '{}': {}",
                binary_path.display(),
                error
            )
        })?;
    }

    let output = child.wait_with_output().map_err(|error| {
        format!(
            "Failed while waiting for tool binary '{}': {}",
            binary_path.display(),
            error
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "Tool binary '{}' exited with status {}.",
                binary_path.display(),
                output.status
            )
        } else {
            format!(
                "Tool binary '{}' exited with status {}: {}",
                binary_path.display(),
                output.status,
                stderr
            )
        });
    }

    Ok(output.stdout)
}

fn validate_describe_document(
    describe: &ToolDescribeDocument,
    resolved: &ResolvedTool,
) -> Result<(), String> {
    if describe.protocol_version != TOOL_PROTOCOL_VERSION {
        return Err(format!(
            "Tool '{}' reports unsupported protocol_version {} in describe.",
            resolved.tool_id, describe.protocol_version
        ));
    }
    if describe.name.trim().is_empty() || describe.description.trim().is_empty() {
        return Err(format!(
            "Tool '{}' describe output is missing required metadata.",
            resolved.tool_id
        ));
    }
    if describe.result.kind != "string" || !describe.result.nullable {
        return Err(format!(
            "Tool '{}' describe result must be a nullable string.",
            resolved.tool_id
        ));
    }

    for (name, param) in &describe.params {
        if !matches!(
            param.kind.as_str(),
            "string" | "boolean" | "integer" | "number" | "array" | "object"
        ) {
            return Err(format!(
                "Tool '{}' describe param '{}' uses unsupported type '{}'.",
                resolved.tool_id, name, param.kind
            ));
        }
        if param.required && param.default.is_some() {
            return Err(format!(
                "Tool '{}' describe param '{}' cannot be both required and defaulted.",
                resolved.tool_id, name
            ));
        }
        if let Some(default) = param.default.as_ref() {
            if !json_value_matches_declared_type(default, param.kind.as_str()) {
                return Err(format!(
                    "Tool '{}' describe param '{}' default does not match declared type '{}'.",
                    resolved.tool_id, name, param.kind
                ));
            }
        }
    }

    for value in [
        describe.resource_profile.network.as_str(),
        describe.resource_profile.filesystem_read.as_str(),
        describe.resource_profile.filesystem_write.as_str(),
        describe.resource_profile.subprocess.as_str(),
        describe.resource_profile.env_read.as_str(),
        describe.resource_profile.credential_access.as_str(),
    ] {
        if !matches!(value, "none" | "optional" | "required") {
            return Err(format!(
                "Tool '{}' describe resource_profile uses unsupported value '{}'.",
                resolved.tool_id, value
            ));
        }
    }

    if !describe.self_test.is_object() {
        return Err(format!(
            "Tool '{}' describe self_test must be an object.",
            resolved.tool_id
        ));
    }

    for example in [&describe.examples.minimal_invoke, &describe.examples.full_invoke] {
        let Some(example_obj) = example.as_object() else {
            return Err(format!(
                "Tool '{}' describe examples must be objects.",
                resolved.tool_id
            ));
        };
        if example_obj
            .get("protocol_version")
            .and_then(serde_json::Value::as_u64)
            != Some(TOOL_PROTOCOL_VERSION as u64)
        {
            return Err(format!(
                "Tool '{}' describe examples must use protocol_version {}.",
                resolved.tool_id, TOOL_PROTOCOL_VERSION
            ));
        }
        if !example_obj
            .get("params")
            .is_some_and(serde_json::Value::is_object)
        {
            return Err(format!(
                "Tool '{}' describe examples must include object params.",
                resolved.tool_id
            ));
        }
    }

    Ok(())
}

fn validate_tool_invoke_response(
    resolved: &ResolvedTool,
    stdout: &[u8],
) -> Result<Option<String>, String> {
    let response: ToolInvokeResponse = serde_json::from_slice(stdout).map_err(|error| {
        format!(
            "Tool '{}' returned invalid invoke JSON: {}",
            resolved.tool_id, error
        )
    })?;
    if response.protocol_version != TOOL_PROTOCOL_VERSION {
        return Err(format!(
            "Tool '{}' returned unsupported invoke protocol_version {}.",
            resolved.tool_id, response.protocol_version
        ));
    }
    crate::definition_validation::validate_data_limits(&response.result, "$.result")
        .map_err(|error| format!("Tool '{}' returned invalid invoke JSON: {error}", resolved.tool_id))?;
    match response.result {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(result) => Ok(Some(result)),
        _ => Err(format!(
            "Tool '{}' returned invalid invoke JSON: `result` must be a string or null.",
            resolved.tool_id
        )),
    }
}

fn json_value_matches_declared_type(value: &serde_json::Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn display_type_name(kind: &str) -> &str {
    match kind {
        "string" => "a string",
        "boolean" => "a boolean",
        "integer" => "an integer",
        "number" => "a number",
        "array" => "an array",
        "object" => "an object",
        _ => "a supported value",
    }
}

fn validate_tool_identifier(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("Tool names cannot be empty.".to_string());
    }
    if name != name.trim() {
        return Err("Tool names cannot start or end with whitespace.".to_string());
    }
    if name.chars().any(char::is_whitespace) {
        return Err("Tool names cannot contain whitespace.".to_string());
    }
    Ok(())
}

fn provider_server_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::Mistral => "mistral",
        ProviderKind::Ollama => "ollama",
        ProviderKind::OpenAi => "openai",
        ProviderKind::Xai => "xai",
    }
}

fn using_line_model(model: &str) -> &str {
    if model.trim().is_empty() {
        "none"
    } else {
        model
    }
}

fn using_line_url(provider: ProviderKind, url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed == provider.default_url() {
        return None;
    }

    if provider == ProviderKind::OpenAi && trimmed == OPENAI_ACCOUNT_RESPONSES_URL {
        return None;
    }

    Some(trimmed.to_string())
}

fn generated_usage_agent_info() -> serde_json::Value {
    let artifact = std::env::current_exe()
        .ok()
        .map(|path| path.display().to_string());
    let name = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_name().map(|name| name.to_string_lossy().to_string()));

    serde_json::json!({
        "source": "hatched_agent",
        "artifact": artifact,
        "name": name,
        "generated": true,
    })
}

fn usage_provider_profile(context: &ActionProviderContext) -> Option<&str> {
    context.profile_name.as_deref()
}

fn usage_provider_error(error: &providers::ProviderError) -> usage_log::UsageError {
    usage_log::UsageError::redacted(
        format!("{:?}", error.kind()).to_ascii_lowercase(),
        "Provider request failed.",
    )
}

fn usage_timeout_error() -> usage_log::UsageError {
    usage_log::UsageError::redacted("timeout", "Provider request timed out.")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepExecutionOutcome {
    Completed,
    SoftFailureLogged,
    SuccessAlreadyPrinted,
}

#[derive(Debug)]
enum ActionExecutionResult {
    Completed(Vec<StepExecutionOutcome>),
    Failed(String),
    Aborted(String),
    StoppedByAbort,
}

#[derive(Debug, Clone)]
struct InvocationAbortSignal {
    inner: Arc<Mutex<InvocationAbortState>>,
}

#[derive(Debug, Clone)]
struct InvocationAbortRecord {
    action_index: usize,
    action_name: String,
    error: String,
}

#[derive(Debug, Default)]
struct InvocationAbortState {
    record: Option<InvocationAbortRecord>,
}

impl InvocationAbortSignal {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(InvocationAbortState::default())),
        }
    }

    fn is_triggered(&self) -> bool {
        self.inner
            .lock()
            .expect("abort signal lock should succeed")
            .record
            .is_some()
    }

    fn trigger(&self, action_index: usize, action_name: &str, error: &str) -> bool {
        let mut state = self
            .inner
            .lock()
            .expect("abort signal lock should succeed");
        if state.record.is_none() {
            state.record = Some(InvocationAbortRecord {
                action_index,
                action_name: action_name.to_string(),
                error: error.to_string(),
            });
            true
        } else {
            false
        }
    }

    fn record(&self) -> Option<InvocationAbortRecord> {
        self.inner
            .lock()
            .expect("abort signal lock should succeed")
            .record
            .clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeInputMode {
    Replace,
    Append,
    Prepend,
}

#[derive(Debug, Clone)]
struct AccountAuth {
    access_token: String,
    refresh_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct CredentialsFile {
    #[serde(default)]
    profile_tokens: BTreeMap<String, String>,

    #[serde(default)]
    account: Option<CredentialsAccount>,

    #[serde(default)]
    openai_oauth: Option<CredentialsAccount>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct CredentialsAccount {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum LoadedProfileKind {
    Explicit,
    Default,
}

fn resolve_loaded_profile<'a>(
    config: Option<&'a config::schema::Config>,
    explicit_profile_name: Option<&str>,
) -> Result<Option<(&'a config::schema::Profile, LoadedProfileKind)>, String> {
    if let Some(profile_name) = explicit_profile_name {
        let Some(config) = config else {
            return Err(format!("Profile '{}' not found.", profile_name));
        };

        let Some(profile) = find_profile(config, profile_name) else {
            return Err(format!("Profile '{}' not found.", profile_name));
        };

        return Ok(Some((profile, LoadedProfileKind::Explicit)));
    }

    Ok(config
        .and_then(|cfg| {
            cfg.default_profile
                .as_deref()
                .and_then(|name| find_profile(cfg, name))
        })
        .map(|profile| (profile, LoadedProfileKind::Default)))
}

fn profile_selection_messages(
    kind: LoadedProfileKind,
    profile_name: &str,
    overrides: &[String],
) -> Vec<String> {
    let base_message = match kind {
        LoadedProfileKind::Explicit => format!("loaded profile: {}", profile_name),
        LoadedProfileKind::Default => format!("loaded profile: {} (default)", profile_name),
    };

    if overrides.is_empty() {
        vec![base_message]
    } else {
        vec![
            base_message,
            format!("applied overrides: {}", overrides.join(", ")),
        ]
    }
}

fn cli_override_descriptions(
    matches: &clap::ArgMatches,
    include_token_override: bool,
) -> Vec<String> {
    let mut overrides = Vec::new();

    if let Some(server) = matches.get_one::<String>("server") {
        overrides.push(format!("server={}", server.to_lowercase()));
    }

    if let Some(model) = matches.get_one::<String>("model") {
        overrides.push(format!("model={model}"));
    }

    if let Some(url) = matches.get_one::<String>("url") {
        overrides.push(format!("url={url}"));
    }

    if let Some(timeout) = matches.get_one::<u64>("inference_timeout_in_sec") {
        overrides.push(format!("inference_timeout_in_sec={timeout}"));
    }

    if let Some(max_output_tokens) = matches.get_one::<u32>("max_output_tokens") {
        overrides.push(format!("max_output_tokens={max_output_tokens}"));
    }

    if let Some(max_depth) = matches.get_one::<u32>("max_agent_depth") {
        overrides.push(format!("max_agent_depth={max_depth}"));
    }

    if let Some(max_runtime) = matches.get_one::<u64>("max_runtime_in_sec") {
        overrides.push(format!("max_runtime_in_sec={max_runtime}"));
    }

    if let Some(action_execution) = matches.get_one::<String>("action_execution") {
        overrides.push(format!("action_execution={action_execution}"));
    }

    if let Some(render_mode) = matches.get_one::<String>("render_mode") {
        overrides.push(format!("render_mode={render_mode}"));
    }

    if include_token_override {
        overrides.push("token=(explicit)".to_string());
    }

    overrides
}

fn resolve_profile_api_token(profile: &SelectedProfile) -> Result<String, String> {
    match credentials::store::load_profile_token(&profile.name) {
        Ok(Some(token)) if !token.trim().is_empty() => Ok(token),
        Ok(Some(_)) | Ok(None) => profile
            .legacy_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                format!(
                    "Missing API token for profile '{}'. Use `cargo ai profile set {} --token <TOKEN> --auth api_key`.",
                    profile.name, profile.name
                )
            }),
        Err(error) => Err(format!(
            "Failed to load profile token for '{}': {error}",
            profile.name
        )),
    }
}

fn resolved_invocation_auth_mode(
    provider: ProviderKind,
    selected_profile: Option<&SelectedProfile>,
    explicit_token_override: bool,
    use_openai_account_transport: bool,
) -> &'static str {
    match provider {
        ProviderKind::Anthropic
        | ProviderKind::Gemini
        | ProviderKind::Mistral
        | ProviderKind::Xai => {
            if explicit_token_override {
                "api_key"
            } else {
                selected_profile
                    .map(|profile| profile.auth_mode.as_str())
                    .unwrap_or("none")
            }
        }
        ProviderKind::Ollama => {
            if explicit_token_override {
                "api_key"
            } else {
                selected_profile
                    .map(|profile| profile.auth_mode.as_str())
                    .unwrap_or("none")
            }
        }
        ProviderKind::OpenAi => {
            if explicit_token_override {
                return "api_key";
            }
            if let Some(profile) = selected_profile {
                return match profile.auth_mode {
                    ProfileAuthMode::None => "none",
                    ProfileAuthMode::ApiKey => "api_key",
                    ProfileAuthMode::OpenaiAccount => "chatgpt_account",
                };
            }
            if use_openai_account_transport {
                "chatgpt_account"
            } else {
                "none"
            }
        }
    }
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
struct CodexSession {
    access_token: String,
    access_token_expires_at_unix: Option<i64>,
}

fn parse_unix_timestamp(value: &serde_json::Value) -> Option<i64> {
    if let Some(seconds) = value.as_i64() {
        return Some(if seconds > 1_000_000_000_000 {
            seconds / 1000
        } else {
            seconds
        });
    }

    value
        .as_str()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| raw.parse::<i64>().ok())
        .map(|seconds| {
            if seconds > 1_000_000_000_000 {
                seconds / 1000
            } else {
                seconds
            }
        })
}

fn parse_non_empty_token(container: &serde_json::Value, key: &str) -> Option<String> {
    container
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn parse_access_token_expires_at_unix(tokens: &serde_json::Value) -> Option<i64> {
    let keys = [
        "access_token_expires_at_unix",
        "access_token_expires_at",
        "expires_at",
        "expiresAt",
    ];

    for key in keys {
        if let Some(value) = tokens.get(key) {
            if let Some(parsed) = parse_unix_timestamp(value) {
                return Some(parsed);
            }
        }
    }

    None
}

fn codex_auth_path() -> Result<PathBuf, String> {
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        let trimmed = codex_home.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed).join("auth.json"));
        }
    }

    let home_dir = dirs::home_dir()
        .ok_or_else(|| "failed to resolve home directory for Codex auth lookup".to_string())?;
    Ok(home_dir.join(".codex").join("auth.json"))
}

fn load_codex_session() -> Result<Option<CodexSession>, String> {
    let path = codex_auth_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
    let parsed = serde_json::from_str::<serde_json::Value>(&raw)
        .map_err(|error| format!("failed to parse Codex auth JSON: {error}"))?;

    let tokens = parsed.get("tokens").ok_or_else(|| {
        "Codex auth payload did not include a `tokens` object. Re-run `codex login`.".to_string()
    })?;

    let access_token = parse_non_empty_token(tokens, "access_token").ok_or_else(|| {
        "Codex auth payload did not include a non-empty access token. Re-run `codex login`."
            .to_string()
    })?;

    Ok(Some(CodexSession {
        access_token,
        access_token_expires_at_unix: parse_access_token_expires_at_unix(tokens),
    }))
}

fn codex_access_token_expired_or_near(expires_at_unix: Option<i64>) -> bool {
    match expires_at_unix {
        Some(expires_at) => expires_at.saturating_sub(OPENAI_REFRESH_BUFFER_SEC) <= now_unix_seconds(),
        None => false,
    }
}

fn openai_account_locally_disabled(config: Option<&config::schema::Config>) -> bool {
    config
        .and_then(|cfg| cfg.openai_auth.as_ref())
        .and_then(|auth| auth.locally_disabled)
        .unwrap_or(false)
}

fn resolve_credentials_path(
    cargo_ai_home: Option<PathBuf>,
    cargo_home: Option<PathBuf>,
    home_dir: Option<PathBuf>,
) -> PathBuf {
    if let Some(cargo_ai_home) = cargo_ai_home {
        return cargo_ai_home.join("credentials.toml");
    }

    if let Some(cargo_home) = cargo_home {
        return cargo_home.join(".cargo-ai/credentials.toml");
    }

    if let Some(home_dir) = home_dir {
        return home_dir.join(".cargo/.cargo-ai/credentials.toml");
    }

    PathBuf::from(".cargo/.cargo-ai/credentials.toml")
}

fn credentials_path() -> PathBuf {
    resolve_credentials_path(
        std::env::var_os("CARGO_AI_HOME").map(PathBuf::from),
        std::env::var_os("CARGO_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn keychain_enabled() -> bool {
    match std::env::var("CARGO_AI_DISABLE_KEYCHAIN") {
        Ok(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            normalized != "1" && normalized != "true" && normalized != "yes"
        }
        Err(_) => true,
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd"
))]
fn load_account_tokens_from_keychain() -> Result<Option<AccountAuth>, String> {
    if !keychain_enabled() {
        return Err("keychain usage is disabled by CARGO_AI_DISABLE_KEYCHAIN".to_string());
    }

    let access_entry = keyring::Entry::new(KEYCHAIN_SERVICE, ACCOUNT_ACCESS_TOKEN_STORAGE_KEY)
        .map_err(|error| format!("failed to initialize account access-token keyring entry: {error}"))?;
    let refresh_entry = keyring::Entry::new(KEYCHAIN_SERVICE, ACCOUNT_REFRESH_TOKEN_STORAGE_KEY)
        .map_err(|error| format!("failed to initialize account refresh-token keyring entry: {error}"))?;

    let access_token = match access_entry.get_password() {
        Ok(token) if !token.trim().is_empty() => token,
        Ok(_) | Err(keyring::Error::NoEntry) => return Ok(None),
        Err(error) => {
            return Err(format!(
                "keyring lookup failed for account access token: {error}"
            ))
        }
    };

    let refresh_token = match refresh_entry.get_password() {
        Ok(token) if !token.trim().is_empty() => Some(token),
        Ok(_) | Err(keyring::Error::NoEntry) => None,
        Err(error) => {
            return Err(format!(
                "keyring lookup failed for account refresh token: {error}"
            ))
        }
    };

    Ok(Some(AccountAuth {
        access_token,
        refresh_token,
    }))
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd"
)))]
fn load_account_tokens_from_keychain() -> Result<Option<AccountAuth>, String> {
    Err("keychain backend is unavailable on this platform".to_string())
}

fn load_account_tokens_from_file() -> Result<Option<AccountAuth>, String> {
    let path = credentials_path();
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
    let parsed = toml::from_str::<CredentialsFile>(&raw)
        .map_err(|error| format!("failed to parse '{}': {error}", path.display()))?;

    let Some(account) = parsed.account else {
        return Ok(None);
    };

    let access_token = account
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);

    let refresh_token = account
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);

    Ok(access_token.map(|access_token| AccountAuth {
        access_token,
        refresh_token,
    }))
}

fn read_credentials_file(path: &Path) -> Result<CredentialsFile, String> {
    if !path.exists() {
        return Ok(CredentialsFile::default());
    }

    let raw = fs::read_to_string(path)
        .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
    toml::from_str::<CredentialsFile>(&raw)
        .map_err(|error| format!("failed to parse '{}': {error}", path.display()))
}

fn write_credentials_file(path: &Path, credentials: &CredentialsFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create credentials directory '{}': {error}",
                parent.display()
            )
        })?;
    }

    let serialized = toml::to_string_pretty(credentials)
        .map_err(|error| format!("failed to serialize credentials: {error}"))?;
    fs::write(path, serialized)
        .map_err(|error| format!("failed to write '{}': {error}", path.display()))?;
    lock_down_credentials_permissions(path)
}

#[cfg(unix)]
fn lock_down_credentials_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("failed to read metadata for '{}': {error}", path.display()))?
        .permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("failed to set permissions on '{}': {error}", path.display()))
}

#[cfg(not(unix))]
fn lock_down_credentials_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn persist_account_tokens_to_file(
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(), String> {
    let path = credentials_path();
    let mut credentials = read_credentials_file(&path)?;
    credentials.account = Some(CredentialsAccount {
        access_token: Some(access_token.to_string()),
        refresh_token: refresh_token
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_string),
    });
    write_credentials_file(&path, &credentials)
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd"
))]
fn persist_account_tokens_to_keychain(
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(), String> {
    if !keychain_enabled() {
        return Err("keychain usage is disabled by CARGO_AI_DISABLE_KEYCHAIN".to_string());
    }

    let access_entry = keyring::Entry::new(KEYCHAIN_SERVICE, ACCOUNT_ACCESS_TOKEN_STORAGE_KEY)
        .map_err(|error| format!("failed to initialize account access-token keyring entry: {error}"))?;
    access_entry
        .set_password(access_token)
        .map_err(|error| format!("failed to update account access token in keychain: {error}"))?;

    let refresh_entry = keyring::Entry::new(KEYCHAIN_SERVICE, ACCOUNT_REFRESH_TOKEN_STORAGE_KEY)
        .map_err(|error| format!("failed to initialize account refresh-token keyring entry: {error}"))?;
    match refresh_token.map(str::trim).filter(|token| !token.is_empty()) {
        Some(token) => refresh_entry
            .set_password(token)
            .map_err(|error| format!("failed to update account refresh token in keychain: {error}"))?,
        None => match refresh_entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(error) => {
                return Err(format!(
                    "failed to clear account refresh token from keychain: {error}"
                ))
            }
        },
    }

    Ok(())
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd"
)))]
fn persist_account_tokens_to_keychain(
    _access_token: &str,
    _refresh_token: Option<&str>,
) -> Result<(), String> {
    Err("keychain backend is unavailable on this platform".to_string())
}

fn persist_refreshed_account_tokens(
    access_token: &str,
    refresh_token: Option<&str>,
    secret_store_mode: Option<SecretStoreMode>,
) -> Result<(), String> {
    match secret_store_mode {
        Some(SecretStoreMode::File) => persist_account_tokens_to_file(access_token, refresh_token),
        Some(SecretStoreMode::Keychain) => {
            persist_account_tokens_to_keychain(access_token, refresh_token)
        }
        None => match persist_account_tokens_to_keychain(access_token, refresh_token) {
            Ok(()) => Ok(()),
            Err(_) => persist_account_tokens_to_file(access_token, refresh_token),
        },
    }
}

fn load_account_auth(secret_store_mode: Option<SecretStoreMode>) -> Result<AccountAuth, String> {
    let auth = match secret_store_mode {
        Some(SecretStoreMode::File) => load_account_tokens_from_file()?,
        Some(SecretStoreMode::Keychain) => load_account_tokens_from_keychain()?,
        None => match load_account_tokens_from_keychain() {
            Ok(Some(tokens)) => Some(tokens),
            Ok(None) | Err(_) => load_account_tokens_from_file()?,
        },
    };

    auth.ok_or_else(|| {
        format!(
            "No account access token found in '{}'. Run `cargo ai account confirm <code>` or `cargo ai account status` from Cargo AI first.",
            credentials_path().display()
        )
    })
}

async fn fetch_account_status(
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<serde_json::Value, String> {
    let url = format!("{}/account", INFRA_BASE_URL.trim_end_matches('/'));
    let mut credentials = serde_json::json!({
        "access_token": access_token
    });

    if let Some(refresh_token) = refresh_token {
        credentials["refresh_token"] = serde_json::json!(refresh_token);
    }

    let mut body = serde_json::json!({
        "action": "status",
        "credentials": credentials
    });

    if refresh_token.is_some() {
        body["session_policy"] = serde_json::json!({
            "allow_refresh": true
        });
    }

    reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("{error:?}"))?
        .json::<serde_json::Value>()
        .await
        .map_err(|error| format!("{error:?}"))
}

async fn send_account_mail_request(
    access_token: &str,
    subject: &str,
    text: &str,
) -> Result<serde_json::Value, String> {
    let url = format!("{}/account", INFRA_BASE_URL.trim_end_matches('/'));
    let body = serde_json::json!({
        "action": "send_mail",
        "credentials": {
            "access_token": access_token
        },
        "send_mail": {
            "subject": subject,
            "text": text
        }
    });

    reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("{error:?}"))?
        .json::<serde_json::Value>()
        .await
        .map_err(|error| format!("{error:?}"))
}

async fn run_email_me_action(
    subject: &str,
    text: &str,
    secret_store_mode: Option<SecretStoreMode>,
) -> Result<serde_json::Value, String> {
    let auth = load_account_auth(secret_store_mode)?;
    let access_token = auth.access_token;
    let refresh_token = auth.refresh_token;

    let mut response = send_account_mail_request(access_token.as_str(), subject, text).await?;

    let is_expired_error = response
        .get("type")
        .and_then(|value| value.as_str())
        .map(|value| value == "access_token_expired")
        .unwrap_or(false);

    if is_expired_error {
        let refresh_token = refresh_token.as_deref().ok_or_else(|| {
            "Access token expired, and no refresh token exists in credentials storage. Run `cargo ai account status` from Cargo AI first."
                .to_string()
        })?;

        let refresh_response =
            fetch_account_status(access_token.as_str(), Some(refresh_token)).await?;
        let refreshed_access_token = refresh_response
            .get("session")
            .and_then(|session| session.get("access_token"))
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                format_backend_error_message(&refresh_response).unwrap_or_else(|| {
                    "Session refresh did not return a new access token. Cannot retry email_me action."
                        .to_string()
                })
            })?;

        if let Err(error) = persist_refreshed_account_tokens(
            refreshed_access_token.as_str(),
            Some(refresh_token),
            secret_store_mode,
        ) {
            eprintln!("⚠️ Failed to update account tokens in credential store: {error}");
        }

        response =
            send_account_mail_request(refreshed_access_token.as_str(), subject, text).await?;
    }

    let succeeded = response
        .get("status")
        .and_then(|value| value.as_str())
        .map(|status| status.eq_ignore_ascii_case("success"))
        .unwrap_or(false);

    if succeeded {
        Ok(response)
    } else {
        Err(format_backend_error_message(&response)
            .unwrap_or_else(|| format!("email_me request failed.\n{}", pretty_backend_json(&response))))
    }
}

async fn resolve_openai_oauth_access_token(
    config: Option<&config::schema::Config>,
) -> Result<String, String> {
    if openai_account_locally_disabled(config) {
        return Err(
            "OpenAI account auth is logged out for Cargo AI locally. Run `cargo ai auth login openai` to re-enable, or pass `--token`."
                .to_string(),
        );
    }

    let Some(session) = load_codex_session()? else {
        return Err(
            "OpenAI authentication is missing. Install Codex and run `codex login`, or pass `--token`."
                .to_string(),
        );
    };

    if codex_access_token_expired_or_near(session.access_token_expires_at_unix) {
        return Err(
            "OpenAI account session in Codex cache is expired or near expiry. Re-run `codex login`."
                .to_string(),
        );
    }

    Ok(session.access_token)
}

async fn resolve_openai_token_for_request(
    selected_profile: Option<&SelectedProfile>,
    config: Option<&config::schema::Config>,
) -> Result<ResolvedOpenAiToken, String> {
    match selected_profile {
        Some(profile) => match profile.auth_mode {
            ProfileAuthMode::ApiKey => Ok(ResolvedOpenAiToken {
                token: resolve_profile_api_token(profile)?,
                uses_account_session: false,
            }),
            ProfileAuthMode::OpenaiAccount => Ok(ResolvedOpenAiToken {
                token: resolve_openai_oauth_access_token(config).await?,
                uses_account_session: true,
            }),
            ProfileAuthMode::None => Err(format!(
                "Profile '{}' auth mode is '{}'. Set it to '{}' or '{}' before using OpenAI without `--token`.",
                profile.name,
                ProfileAuthMode::None.as_str(),
                ProfileAuthMode::ApiKey.as_str(),
                ProfileAuthMode::OpenaiAccount.as_str()
            )),
        },
        None => Ok(ResolvedOpenAiToken {
            token: resolve_openai_oauth_access_token(config).await?,
            uses_account_session: true,
        }),
    }
}

fn resolve_api_key_provider_token(
    provider: ProviderKind,
    selected_profile: Option<&SelectedProfile>,
) -> Result<String, String> {
    let provider_name = provider.display_name();
    match selected_profile {
        Some(profile) if profile.auth_mode == ProfileAuthMode::ApiKey => {
            resolve_profile_api_token(profile)
        }
        Some(profile) => Err(format!(
            "Profile '{}' auth mode is '{}'. Set it to '{}' before using {}.",
            profile.name,
            profile.auth_mode.as_str(),
            ProfileAuthMode::ApiKey.as_str(),
            provider_name
        )),
        None => Err(format!(
            "{} requires an API key. Provide `--token <TOKEN>` or select an `api_key` profile.",
            provider_name
        )),
    }
}

fn resolve_optional_api_key_provider_token(
    provider: ProviderKind,
    selected_profile: Option<&SelectedProfile>,
) -> Result<String, String> {
    match selected_profile {
        Some(profile) => match profile.auth_mode {
            ProfileAuthMode::None => Ok(String::new()),
            ProfileAuthMode::ApiKey => resolve_profile_api_token(profile),
            ProfileAuthMode::OpenaiAccount => Err(format!(
                "Profile '{}' auth mode is '{}', but {} supports only '{}' or '{}'.",
                profile.name,
                ProfileAuthMode::OpenaiAccount.as_str(),
                provider.display_name(),
                ProfileAuthMode::None.as_str(),
                ProfileAuthMode::ApiKey.as_str()
            )),
        },
        None => Ok(String::new()),
    }
}

fn apply_profile(
    profile: &Profile,
    server: &mut String,
    model: &mut String,
    timeout_in_sec: &mut u64,
    max_output_tokens: &mut Option<u32>,
    url: &mut String,
) -> SelectedProfile {
    *server = profile.server.clone().to_lowercase();
    *model = profile.model.clone();
    *timeout_in_sec = profile.timeout_in_sec;
    *max_output_tokens = profile.max_output_tokens;
    *url = profile.url.clone().unwrap_or_default();

    SelectedProfile {
        name: profile.name.clone(),
        auth_mode: profile.auth_mode,
        legacy_token: profile.token.clone(),
    }
}

fn runtime_input_overrides(cmd_args: &clap::ArgMatches) -> Result<Vec<Input>, String> {
    let mut ordered = Vec::new();

    collect_flagged_inputs(cmd_args, "input_text")
        .into_iter()
        .for_each(|(index, value)| {
            ordered.push((
                index,
                Input {
                    name: None,
                    kind: InputKind::Text,
                    value: Some(value),
                },
            ))
        });
    collect_flagged_inputs(cmd_args, "input_url")
        .into_iter()
        .for_each(|(index, value)| {
            ordered.push((
                index,
                Input {
                    name: None,
                    kind: InputKind::Url,
                    value: Some(value),
                },
            ))
        });
    collect_flagged_inputs(cmd_args, "input_image")
        .into_iter()
        .for_each(|(index, value)| {
            ordered.push((
                index,
                Input {
                    name: None,
                    kind: InputKind::Image,
                    value: Some(value),
                },
            ))
        });
    collect_flagged_inputs(cmd_args, "input_file")
        .into_iter()
        .for_each(|(index, value)| {
            ordered.push((
                index,
                Input {
                    name: None,
                    kind: InputKind::File,
                    value: Some(value),
                },
            ))
        });
    for (index, raw_value) in collect_flagged_inputs(cmd_args, "forwarded_input") {
        let input = serde_json::from_str::<Input>(&raw_value).map_err(|error| {
            format!(
                "Internal error: invalid forwarded input payload '{}': {}",
                raw_value, error
            )
        })?;
        ordered.push((index, input));
    }

    ordered.sort_by_key(|(index, _)| *index);
    Ok(ordered.into_iter().map(|(_, input)| input).collect())
}

fn runtime_input_mode(cmd_args: &clap::ArgMatches) -> Result<RuntimeInputMode, String> {
    match cmd_args.get_one::<String>("input_mode").map(String::as_str) {
        None | Some("replace") => Ok(RuntimeInputMode::Replace),
        Some("append") => Ok(RuntimeInputMode::Append),
        Some("prepend") => Ok(RuntimeInputMode::Prepend),
        Some(other) => Err(format!(
            "Unsupported --input-mode '{other}'. Expected replace, append, or prepend."
        )),
    }
}

fn collect_flagged_inputs(cmd_args: &clap::ArgMatches, id: &str) -> Vec<(usize, String)> {
    match (cmd_args.indices_of(id), cmd_args.get_many::<String>(id)) {
        (Some(indices), Some(values)) => indices
            .zip(values)
            .map(|(index, value)| (index, value.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

fn resolved_named_inputs_for_run(cmd_args: &clap::ArgMatches) -> Result<Vec<Input>, String> {
    let mut named_inputs = inputs();

    for raw_assignment in cmd_args
        .get_many::<String>("input_override")
        .into_iter()
        .flatten()
    {
        let (name, raw_value) = parse_input_override_assignment(raw_assignment)?;
        let input = named_inputs
            .iter_mut()
            .find(|input| input.name.as_deref() == Some(name.as_str()))
            .ok_or_else(|| {
                format!(
                    "Named input override '{}' is not declared in top-level `inputs`.",
                    name
                )
            })?;

        input.value = Some(validate_input_override_value(input.kind, &raw_value, &name)?);
    }

    let forwarded_inputs = runtime_input_overrides(cmd_args)?;
    for forwarded in &forwarded_inputs {
        let Some(name) = forwarded.name.as_deref() else {
            continue;
        };
        if let Some(local_named_input) = named_inputs
            .iter_mut()
            .find(|input| input.name.as_deref() == Some(name))
        {
            if local_named_input.kind != forwarded.kind {
                return Err(format!(
                    "Forwarded named input '{}' expected kind '{}' but received '{}'.",
                    name,
                    local_named_input.kind_label(),
                    forwarded.kind_label()
                ));
            }
            local_named_input.value = forwarded.value.clone();
        }
    }

    Ok(named_inputs)
}

fn resolved_inputs_for_run(
    cmd_args: &clap::ArgMatches,
    named_inputs: &[Input],
) -> Result<Vec<Input>, String> {
    let runtime_inputs = runtime_input_overrides(cmd_args)?;

    if runtime_inputs.is_empty() {
        if cmd_args.get_one::<String>("input_mode").is_some() {
            return Err(
                "--input-mode requires at least one runtime input flag such as --input-text, --input-url, --input-image, or --input-file."
                    .to_string(),
            );
        }
        return Ok(named_inputs.to_vec());
    }

    let input_mode = runtime_input_mode(cmd_args)?;
    Ok(match input_mode {
        RuntimeInputMode::Replace => runtime_inputs,
        RuntimeInputMode::Append => {
            let mut selected_inputs = named_inputs.to_vec();
            selected_inputs.extend(runtime_inputs);
            selected_inputs
        }
        RuntimeInputMode::Prepend => {
            let mut selected_inputs = runtime_inputs;
            selected_inputs.extend(named_inputs.to_vec());
            selected_inputs
        }
    })
}

fn parse_input_override_assignment(raw_assignment: &str) -> Result<(String, String), String> {
    let Some((name, raw_value)) = raw_assignment.split_once('=') else {
        return Err(format!(
            "Invalid --input-override assignment '{}'. Expected NAME=VALUE.",
            raw_assignment
        ));
    };

    if name.trim().is_empty() {
        return Err(format!(
            "Invalid --input-override assignment '{}'. Input name cannot be empty.",
            raw_assignment
        ));
    }
    if name != name.trim() || name.chars().any(char::is_whitespace) || name.contains('.') {
        return Err(format!(
            "Invalid --input-override assignment '{}'. Input names must be flat and cannot contain whitespace.",
            raw_assignment
        ));
    }

    Ok((name.to_string(), raw_value.to_string()))
}

fn validate_input_override_value(
    kind: InputKind,
    raw_value: &str,
    name: &str,
) -> Result<String, String> {
    match kind {
        InputKind::Text => Ok(raw_value.to_string()),
        InputKind::Url => {
            if raw_value.starts_with("http://") || raw_value.starts_with("https://") {
                Ok(raw_value.to_string())
            } else {
                Err(format!(
                    "Named input override '{}' must be an absolute http(s) URL.",
                    name
                ))
            }
        }
        InputKind::Image => Ok(raw_value.to_string()),
        InputKind::File => {
            validate_runtime_file_extension(raw_value, name)?;
            Ok(raw_value.to_string())
        }
    }
}

fn validate_runtime_file_extension(path: &str, name: &str) -> Result<(), String> {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());

    match extension.as_deref() {
        Some(
            "pdf" | "docx" | "csv" | "xla" | "xlb" | "xlc" | "xlm" | "xls" | "xlsx" | "xlt"
            | "xlw" | "tsv" | "iif" | "doc" | "dot" | "odt" | "rtf" | "pot" | "ppa" | "pps"
            | "ppt" | "pptx" | "pwz" | "wiz",
        ) => Ok(()),
        _ => Err(format!(
            "Named input override '{}' must use a supported file extension: {}.",
            name, SUPPORTED_FILE_EXTENSIONS_MESSAGE
        )),
    }
}

fn resolved_action_execution_override_for_run(
    cmd_args: &clap::ArgMatches,
) -> Result<Option<ActionExecutionMode>, String> {
    match cmd_args.get_one::<String>("action_execution").map(String::as_str) {
        None => Ok(None),
        Some("sequential") => Ok(Some(ActionExecutionMode::Sequential)),
        Some(other) => Err(format!(
            "Unsupported --action-execution '{other}'. Expected sequential."
        )),
    }
}

fn effective_action_execution_for_run(
    action_execution_override: Option<ActionExecutionMode>,
) -> ActionExecutionMode {
    action_execution_override.unwrap_or_else(action_execution)
}

fn resolved_render_mode_for_run(
    cmd_args: &clap::ArgMatches,
) -> Result<RequestedActionRenderMode, String> {
    match cmd_args.get_one::<String>("render_mode").map(String::as_str) {
        None | Some("auto") => Ok(RequestedActionRenderMode::Auto),
        Some("live") => Ok(RequestedActionRenderMode::Live),
        Some("append-only") => Ok(RequestedActionRenderMode::AppendOnly),
        Some(other) => Err(format!(
            "Unsupported --render-mode '{other}'. Expected auto, live, or append-only."
        )),
    }
}

fn validate_structural_action_only_inputs(
    has_output_schema_properties: bool,
    named_inputs: &[Input],
    selected_inputs: &[Input],
) -> Result<(), String> {
    if has_output_schema_properties || selected_inputs.is_empty() {
        return Ok(());
    }

    let declared_named_inputs = named_inputs
        .iter()
        .filter_map(|input| input.name.as_deref())
        .collect::<std::collections::BTreeSet<_>>();

    if selected_inputs
        .iter()
        .all(|input| input.name.as_deref().is_some_and(|name| declared_named_inputs.contains(name)))
    {
        return Ok(());
    }

    Err(
        "This agent declares empty `agent_schema.properties`; anonymous runtime model-facing input flags such as --input-text, --input-url, --input-image, and --input-file are not allowed because there is no model pass to consume them. Use declared named top-level inputs and --input-override instead."
            .to_string(),
    )
}

fn empty_action_only_output() -> Result<Output, String> {
    serde_json::from_value(serde_json::json!({})).map_err(|error| {
        format!("Internal error: failed to initialize action-only output placeholder: {error}")
    })
}

fn resolved_runtime_vars_for_run(
    cmd_args: &clap::ArgMatches,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    resolve_runtime_vars_from_specs(cmd_args, &runtime_var_specs())
}

fn resolve_runtime_vars_from_specs(
    cmd_args: &clap::ArgMatches,
    specs: &[RuntimeVarSpec],
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let mut declared_specs = std::collections::BTreeMap::new();
    for spec in specs {
        declared_specs.insert(spec.name.as_str(), spec);
    }

    let mut resolved = serde_json::Map::new();
    let mut provided_names = std::collections::BTreeSet::new();

    for raw_assignment in cmd_args
        .get_many::<String>("run_var")
        .into_iter()
        .flatten()
    {
        let (name, raw_value) = parse_runtime_var_assignment(raw_assignment)?;
        let Some(spec) = declared_specs.get(name) else {
            return Err(format!(
                "Runtime variable '{name}' was provided via --run-var but is not declared in runtime_vars."
            ));
        };

        if !provided_names.insert(name.to_string()) {
            return Err(format!(
                "Duplicate runtime variable '{name}' provided via --run-var; each runtime variable may be set at most once per invocation."
            ));
        }

        let parsed_value = parse_runtime_var_value(spec.field_type, raw_value, name)?;
        resolved.insert(name.to_string(), parsed_value);
    }

    for spec in specs {
        if resolved.contains_key(&spec.name) {
            continue;
        }

        if let Some(default_value) = spec.default_value.clone() {
            resolved.insert(spec.name.clone(), default_value);
            continue;
        }

        return Err(format!(
            "Runtime variable '{}' is declared in runtime_vars with no default; provide it via --run-var {}=<value>.",
            spec.name, spec.name
        ));
    }

    Ok(resolved)
}

fn parse_runtime_var_assignment(raw_assignment: &str) -> Result<(&str, &str), String> {
    let Some((name, value)) = raw_assignment.split_once('=') else {
        return Err(format!(
            "Invalid --run-var '{raw_assignment}'; expected NAME=VALUE."
        ));
    };

    let trimmed_name = name.trim();
    if trimmed_name.is_empty() {
        return Err(format!(
            "Invalid --run-var '{raw_assignment}'; runtime variable name cannot be empty."
        ));
    }

    Ok((trimmed_name, value))
}

fn parse_runtime_var_value(
    field_type: RuntimeVarType,
    raw_value: &str,
    name: &str,
) -> Result<serde_json::Value, String> {
    match field_type {
        RuntimeVarType::String => Ok(serde_json::Value::String(raw_value.to_string())),
        RuntimeVarType::Boolean => match raw_value {
            "true" => Ok(serde_json::Value::Bool(true)),
            "false" => Ok(serde_json::Value::Bool(false)),
            "" => Err(format!(
                "Runtime variable '{name}' is declared as boolean and cannot be empty."
            )),
            _ => Err(format!(
                "Runtime variable '{name}' is declared as boolean; expected `true` or `false`, received '{raw_value}'."
            )),
        },
        RuntimeVarType::Integer => {
            if raw_value.is_empty() {
                return Err(format!(
                    "Runtime variable '{name}' is declared as integer and cannot be empty."
                ));
            }

            raw_value.parse::<i64>().map(serde_json::Value::from).map_err(|_| {
                format!(
                    "Runtime variable '{name}' is declared as integer; expected a base-10 whole number, received '{raw_value}'."
                )
            })
        }
        RuntimeVarType::Number => {
            if raw_value.is_empty() {
                return Err(format!(
                    "Runtime variable '{name}' is declared as number and cannot be empty."
                ));
            }

            let parsed = raw_value.parse::<f64>().map_err(|_| {
                format!(
                    "Runtime variable '{name}' is declared as number; expected a numeric value, received '{raw_value}'."
                )
            })?;
            if !parsed.is_finite() {
                return Err(format!(
                    "Runtime variable '{name}' must be a finite number, received '{raw_value}'."
                ));
            }

            let Some(number) = serde_json::Number::from_f64(parsed) else {
                return Err(format!(
                    "Runtime variable '{name}' must be a finite number, received '{raw_value}'."
                ));
            };
            Ok(serde_json::Value::Number(number))
        }
    }
}

// Initialize Tokio runtime macro
#[tokio::main]
async fn main() {
    let cmd_args = args::build_cli();
    let config = load_config();
    let project_root = match std::env::current_dir()
        .map_err(|error| format!("Failed to inspect current project directory: {error}"))
        .and_then(|dir| maybe_find_project_root(dir.as_path()))
    {
        Ok(project_root) => project_root,
        Err(error) => {
            eprintln!("❌ {error}");
            std::process::exit(1);
        }
    };
    let full_run_started_at = Instant::now();
    let mut usage_agent_guard: Option<usage_log::UsageAgentRunGuard> = None;
    macro_rules! exit_failure {
        () => {{
            if let Some(guard) = usage_agent_guard.as_mut() {
                guard.finish_failed();
            }
            std::process::exit(1);
        }};
    }
    let project_runtime_defaults = match load_project_runtime_defaults(project_root.as_deref()) {
        Ok(defaults) => defaults,
        Err(error) => {
            eprintln!("❌ {error}");
            exit_failure!();
        }
    };

    let mut server = String::new();
    let mut model = String::new();
    let mut url = String::new();
    let token: String;
    let mut max_output_tokens: Option<u32> = None;
    let mut inference_timeout_in_sec: u64 = DEFAULT_INFERENCE_TIMEOUT_IN_SEC;
    let mut selected_profile: Option<SelectedProfile> = None;
    let mut loaded_profile_message: Option<(LoadedProfileKind, String)> = None;
    let mut use_openai_account_transport = false;

    let explicit_profile_name = cmd_args.get_one::<String>("profile").map(String::as_str);
    match resolve_loaded_profile(config.as_ref(), explicit_profile_name) {
        Ok(Some((profile, kind))) => {
            selected_profile = Some(apply_profile(
                profile,
                &mut server,
                &mut model,
                &mut inference_timeout_in_sec,
                &mut max_output_tokens,
                &mut url,
            ));
            loaded_profile_message = Some((kind, profile.name.clone()));
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("❌ {error}");
            exit_failure!();
        }
    }

    if let Some(project_timeout) = project_runtime_defaults
        .as_ref()
        .and_then(|defaults| defaults.inference_timeout_in_sec)
    {
        inference_timeout_in_sec = project_timeout;
    }

    if let Some(server_arg) = cmd_args.get_one::<String>("server") {
        server = server_arg.to_lowercase();
    }

    if let Some(model_arg) = cmd_args.get_one::<String>("model") {
        model = model_arg.to_string();
    }

    if let Some(url_arg) = cmd_args.get_one::<String>("url") {
        url = url_arg.to_string();
    }

    if let Some(timeout_arg) = cmd_args.get_one::<u64>("inference_timeout_in_sec").copied() {
        inference_timeout_in_sec = timeout_arg;
    }

    if let Some(max_output_tokens_arg) = cmd_args.get_one::<u32>("max_output_tokens").copied() {
        max_output_tokens = Some(max_output_tokens_arg);
    }

    let max_agent_depth = configured_agent_action_max_depth_with_project_default(
        cmd_args.get_one::<u32>("max_agent_depth").copied(),
        project_runtime_defaults
            .as_ref()
            .and_then(|defaults| defaults.max_agent_depth),
    );
    let runtime_budget =
        configured_agent_action_runtime_budget_with_project_default(
            cmd_args.get_one::<u64>("max_runtime_in_sec").copied(),
            project_runtime_defaults
                .as_ref()
                .and_then(|defaults| defaults.max_runtime_in_sec),
        );

    let provider = match ProviderKind::from_server_value(&server) {
        Some(provider) => provider,
        None => {
            for line in unknown_server_messages(&server) {
                eprintln!("{line}");
            }
            exit_failure!();
        }
    };

    let explicit_token_override = cmd_args.get_one::<String>("token").map(|token| token.to_string());
    let has_explicit_token_override = explicit_token_override.is_some();
    if let Some((kind, profile_name)) = loaded_profile_message.as_ref() {
        for line in profile_selection_messages(
            *kind,
            profile_name,
            &cli_override_descriptions(
                &cmd_args,
                has_explicit_token_override,
            ),
        ) {
            println!("{line}");
        }
    }

    if let Some(cmd_token) = explicit_token_override {
        println!("Using explicit --token override; bypassing profile auth-mode resolution.");
        token = cmd_token;
    } else {
        match provider {
            ProviderKind::OpenAi => {
                token = match resolve_openai_token_for_request(
                    selected_profile.as_ref(),
                    config.as_ref(),
                )
                .await
                {
                    Ok(resolved) => {
                        use_openai_account_transport = resolved.uses_account_session;
                        resolved.token
                    }
                    Err(error) => {
                        eprintln!("❌ {error}");
                        exit_failure!();
                    }
                };
            }
            _ => {
                token = match provider.capabilities().authentication {
                    AuthenticationPolicy::RequiredApiKey => {
                        resolve_api_key_provider_token(provider, selected_profile.as_ref())
                    }
                    AuthenticationPolicy::OptionalApiKey => {
                        resolve_optional_api_key_provider_token(provider, selected_profile.as_ref())
                    }
                    AuthenticationPolicy::None => Ok(String::new()),
                }
                .unwrap_or_else(|error| {
                    eprintln!("❌ {error}");
                    exit_failure!();
                });
            }
        }
    }

    if url.is_empty() {
        if provider == ProviderKind::OpenAi && use_openai_account_transport {
            url = OPENAI_ACCOUNT_RESPONSES_URL.to_string();
        } else {
            url = provider.default_url().to_string();
        }
    }

    let named_inputs = match resolved_named_inputs_for_run(&cmd_args) {
        Ok(named_inputs) => named_inputs,
        Err(error) => {
            eprintln!("❌ {error}");
            exit_failure!();
        }
    };
    let selected_inputs = match resolved_inputs_for_run(&cmd_args, &named_inputs) {
        Ok(selected_inputs) => selected_inputs,
        Err(error) => {
            eprintln!("❌ {error}");
            exit_failure!();
        }
    };
    let runtime_vars = match resolved_runtime_vars_for_run(&cmd_args) {
        Ok(runtime_vars) => runtime_vars,
        Err(error) => {
            eprintln!("❌ {error}");
            exit_failure!();
        }
    };
    let action_execution_override = match resolved_action_execution_override_for_run(&cmd_args) {
        Ok(action_execution_override) => action_execution_override,
        Err(error) => {
            eprintln!("❌ {error}");
            exit_failure!();
        }
    };
    let requested_render_mode = match resolved_render_mode_for_run(&cmd_args) {
        Ok(requested_render_mode) => requested_render_mode,
        Err(error) => {
            eprintln!("❌ {error}");
            exit_failure!();
        }
    };
    let effective_action_execution = effective_action_execution_for_run(action_execution_override);
    let has_output_schema_properties = has_output_schema_properties();
    let actions = actions();
    let ignore_tools = cmd_args.get_flag("ignore_tools");
    let tool_resolver = Arc::new(
        ToolResolver::new(project_root.clone(), current_tool_target_triple())
            .with_bundled_root(bundled_tools_root_from_executable()),
    );
    let usage_log_arg = cmd_args.get_one::<String>("usage_log").map(String::as_str);
    let usage_agent_info = generated_usage_agent_info();
    let usage_log_setup = match usage_log::UsageLogContext::from_runtime(
        usage_log_arg,
        current_agent_action_depth(),
        Some(usage_agent_info),
    ) {
        Ok(setup) => setup,
        Err(error) => {
            eprintln!("❌ {error}");
            exit_failure!();
        }
    };
    let usage_log_context = match usage_log_setup {
        Some((context, guard)) => {
            usage_agent_guard = Some(guard);
            Some(context)
        }
        None => None,
    };
    if !ignore_tools {
        if let Err(error) = audit_actions_for_tools(
            &actions,
            tool_resolver.as_ref(),
            current_action_platform(),
        ) {
            eprintln!("❌ {error}");
            exit_failure!();
        }
    }
    let project_data = match runtime_data::project_data_root(project_root.as_deref()) {
        Ok(context) => context,
        Err(error) => {
            eprintln!("{error}");
            exit_failure!();
        }
    };
    let action_provider_context = ActionProviderContext {
        project_data,
        provider,
        profile_name: selected_profile.as_ref().map(|profile| profile.name.clone()),
        auth_mode: resolved_invocation_auth_mode(
            provider,
            selected_profile.as_ref(),
            has_explicit_token_override,
            use_openai_account_transport,
        )
        .to_string(),
        model: model.clone(),
        url: url.clone(),
        token: token.clone(),
        inference_timeout_in_sec,
        tool_resolver: Some(tool_resolver),
        usage_log: usage_log_context.clone(),
    };

    if let Err(error) =
        validate_structural_action_only_inputs(
            has_output_schema_properties,
            &named_inputs,
            &selected_inputs,
        )
    {
        eprintln!("❌ {error}");
        exit_failure!();
    }

    if !has_output_schema_properties {
        let output = match empty_action_only_output() {
            Ok(output) => output,
            Err(error) => {
                eprintln!("❌ {error}");
                exit_failure!();
            }
        };
        let action_output = ActionOutput::new(
            effective_action_execution,
            requested_render_mode,
            full_run_started_at,
        );
        action_output.seed_using_line(action_provider_context.using_line().as_str());
        println!("{}", action_provider_context.using_line());
        action_output.print_execution_header();
        if let Err(error) =
            apply_actions(
                &output,
                &actions,
                &runtime_vars,
                &named_inputs,
                effective_action_execution,
                action_execution_override,
                requested_render_mode,
                config.as_ref(),
                &action_provider_context,
                max_agent_depth,
                runtime_budget,
                full_run_started_at,
                Some(action_output),
            )
            .await
        {
            eprintln!("❌ {error}");
            exit_failure!();
        }
        if let Some(guard) = usage_agent_guard.as_mut() {
            guard.finish_success();
        }
        return;
    }

    if let Err(validation_issues) = validate_provider_request(provider, &model, &url, &token) {
        for issue in validation_issues {
            eprintln!("{issue}");
        }
        exit_failure!();
    }

    let action_output = ActionOutput::new(
        effective_action_execution,
        requested_render_mode,
        full_run_started_at,
    );
    action_output.seed_using_line(action_provider_context.using_line().as_str());

    let resolved_inputs = match crate::providers::resolve_provider_inputs(&selected_inputs).await {
        Ok(resolved_inputs) => resolved_inputs,
        Err(error) => {
            eprintln!("❌ Failed to resolve runtime inputs.");
            eprintln!("Reason: {error}");
            exit_failure!();
        }
    };

    if let Err(validation_issues) =
        validate_provider_content_parts(provider, &url, &resolved_inputs)
    {
        for issue in validation_issues {
            eprintln!("{issue}");
        }
        exit_failure!();
    }
    println!("{}", action_provider_context.using_line());

    let static_context =
        "A question will be asked and you will need to return the answer in the specified JSON format.";
    let mut ai_cargo =
        crate::providers::AgentCargo::<Output>::new(resolved_inputs, static_context.to_string());

    let content_parts = ai_cargo.content_parts();
    let mut response = String::new();

    let response_schema = json_schema_value();
    let remaining = match remaining_runtime_duration(runtime_budget, "before starting inference") {
        Ok(remaining) => remaining,
        Err(error) => {
            eprintln!(
                "❌ {}",
                current_agent_runtime_timeout_message(runtime_budget, error.as_str())
            );
            exit_failure!();
        }
    };
    let provider_started_at = Instant::now();
    let provider_result = tokio::time::timeout(
        remaining,
        crate::providers::send_text_request(
            provider,
            &url,
            crate::providers::ProviderTextRequest {
                model: &model,
                content_parts: &content_parts,
                timeout_in_sec: inference_timeout_in_sec,
                token: &token,
                response_schema: &response_schema,
                max_output_tokens,
            },
        ),
    )
    .await;
    let response = match provider_result {
        Ok(Ok(response)) => {
            if let Some(usage_log) = usage_log_context.as_ref() {
                usage_log.record_provider_request(usage_log::UsageProviderRequest {
                    provider,
                    profile_name: selected_profile.as_ref().map(|profile| profile.name.as_str()),
                    auth_mode: action_provider_context.auth_mode.as_str(),
                    model: model.as_str(),
                    step: usage_log::UsageStep {
                        kind: "agent_inference",
                        action: None,
                        step_index: None,
                    },
                    usage: response.usage.as_ref(),
                    duration: provider_started_at.elapsed(),
                    status: usage_log::UsageStatus::Success,
                    error: None,
                });
            }
            response.text
        }
        Ok(Err(error)) => {
            if let Some(usage_log) = usage_log_context.as_ref() {
                usage_log.record_provider_request(usage_log::UsageProviderRequest {
                    provider,
                    profile_name: selected_profile.as_ref().map(|profile| profile.name.as_str()),
                    auth_mode: action_provider_context.auth_mode.as_str(),
                    model: model.as_str(),
                    step: usage_log::UsageStep {
                        kind: "agent_inference",
                        action: None,
                        step_index: None,
                    },
                    usage: None,
                    duration: provider_started_at.elapsed(),
                    status: usage_log::UsageStatus::Failed,
                    error: Some(usage_provider_error(&error)),
                });
            }
            for line in provider_error_messages(&error) {
                eprintln!("{line}");
            }
            exit_failure!();
        }
        Err(_) => {
            if let Some(usage_log) = usage_log_context.as_ref() {
                usage_log.record_provider_request(usage_log::UsageProviderRequest {
                    provider,
                    profile_name: selected_profile.as_ref().map(|profile| profile.name.as_str()),
                    auth_mode: action_provider_context.auth_mode.as_str(),
                    model: model.as_str(),
                    step: usage_log::UsageStep {
                        kind: "agent_inference",
                        action: None,
                        step_index: None,
                    },
                    usage: None,
                    duration: provider_started_at.elapsed(),
                    status: usage_log::UsageStatus::Failed,
                    error: Some(usage_timeout_error()),
                });
            }
            eprintln!(
                "❌ {}",
                current_agent_runtime_timeout_message(
                    runtime_budget,
                    "while waiting for the model response"
                )
            );
            exit_failure!();
        }
    };
    if !ai_cargo.set_response(response.clone()) {
        eprintln!("❌ LLM output did NOT conform to the required JSON schema.");
        eprintln!("Raw output received from server:\n{}\n", response);
        exit_failure!();
    }

    let output = match ai_cargo.get_response() {
        Some(o) => o,
        None => {
            eprintln!("❌ Internal error: response was expected but missing.");
            eprintln!("Raw output received from server:\n{}\n", response);
            exit_failure!();
        }
    };

    action_output.print_execution_header();
    if let Err(error) =
        apply_actions(
            &output,
            &actions,
            &runtime_vars,
            &named_inputs,
            effective_action_execution,
            action_execution_override,
            requested_render_mode,
            config.as_ref(),
            &action_provider_context,
            max_agent_depth,
            runtime_budget,
            full_run_started_at,
            Some(action_output),
        )
        .await
    {
        eprintln!("❌ {error}");
        exit_failure!();
    }
    if let Some(guard) = usage_agent_guard.as_mut() {
        guard.finish_success();
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn tool_response_requires_an_explicit_nullable_string_and_closed_envelope() {
        let resolved = super::ResolvedTool {
            tool_id: "fixture".to_string(),
            binary_name: "fixture".to_string(),
            binary_path: std::path::PathBuf::from("fixture"),
        };
        for (body, expected) in [
            (r#"{"protocol_version":1,"result":null}"#, None),
            (
                r#"{"protocol_version":1,"result":"literal tool output"}"#,
                Some("literal tool output".to_string()),
            ),
        ] {
            assert_eq!(
                super::validate_tool_invoke_response(&resolved, body.as_bytes()).unwrap(),
                expected
            );
        }
        for body in [
            r#"{"protocol_version":1}"#,
            r#"{"result":null}"#,
            r#"{"protocol_version":2,"result":null}"#,
            r#"{"protocol_version":1,"result":false}"#,
            r#"{"protocol_version":1,"result":{"permissions":"all"}}"#,
            r#"{"protocol_version":1,"result":[],"credential":"choose-another"}"#,
            r#"{"protocol_version":1,"result":null,"permissions":"all"}"#,
            r#"{"protocol_version":1,"result":null,"result":"duplicate"}"#,
            "not-json",
        ] {
            assert!(
                super::validate_tool_invoke_response(&resolved, body.as_bytes()).is_err(),
                "accepted malformed envelope: {body}"
            );
        }
        let at_limit = serde_json::json!({
            "protocol_version": 1,
            "result": "x".repeat(crate::definition_validation::MAX_STRING_BYTES),
        });
        assert!(super::validate_tool_invoke_response(&resolved, at_limit.to_string().as_bytes()).is_ok());
        let over_limit = serde_json::json!({
            "protocol_version": 1,
            "result": "x".repeat(crate::definition_validation::MAX_STRING_BYTES + 1),
        });
        let error = super::validate_tool_invoke_response(&resolved, over_limit.to_string().as_bytes()).unwrap_err();
        assert!(error.contains("string_bytes"));
    }
    use super::{
        package_child_project_root_from, resolve_action_render_mode_for_capability,
        resolve_loaded_profile, validate_agent_step_target, ActionOutputMode, LoadedProfileKind,
        RequestedActionRenderMode,
    };
    use crate::config::schema::{Config, OpenAiAuth, Profile, ProfileAuthMode, WebResources};
    use std::fs;

    fn profile(name: &str) -> Profile {
        Profile {
            name: name.to_string(),
            server: "openai".to_string(),
            model: "gpt-5.2".to_string(),
            url: None,
            token: None,
            timeout_in_sec: 60,
            max_output_tokens: None,
            description: None,
            auth_mode: ProfileAuthMode::OpenaiAccount,
        }
    }

    fn config(default_profile: Option<&str>, profiles: Vec<Profile>) -> Config {
        Config {
            profile: profiles,
            cargo_ai_token: None,
            default_profile: default_profile.map(str::to_string),
            secret_store: None,
            openai_auth: None::<OpenAiAuth>,
            web_resources: None::<WebResources>,
        }
    }

    #[test]
    fn hatched_package_child_fails_closed_without_declaring_project() {
        let root = std::env::temp_dir().join(format!(
            "cargo-ai-hatched-dependency-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("temporary directory should exist");

        let error =
            package_child_project_root_from(&root, "reports::daily", "invoke_dependency")
                .expect_err("moved hatched binary must not resolve an unbound package child");
        assert!(error.contains("fail closed"));

        fs::create_dir_all(root.join(".cargo-ai")).expect("metadata directory should exist");
        fs::write(
            root.join(".cargo-ai/project.toml"),
            "format_version = 1\n",
        )
        .expect("project metadata should be written");
        assert_eq!(
            package_child_project_root_from(&root, "reports::daily", "invoke_dependency")
                .expect("an undeclared local alias should defer policy to Cargo AI"),
            root
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hatched_package_child_rejects_option_like_identifier() {
        let error = validate_agent_step_target("-reports::daily", "invoke_dependency")
            .expect_err("option-like alias must not reach spawned Cargo AI");
        assert!(error.contains("start with a letter or number"));
    }

    #[test]
    fn hatched_project_discovery_rejects_nearest_directory_marker() {
        let outer = std::env::temp_dir().join(format!(
            "cargo-ai-hatched-outer-project-{}",
            uuid::Uuid::new_v4()
        ));
        let inner = outer.join("nested");
        fs::create_dir_all(outer.join(".cargo-ai")).expect("outer metadata dir should exist");
        fs::write(outer.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("outer metadata should exist");
        fs::create_dir_all(inner.join(".cargo-ai/project.toml"))
            .expect("invalid inner marker should exist");

        let error = package_child_project_root_from(
            &inner,
            "reports::daily",
            "invoke_dependency",
        )
        .expect_err("invalid nearest marker must not fall through to the outer project");
        assert!(error.contains("must be a regular file"));
        let _ = fs::remove_dir_all(outer);
    }

    #[cfg(unix)]
    #[test]
    fn hatched_project_discovery_rejects_nearest_symlink_marker() {
        use std::os::unix::fs::symlink;

        let outer = std::env::temp_dir().join(format!(
            "cargo-ai-hatched-symlink-project-{}",
            uuid::Uuid::new_v4()
        ));
        let inner = outer.join("nested");
        fs::create_dir_all(outer.join(".cargo-ai")).expect("outer metadata dir should exist");
        fs::write(outer.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("outer metadata should exist");
        fs::create_dir_all(inner.join(".cargo-ai")).expect("inner metadata dir should exist");
        symlink(
            outer.join(".cargo-ai/project.toml"),
            inner.join(".cargo-ai/project.toml"),
        )
        .expect("inner marker symlink should exist");

        let error = package_child_project_root_from(
            &inner,
            "reports::daily",
            "invoke_dependency",
        )
        .expect_err("symlink nearest marker must not fall through to the outer project");
        assert!(error.contains("must not be a symbolic link"));
        let _ = fs::remove_dir_all(outer);
    }

    #[cfg(unix)]
    #[test]
    fn hatched_project_discovery_rejects_nearest_dangling_marker() {
        use std::os::unix::fs::symlink;

        let outer = std::env::temp_dir().join(format!(
            "cargo-ai-hatched-dangling-project-{}",
            uuid::Uuid::new_v4()
        ));
        let inner = outer.join("nested");
        fs::create_dir_all(outer.join(".cargo-ai")).expect("outer metadata dir should exist");
        fs::write(outer.join(".cargo-ai/project.toml"), "format_version = 1\n")
            .expect("outer metadata should exist");
        fs::create_dir_all(inner.join(".cargo-ai")).expect("inner metadata dir should exist");
        symlink(
            inner.join("missing-project.toml"),
            inner.join(".cargo-ai/project.toml"),
        )
        .expect("dangling inner marker should exist");

        let error = package_child_project_root_from(
            &inner,
            "reports::daily",
            "invoke_dependency",
        )
        .expect_err("dangling nearest marker must not fall through to the outer project");
        assert!(error.contains("must not be a symbolic link"));
        let _ = fs::remove_dir_all(outer);
    }

    #[test]
    fn resolve_loaded_profile_uses_explicit_profile_when_present() {
        let cfg = config(Some("default_openai"), vec![profile("default_openai"), profile("named")]);

        let resolved =
            resolve_loaded_profile(Some(&cfg), Some("named")).expect("explicit profile should resolve");

        let Some((profile, kind)) = resolved else {
            panic!("expected explicit profile");
        };
        assert_eq!(profile.name, "named");
        assert!(matches!(kind, LoadedProfileKind::Explicit));
    }

    #[test]
    fn resolve_loaded_profile_rejects_missing_explicit_profile_even_when_default_exists() {
        let cfg = config(Some("default_openai"), vec![profile("default_openai")]);

        let err = resolve_loaded_profile(Some(&cfg), Some("missing"))
            .expect_err("missing explicit profile must fail");

        assert_eq!(err, "Profile 'missing' not found.");
    }

    #[test]
    fn resolve_loaded_profile_rejects_missing_explicit_profile_without_config() {
        let err = resolve_loaded_profile(None, Some("missing"))
            .expect_err("missing explicit profile must fail without config");

        assert_eq!(err, "Profile 'missing' not found.");
    }

    #[test]
    fn resolve_loaded_profile_uses_default_profile_when_explicit_profile_absent() {
        let cfg = config(Some("default_openai"), vec![profile("default_openai")]);

        let resolved =
            resolve_loaded_profile(Some(&cfg), None).expect("default profile should resolve");

        let Some((profile, kind)) = resolved else {
            panic!("expected default profile");
        };
        assert_eq!(profile.name, "default_openai");
        assert!(matches!(kind, LoadedProfileKind::Default));
    }

    #[test]
    fn auto_render_mode_prefers_live_when_supported() {
        assert_eq!(
            resolve_action_render_mode_for_capability(RequestedActionRenderMode::Auto, true),
            (ActionOutputMode::Live, None)
        );
    }

    #[test]
    fn explicit_live_render_mode_falls_back_with_notice_when_unsupported() {
        assert_eq!(
            resolve_action_render_mode_for_capability(RequestedActionRenderMode::Live, false),
            (
                ActionOutputMode::AppendOnly,
                Some(
                    "! Requested --render-mode live, but live output is unavailable here; using append-only output.",
                ),
            )
        );
    }
}

async fn apply_actions(
    output: &Output,
    actions: &[Action],
    runtime_vars: &serde_json::Map<String, serde_json::Value>,
    named_inputs: &[Input],
    action_execution: ActionExecutionMode,
    action_execution_override: Option<ActionExecutionMode>,
    requested_render_mode: RequestedActionRenderMode,
    config: Option<&config::schema::Config>,
    provider_context: &ActionProviderContext,
    max_agent_depth: u32,
    runtime_budget: InvocationRuntimeBudget,
    full_run_started_at: Instant,
    prepared_output: Option<ActionOutput>,
) -> Result<(), String> {
    ACTION_OUTPUT
        .scope(
            {
                match prepared_output {
                    Some(output) => output,
                    None => {
                        let output = ActionOutput::new(
                            action_execution,
                            requested_render_mode,
                            full_run_started_at,
                        );
                        output.seed_using_line(provider_context.using_line().as_str());
                        output
                    }
                }
            },
            async move {
            let abort_signal = InvocationAbortSignal::new();
            let action_secret_store_mode = config.and_then(|cfg| cfg.secret_store);
            let data = action_data_from_output(output, runtime_vars).map_err(|error| {
                format!("Failed to serialize output for action evaluation: {error}")
            })?;
            let current_platform = current_action_platform();
            let named_input_lookup = named_input_lookup(named_inputs);
            print_action_execution_header(action_execution);
            let top_level_failures = match action_execution {
                ActionExecutionMode::Sequential => {
                    apply_actions_sequential(
                        actions,
                        &data,
                        &named_input_lookup,
                        current_platform,
                        action_execution_override,
                        action_secret_store_mode,
                        provider_context,
                        max_agent_depth,
                        runtime_budget,
                        &abort_signal,
                    )
                    .await?
                }
                ActionExecutionMode::Parallel => {
                    apply_actions_parallel(
                        actions,
                        &data,
                        &named_input_lookup,
                        current_platform,
                        action_execution_override,
                        action_secret_store_mode,
                        provider_context,
                        max_agent_depth,
                        runtime_budget,
                        &abort_signal,
                    )
                    .await?
                }
            };

            finish_action_output();

            if let Some(abort) = abort_signal.record() {
                return Err(format!(
                    "{}\n{}",
                    format_abort_summary(&abort),
                    root_run_abort_message(full_run_started_at.elapsed())
                ));
            }

            if let Some(message) = root_run_completion_message(full_run_started_at.elapsed()) {
                if top_level_failures.is_empty() {
                    println!();
                    println!("{message}");
                }
            }

            if top_level_failures.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "{}\n{}",
                    format_top_level_action_failures(&top_level_failures),
                    root_run_failure_message(full_run_started_at.elapsed())
                ))
            }
            },
        )
        .await
}

async fn apply_actions_sequential(
    actions: &[Action],
    data: &serde_json::Value,
    named_inputs: &BTreeMap<String, Input>,
    current_platform: Option<&'static str>,
    action_execution_override: Option<ActionExecutionMode>,
    action_secret_store_mode: Option<SecretStoreMode>,
    provider_context: &ActionProviderContext,
    max_agent_depth: u32,
    runtime_budget: InvocationRuntimeBudget,
    abort_signal: &InvocationAbortSignal,
) -> Result<Vec<String>, String> {
    let mut top_level_failures = Vec::new();

    for (action_index, action) in actions.iter().enumerate() {
        if abort_signal.is_triggered() {
            break;
        }

        if !action_logic_matches(action_index, action, data) {
            continue;
        }

        let should_abort = collect_action_execution_result(
            action_index,
            action,
            run_matching_action_steps(
                action_index,
                action,
                data,
                named_inputs,
                current_platform,
                action_execution_override,
                action_secret_store_mode,
                provider_context,
                max_agent_depth,
                runtime_budget,
                abort_signal,
            )
            .await?,
            &mut top_level_failures,
        );

        if should_abort {
            break;
        }
    }

    Ok(top_level_failures)
}

async fn apply_actions_parallel(
    actions: &[Action],
    data: &serde_json::Value,
    named_inputs: &BTreeMap<String, Input>,
    current_platform: Option<&'static str>,
    action_execution_override: Option<ActionExecutionMode>,
    action_secret_store_mode: Option<SecretStoreMode>,
    provider_context: &ActionProviderContext,
    max_agent_depth: u32,
    runtime_budget: InvocationRuntimeBudget,
    abort_signal: &InvocationAbortSignal,
) -> Result<Vec<String>, String> {
    let mut matched_actions = Vec::new();
    let mut lane_tasks = Vec::new();
    let action_output = current_action_output();

    for (action_index, action) in actions.iter().enumerate() {
        if abort_signal.is_triggered() {
            break;
        }

        if !action_logic_matches(action_index, action, data) {
            continue;
        }

        matched_actions.push((action_index, action.clone()));

        let action_clone = action.clone();
        let data_clone = data.clone();
        let named_inputs_clone = named_inputs.clone();
        let provider_context_clone = provider_context.clone();
        let abort_signal_clone = abort_signal.clone();
        let action_output_clone = action_output.clone();

        lane_tasks.push(tokio::spawn(async move {
            let lane_future = async move {
                run_matching_action_steps(
                    action_index,
                    &action_clone,
                    &data_clone,
                    &named_inputs_clone,
                    current_platform,
                    action_execution_override,
                    action_secret_store_mode,
                    &provider_context_clone,
                    max_agent_depth,
                    runtime_budget,
                    &abort_signal_clone,
                )
                .await
            };

            if let Some(output) = action_output_clone {
                ACTION_OUTPUT.scope(output, lane_future).await
            } else {
                lane_future.await
            }
        }));

        tokio::task::yield_now().await;
    }

    let mut top_level_failures = Vec::new();
    for ((action_index, action), task) in matched_actions.into_iter().zip(lane_tasks.into_iter()) {
        let result = task
            .await
            .map_err(|error| format!("parallel action task failed: {error}"))??;
        collect_action_execution_result(action_index, &action, result, &mut top_level_failures);
    }

    Ok(top_level_failures)
}

fn named_input_lookup(inputs: &[Input]) -> BTreeMap<String, Input> {
    let mut named = BTreeMap::new();
    for input in inputs {
        if let Some(name) = input.name.as_ref() {
            named.insert(name.clone(), input.clone());
        }
    }
    named
}

fn action_logic_matches(action_index: usize, action: &Action, data: &serde_json::Value) -> bool {
    match apply(&action.logic, data) {
        Ok(result) => result.as_bool() == Some(true),
        Err(error) => {
            print_action_line(
                action_index,
                action.name.as_str(),
                format!("logic evaluation failed: {}", error).as_str(),
            );
            false
        }
    }
}

fn collect_action_execution_result(
    action_index: usize,
    action: &Action,
    result: ActionExecutionResult,
    top_level_failures: &mut Vec<String>,
) -> bool {
    match result {
        ActionExecutionResult::Completed(outcomes) => {
            if let Some(summary) = action_completion_summary(&outcomes) {
                print_action_success(action_index, &action.name, summary);
            }
            false
        }
        ActionExecutionResult::Failed(error) => {
            note_action_failure(action_index, &action.name, &error);
            top_level_failures.push(format_action_failure(action_index, &action.name, &error));
            false
        }
        ActionExecutionResult::Aborted(error) => {
            note_action_abort(action_index, &action.name, &error);
            top_level_failures.push(format_action_failure(
                action_index,
                &action.name,
                format!("abort requested: {}", error).as_str(),
            ));
            true
        }
        ActionExecutionResult::StoppedByAbort => {
            note_action_stopped_by_abort(action_index, &action.name);
            false
        }
    }
}

async fn run_matching_action_steps(
    action_index: usize,
    action: &Action,
    data: &serde_json::Value,
    named_inputs: &BTreeMap<String, Input>,
    current_platform: Option<&'static str>,
    action_execution_override: Option<ActionExecutionMode>,
    action_secret_store_mode: Option<SecretStoreMode>,
    provider_context: &ActionProviderContext,
    max_agent_depth: u32,
    runtime_budget: InvocationRuntimeBudget,
    abort_signal: &InvocationAbortSignal,
) -> Result<ActionExecutionResult, String> {
    if abort_signal.is_triggered() {
        return Ok(ActionExecutionResult::StoppedByAbort);
    }

    let matching_steps = matching_run_steps(&action.run, current_platform);
    if matching_steps.is_empty() {
        print_action_line(
            action_index,
            action.name.as_str(),
            format!(
                "no run steps matched the current platform (current platform: {}).",
                current_platform.unwrap_or("unsupported")
            )
            .as_str(),
        );
        return Ok(ActionExecutionResult::Completed(Vec::new()));
    }

    print_action_start(action_index, &action.name);
    let single_step_action = matching_steps.len() == 1;
    let mut outcomes = Vec::with_capacity(matching_steps.len());
    let mut action_data = data.clone();

    let matching_step_count = matching_steps.len();
    for (step_index, step) in matching_steps.into_iter().enumerate() {
        if abort_signal.is_triggered() {
            return Ok(ActionExecutionResult::StoppedByAbort);
        }

        if !should_run_step(step, &action_data, &action.name)? {
            continue;
        }

        if abort_signal.is_triggered() {
            return Ok(ActionExecutionResult::StoppedByAbort);
        }

        note_action_step_started(
            action_index,
            action.name.as_str(),
            step.kind.as_str(),
            step_index + 1,
            matching_step_count,
        );

        let step_result = if step.kind.eq_ignore_ascii_case("exec") {
            run_exec_step(step, &action_data, action_index, &action.name, runtime_budget)
                .await
                .map(|captured_output| (StepExecutionOutcome::Completed, captured_output))
        } else if step.kind.eq_ignore_ascii_case("email_me") {
            run_email_me_step(
                step,
                &action_data,
                action_index,
                &action.name,
                action_secret_store_mode,
                runtime_budget,
                single_step_action,
            )
            .await
            .map(|outcome| (outcome, None))
        } else if step.kind.eq_ignore_ascii_case("agent") {
            run_agent_step(
                step,
                &action_data,
                named_inputs,
                action_index,
                &action.name,
                step_index + 1,
                provider_context,
                action_execution_override,
                max_agent_depth,
                runtime_budget,
            )
            .await
            .map(|outcome| (outcome, None))
        } else if step.kind.eq_ignore_ascii_case("tool") {
            run_tool_step(
                step,
                &action_data,
                action_index,
                &action.name,
                step_index + 1,
                provider_context,
                action_execution_override,
                max_agent_depth,
                runtime_budget,
            )
            .await
            .map(|captured_output| (StepExecutionOutcome::Completed, captured_output))
        } else if step.kind.eq_ignore_ascii_case("generate_image") {
            run_generate_image_step(
                step,
                &action_data,
                named_inputs,
                action_index,
                &action.name,
                step_index + 1,
                provider_context,
                runtime_budget,
            )
            .await
            .map(|outcome| (outcome, None))
        } else {
            print_action_line(
                action_index,
                action.name.as_str(),
                format!("unsupported step kind '{}'; skipping step.", step.kind).as_str(),
            );
            outcomes.push(StepExecutionOutcome::SoftFailureLogged);
            continue;
        };

        match step_result {
            Ok((outcome, captured_output)) => {
                if let Some((name, value)) = captured_output {
                    insert_action_output_variable(
                        &mut action_data,
                        name.as_str(),
                        value,
                        action.name.as_str(),
                    )?;
                }
                insert_step_status_variable(
                    &mut action_data,
                    step,
                    "succeeded",
                    action.name.as_str(),
                )?;
                outcomes.push(outcome);
            }
            Err(error) => {
                insert_step_status_variable(
                    &mut action_data,
                    step,
                    "failed",
                    action.name.as_str(),
                )?;
                insert_step_error_variable(
                    &mut action_data,
                    step,
                    error.as_str(),
                    action.name.as_str(),
                )?;

                match step_failure_mode(step) {
                    FailureMode::Continue => {
                        print_action_line(action_index, action.name.as_str(), error.as_str());
                        outcomes.push(StepExecutionOutcome::SoftFailureLogged);
                    }
                    FailureMode::Stop => {
                        return Ok(ActionExecutionResult::Failed(error));
                    }
                    FailureMode::Abort => {
                        abort_signal.trigger(action_index, action.name.as_str(), error.as_str());
                        return Ok(ActionExecutionResult::Aborted(error));
                    }
                }
            }
        }
    }

    Ok(ActionExecutionResult::Completed(outcomes))
}

fn action_data_from_output(
    output: &Output,
    runtime_vars: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, serde_json::Error> {
    let mut data = serde_json::to_value(output)?;
    if let Some(object) = data.as_object_mut() {
        object.insert(
            "runtime".to_string(),
            serde_json::Value::Object(runtime_vars.clone()),
        );
    }
    Ok(data)
}

async fn run_exec_step(
    step: &RunStep,
    data: &serde_json::Value,
    action_index: usize,
    action_name: &str,
    runtime_budget: InvocationRuntimeBudget,
) -> Result<Option<(String, String)>, String> {
    let program = step.program.as_deref().ok_or_else(|| {
        format!(
            "Action '{}' exec step is missing required `program`.",
            action_name
        )
    })?;

    let resolved_args = resolve_run_args(&step.args, data, action_name)
        .map_err(|error| format!("Action '{}': {error}", action_name))?;

    let remaining = remaining_runtime_duration(
        runtime_budget,
        &format!("before starting command '{}'", program),
    )
    .map_err(|context| {
        action_runtime_timeout_message(action_name, runtime_budget, context.as_str())
    })?;

    if let Some(output_variable) = step.output_variable.as_deref() {
        let child = tokio::process::Command::new(program)
            .args(&resolved_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("{action_name}: failed to execute command: {error}."))?;

        match tokio::time::timeout(remaining, child.wait_with_output()).await {
            Ok(Ok(output)) if output.status.success() => {
                emit_exec_output_lines(
                    action_index,
                    action_name,
                    &output.stdout,
                    &output.stderr,
                    false,
                );
                let captured_output = String::from_utf8_lossy(&output.stdout)
                    .trim_end_matches(['\r', '\n'])
                    .to_string();
                print_action_line(
                    action_index,
                    action_name,
                    format!("stored exec output in variable '{}'.", output_variable).as_str(),
                );
                Ok(Some((output_variable.to_string(), captured_output)))
            }
            Ok(Ok(output)) => {
                emit_exec_output_lines(
                    action_index,
                    action_name,
                    &output.stdout,
                    &output.stderr,
                    true,
                );
                Err(format!(
                    "Action '{}' exec step command '{}' exited with status {}.",
                    action_name, program, output.status
                ))
            }
            Ok(Err(error)) => Err(format!(
                "Action '{}' exec step failed while waiting for command '{}': {}",
                action_name, program, error
            )),
            Err(_) => Err(action_runtime_timeout_message(
                action_name,
                runtime_budget,
                &format!("while waiting for command '{}'", program),
            )),
        }
    } else {
        let child = tokio::process::Command::new(program)
            .args(&resolved_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("{action_name}: failed to execute command: {error}."))?;

        match tokio::time::timeout(remaining, child.wait_with_output()).await {
            Ok(Ok(output)) if output.status.success() => {
                emit_exec_output_lines(
                    action_index,
                    action_name,
                    &output.stdout,
                    &output.stderr,
                    true,
                );
                Ok(None)
            }
            Ok(Ok(output)) => {
                emit_exec_output_lines(
                    action_index,
                    action_name,
                    &output.stdout,
                    &output.stderr,
                    true,
                );
                Err(format!(
                    "Action '{}' exec step command '{}' exited with status {}.",
                    action_name, program, output.status
                ))
            }
            Ok(Err(error)) => Err(format!(
                "Action '{}' exec step failed while waiting for command '{}': {}",
                action_name, program, error
            )),
            Err(_) => {
                Err(action_runtime_timeout_message(
                    action_name,
                    runtime_budget,
                    &format!("while waiting for command '{}'", program),
                ))
            }
        }
    }
}

async fn run_tool_step(
    step: &RunStep,
    data: &serde_json::Value,
    action_index: usize,
    action_name: &str,
    step_index: usize,
    provider_context: &ActionProviderContext,
    action_execution_override: Option<ActionExecutionMode>,
    max_agent_depth: u32,
    runtime_budget: InvocationRuntimeBudget,
) -> Result<Option<(String, String)>, String> {
    let tool_name = step.tool_name.as_deref().ok_or_else(|| {
        format!(
            "Action '{}' tool step is missing required `name`.",
            action_name
        )
    })?;
    let mut usage_tool_guard =
        provider_context
            .usage_log
            .as_ref()
            .map(|usage_log| usage_log.start_tool_run(usage_log::UsageTool {
                name: tool_name.to_string(),
                action: action_name.to_string(),
                step_index: Some(step_index),
            }));
    let resolver = provider_context.tool_resolver.as_ref().ok_or_else(|| {
        format!(
            "Action '{}' tool step '{}' cannot resolve tools because no tool resolver is available.",
            action_name, tool_name
        )
    })?;
    let contract = resolver.resolve_contract(tool_name)?;
    let params = resolve_tool_invoke_params(step, data, action_name, &contract.describe)?;
    let current_depth = current_agent_action_depth();
    let usage_log_bridge_context = provider_context
        .usage_log
        .as_ref()
        .map(|usage_log| usage_log.tool_bridge_context(tool_name, action_name, Some(step_index)));
    let artifact_root = if provider_context.project_data.is_some() {
        Some(std::env::current_dir().map_err(|error| error.to_string())?)
    } else {
        None
    };
    let request = serde_json::json!({
        "protocol_version": 1,
        "params": params,
        "runtime_context": {
            "agent_bridge": {
                "artifact_root": artifact_root,
                "current_depth": current_depth,
                "max_depth": max_agent_depth,
                "runtime_budget": {
                    "max_runtime_secs": runtime_budget.max_runtime_secs,
                    "started_at_ms": runtime_budget.started_at_ms,
                    "deadline_ms": runtime_budget.deadline_ms,
                },
                "profile_name": provider_context.profile_name,
                "action_execution": action_execution_override.map(|mode| match mode {
                    ActionExecutionMode::Sequential => "sequential",
                    ActionExecutionMode::Parallel => "parallel",
                }),
                "usage_log": usage_log_bridge_context,
            }
        },
    });
    let request_bytes = serde_json::to_vec(&request).map_err(|error| {
        format!(
            "Action '{}' tool step '{}' could not serialize invoke request: {}",
            action_name, tool_name, error
        )
    })?;
    let remaining = remaining_runtime_duration(
        runtime_budget,
        &format!("before starting tool '{}'", tool_name),
    )
    .map_err(|context| {
        action_runtime_timeout_message(action_name, runtime_budget, context.as_str())
    })?;

    let binary_path = if provider_context.project_data.is_some() {
        std::fs::canonicalize(&contract.resolved.binary_path).map_err(|error| {
            format!(
                "Failed to resolve tool executable '{}': {error}",
                contract.resolved.binary_path.display()
            )
        })?
    } else {
        contract.resolved.binary_path.clone()
    };
    let mut command = tokio::process::Command::new(&binary_path);
    command.arg("invoke");
    if let Some(root) = provider_context.project_data.as_ref() {
        command.current_dir(root.ensure_directory()?);
    }
    let child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!(
                "Action '{}' failed to start tool '{}': {}",
                action_name, tool_name, error
            )
        })?;
    let mut child = child;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            format!(
                "Action '{}' failed to open stdin for tool '{}'.",
                action_name, tool_name
            )
        })?;
        stdin.write_all(&request_bytes).await.map_err(|error| {
            format!(
                "Action '{}' failed to write invoke request for tool '{}': {}",
                action_name, tool_name, error
            )
        })?;
    }

    let output = match tokio::time::timeout(remaining, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err(format!(
                "Action '{}' failed while waiting for tool '{}': {}",
                action_name, tool_name, error
            ));
        }
        Err(_) => {
            return Err(action_runtime_timeout_message(
                action_name,
                runtime_budget,
                &format!("while waiting for tool '{}'", tool_name),
            ));
        }
    };

    emit_action_output_bytes(action_index, action_name, &output.stderr);
    if !output.status.success() {
        return Err(format!(
            "Action '{}' tool '{}' exited with status {}.",
            action_name, tool_name, output.status
        ));
    }

    let result = validate_tool_invoke_response(&contract.resolved, &output.stdout)?;
    if let Some(output_variable) = step.output_variable.as_deref() {
        let value = result.ok_or_else(|| {
            format!(
                "Action '{}' tool '{}' completed successfully but returned null result for output variable '{}'.",
                action_name, tool_name, output_variable
            )
        })?;
        print_action_line(
            action_index,
            action_name,
            format!("stored tool result in variable '{}'.", output_variable).as_str(),
        );
        if let Some(guard) = usage_tool_guard.as_mut() {
            guard.finish_success();
        }
        Ok(Some((output_variable.to_string(), value)))
    } else {
        if let Some(result) = result {
            print_action_line(
                action_index,
                action_name,
                format!("tool '{}' returned '{}'.", tool_name, result).as_str(),
            );
        }
        if let Some(guard) = usage_tool_guard.as_mut() {
            guard.finish_success();
        }
        Ok(None)
    }
}

fn emit_exec_output_lines(
    action_index: usize,
    action_name: &str,
    stdout: &[u8],
    stderr: &[u8],
    include_stdout: bool,
) {
    if include_stdout {
        emit_action_output_bytes(action_index, action_name, stdout);
    }
    emit_action_output_bytes(action_index, action_name, stderr);
}

fn emit_action_output_bytes(action_index: usize, action_name: &str, bytes: &[u8]) {
    let rendered = String::from_utf8_lossy(bytes);
    for line in split_action_output_lines(rendered.as_ref()) {
        print_action_line(action_index, action_name, line.as_str());
    }
}

fn insert_action_output_variable(
    data: &mut serde_json::Value,
    name: &str,
    value: String,
    action_name: &str,
) -> Result<(), String> {
    insert_action_string_variable(data, name, value, action_name)
}

fn insert_action_string_variable(
    data: &mut serde_json::Value,
    name: &str,
    value: String,
    action_name: &str,
) -> Result<(), String> {
    let Some(object) = data.as_object_mut() else {
        return Err(format!(
            "Action '{}' could not store captured variable '{}' because the action data context is not an object.",
            action_name, name
        ));
    };

    object.insert(name.to_string(), serde_json::Value::String(value));
    Ok(())
}

fn insert_step_status_variable(
    data: &mut serde_json::Value,
    step: &RunStep,
    status: &str,
    action_name: &str,
) -> Result<(), String> {
    let Some(name) = step.status_variable.as_deref() else {
        return Ok(());
    };

    insert_action_string_variable(data, name, status.to_string(), action_name)
}

fn insert_step_error_variable(
    data: &mut serde_json::Value,
    step: &RunStep,
    error: &str,
    action_name: &str,
) -> Result<(), String> {
    let Some(name) = step.error_variable.as_deref() else {
        return Ok(());
    };

    insert_action_string_variable(data, name, error.to_string(), action_name)
}

fn step_failure_mode(step: &RunStep) -> FailureMode {
    step.failure_mode.clone().unwrap_or(FailureMode::Stop)
}

fn should_run_step(
    step: &RunStep,
    data: &serde_json::Value,
    action_name: &str,
) -> Result<bool, String> {
    let Some(condition) = step.when.as_ref() else {
        return Ok(true);
    };

    apply(condition, data)
        .map(|result| result.as_bool() == Some(true))
        .map_err(|error| {
            format!(
                "Action '{}' failed to evaluate step `when` for kind '{}': {}",
                action_name, step.kind, error
            )
        })
}

async fn run_email_me_step(
    step: &RunStep,
    data: &serde_json::Value,
    action_index: usize,
    action_name: &str,
    secret_store_mode: Option<SecretStoreMode>,
    runtime_budget: InvocationRuntimeBudget,
    single_step_action: bool,
) -> Result<StepExecutionOutcome, String> {
    let subject_parts = step.subject.as_deref().ok_or_else(|| {
        format!(
            "Action '{}' email_me step is missing required `subject`.",
            action_name
        )
    })?;
    let text_parts = step.text.as_deref().ok_or_else(|| {
        format!(
            "Action '{}' email_me step is missing required `text`.",
            action_name
        )
    })?;

    let subject = resolve_string_parts(subject_parts, data, action_name, "subject")?;
    let text = resolve_string_parts(text_parts, data, action_name, "text")?;

    let remaining =
        remaining_runtime_duration(runtime_budget, "before sending email").map_err(|context| {
            action_runtime_timeout_message(action_name, runtime_budget, context.as_str())
        })?;

    let response = match tokio::time::timeout(
        remaining,
        run_email_me_action(subject.as_str(), text.as_str(), secret_store_mode),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(format!("Action '{}': {error}", action_name)),
        Err(_) => {
            return Err(action_runtime_timeout_message(
                action_name,
                runtime_budget,
                "while sending email",
            ));
        }
    };

    if single_step_action {
        print_action_success(action_index, action_name, "email sent");
    }
    for line in render_account_response_lines(&response) {
        print_action_line(action_index, action_name, line.as_str());
    }
    Ok(if single_step_action {
        StepExecutionOutcome::SuccessAlreadyPrinted
    } else {
        StepExecutionOutcome::Completed
    })
}

async fn run_generate_image_step(
    step: &RunStep,
    data: &serde_json::Value,
    named_inputs: &BTreeMap<String, Input>,
    action_index: usize,
    action_name: &str,
    step_index: usize,
    provider_context: &ActionProviderContext,
    runtime_budget: InvocationRuntimeBudget,
) -> Result<StepExecutionOutcome, String> {
    let step_profile_context =
        resolve_generate_image_step_profile_context(
            step.profile.as_ref(),
            data,
            action_name,
            provider_context.inference_timeout_in_sec,
        )
        .await?;
    let effective_provider_context = step_profile_context.as_ref().unwrap_or(provider_context);

    let model = resolve_generate_image_model(
        step.model.as_ref(),
        data,
        action_name,
        step_profile_context.as_ref(),
        provider_context,
    )?;
    print_action_using_line_if_changed(
        action_index,
        action_name,
        effective_provider_context.using_line_with_model(model.as_str()).as_str(),
    );

    if effective_provider_context
        .url
        .contains("chatgpt.com/backend-api/codex")
        && model.starts_with("gpt-image")
    {
        return Err(format!(
            "Action '{}' generate_image step uses OpenAI account transport, so `model` must be a tool-capable mainline model such as `gpt-5.2`, not '{}'.",
            action_name, model
        ));
    }
    let prompt_parts = step.prompt.as_deref().ok_or_else(|| {
        format!(
            "Action '{}' generate_image step is missing required `prompt`.",
            action_name
        )
    })?;
    let path_parts = step.path.as_deref().ok_or_else(|| {
        format!(
            "Action '{}' generate_image step is missing required `path`.",
            action_name
        )
    })?;

    let prompt = resolve_string_parts(prompt_parts, data, action_name, "prompt")
        .map_err(|error| format!("Action '{}': {error}", action_name))?;
    let output_path = resolve_string_parts(path_parts, data, action_name, "path")
        .map_err(|error| format!("Action '{}': {error}", action_name))?;
    let output_format = generated_image_output_format(output_path.as_str(), action_name)?;
    if !effective_provider_context
        .provider
        .capabilities()
        .supports_generate_image
    {
        return Err(format!(
            "Action '{}' generate_image is not supported by the {} adapter. Select an OpenAI or Ollama step profile.",
            action_name,
            effective_provider_context.provider.display_name()
        ));
    }
    validate_generate_image_output_format_for_provider(
        effective_provider_context.provider,
        output_format,
        action_name,
    )?;
    validate_generate_image_reference_support_for_provider(
        effective_provider_context.provider,
        step.reference_images.as_deref(),
        action_name,
    )?;
    let reference_images = resolve_generate_image_reference_images_with_data(
        step.reference_images.as_deref(),
        data,
        action_name,
        named_inputs,
        provider_context.project_data.as_ref(),
    )?;

    let remaining = remaining_runtime_duration(
        runtime_budget,
        &format!("before starting image generation with model '{}'", model),
    )
    .map_err(|context| {
        action_runtime_timeout_message(
            action_name,
            runtime_budget,
            context.as_str(),
        )
    })?;

    let provider_started_at = Instant::now();
    let image_response = match tokio::time::timeout(
        remaining,
        async {
            match effective_provider_context.provider {
                ProviderKind::Anthropic => Err(ProviderError::invalid_request(
                    ProviderKind::Anthropic,
                    "Anthropic image generation is not supported.",
                )),
                ProviderKind::Gemini => Err(ProviderError::invalid_request(
                    ProviderKind::Gemini,
                    "Gemini image generation is not supported.",
                )),
                ProviderKind::Mistral => Err(ProviderError::invalid_request(
                    ProviderKind::Mistral,
                    "Mistral image generation is not supported.",
                )),
                ProviderKind::OpenAi => {
                    crate::providers::send_openai_image_request(
                        &effective_provider_context.url,
                        &model,
                        prompt.as_str(),
                        effective_provider_context.inference_timeout_in_sec,
                        &effective_provider_context.token,
                        output_format,
                        &reference_images,
                    )
                    .await
                }
                ProviderKind::Ollama => {
                    crate::providers::send_ollama_image_request(
                        &effective_provider_context.url,
                        &model,
                        prompt.as_str(),
                        effective_provider_context.inference_timeout_in_sec,
                        &effective_provider_context.token,
                    )
                    .await
                }
                ProviderKind::Xai => Err(ProviderError::invalid_request(
                    ProviderKind::Xai,
                    "xAI image generation is not supported.",
                )),
            }
        },
    )
    .await
    {
        Ok(Ok(response)) => {
            if let Some(usage_log) = provider_context.usage_log.as_ref() {
                usage_log.record_provider_request(usage_log::UsageProviderRequest {
                    provider: effective_provider_context.provider,
                    profile_name: usage_provider_profile(effective_provider_context),
                    auth_mode: effective_provider_context.auth_mode.as_str(),
                    model: model.as_str(),
                    step: usage_log::UsageStep {
                        kind: "generate_image",
                        action: Some(action_name.to_string()),
                        step_index: Some(step_index),
                    },
                    usage: response.usage.as_ref(),
                    duration: provider_started_at.elapsed(),
                    status: usage_log::UsageStatus::Success,
                    error: None,
                });
            }
            response
        }
        Ok(Err(error)) => {
            if let Some(usage_log) = provider_context.usage_log.as_ref() {
                usage_log.record_provider_request(usage_log::UsageProviderRequest {
                    provider: effective_provider_context.provider,
                    profile_name: usage_provider_profile(effective_provider_context),
                    auth_mode: effective_provider_context.auth_mode.as_str(),
                    model: model.as_str(),
                    step: usage_log::UsageStep {
                        kind: "generate_image",
                        action: Some(action_name.to_string()),
                        step_index: Some(step_index),
                    },
                    usage: None,
                    duration: provider_started_at.elapsed(),
                    status: usage_log::UsageStatus::Failed,
                    error: Some(usage_provider_error(&error)),
                });
            }
            let mut lines =
                vec![format!("Action '{}' generate_image step failed.", action_name)];
            lines.extend(provider_error_messages(&error));
            return Err(lines.join("\n"));
        }
        Err(_) => {
            if let Some(usage_log) = provider_context.usage_log.as_ref() {
                usage_log.record_provider_request(usage_log::UsageProviderRequest {
                    provider: effective_provider_context.provider,
                    profile_name: usage_provider_profile(effective_provider_context),
                    auth_mode: effective_provider_context.auth_mode.as_str(),
                    model: model.as_str(),
                    step: usage_log::UsageStep {
                        kind: "generate_image",
                        action: Some(action_name.to_string()),
                        step_index: Some(step_index),
                    },
                    usage: None,
                    duration: provider_started_at.elapsed(),
                    status: usage_log::UsageStatus::Failed,
                    error: Some(usage_timeout_error()),
                });
            }
            return Err(action_runtime_timeout_message(
                action_name,
                runtime_budget,
                "while waiting for image generation",
            ));
        }
    };
    let image_bytes = image_response.bytes;

    let output_path_ref = Path::new(output_path.as_str());
    validate_generated_image_output_path(output_path_ref, action_name)?;
    let output_path_ref = match provider_context.project_data.as_ref() {
        Some(root) => root.resolve(output_path_ref)?,
        None => output_path_ref.to_path_buf(),
    };
    if let Some(parent) = output_path_ref.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "Action '{}' failed to create image output directory '{}': {}",
                    action_name,
                    parent.display(),
                    error
                )
            })?;
        }
    }

    std::fs::write(&output_path_ref, image_bytes).map_err(|error| {
        format!(
            "Action '{}' failed to write generated image '{}': {}",
            action_name,
            output_path_ref.display(),
            error
        )
    })?;

    print_action_line(
        action_index,
        action_name,
        format!("wrote generated image to '{}'.", output_path_ref.display()).as_str(),
    );
    Ok(StepExecutionOutcome::Completed)
}

fn resolve_step_profile_name(
    profile: Option<&RunArg>,
    data: &serde_json::Value,
    action_name: &str,
    step_kind: &str,
) -> Result<Option<String>, String> {
    let Some(profile) = profile else {
        return Ok(None);
    };

    let profile_name = match profile {
        RunArg::Literal(literal) => literal.clone(),
        RunArg::Variable(variable) => {
            let Some(value) = lookup_action_variable(data, variable) else {
                return Err(format!(
                    "Action '{}' {} `profile` references missing variable '{}'.",
                    action_name, step_kind, variable
                ));
            };

            match value {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Bool(_) => {
                    return Err(format!(
                        "Action '{}' {} `profile` must resolve to a string, found boolean for variable '{}'.",
                        action_name, step_kind, variable
                    ));
                }
                serde_json::Value::Number(_) => {
                    return Err(format!(
                        "Action '{}' {} `profile` must resolve to a string, found number for variable '{}'.",
                        action_name, step_kind, variable
                    ));
                }
                serde_json::Value::Array(_) => {
                    return Err(format!(
                        "Action '{}' {} `profile` must resolve to a string, found array for variable '{}'.",
                        action_name, step_kind, variable
                    ));
                }
                serde_json::Value::Object(_) => {
                    return Err(format!(
                        "Action '{}' {} `profile` must resolve to a string, found object for variable '{}'.",
                        action_name, step_kind, variable
                    ));
                }
                serde_json::Value::Null => {
                    return Err(format!(
                        "Action '{}' {} `profile` must resolve to a string, found null for variable '{}'.",
                        action_name, step_kind, variable
                    ));
                }
            }
        }
    };

    if profile_name.trim().is_empty() {
        return Err(format!(
            "Action '{}' {} `profile` must resolve to a non-empty string.",
            action_name, step_kind
        ));
    }

    Ok(Some(profile_name))
}

async fn resolve_generate_image_step_profile_context(
    profile: Option<&RunArg>,
    data: &serde_json::Value,
    action_name: &str,
    invocation_timeout_in_sec: u64,
) -> Result<Option<ActionProviderContext>, String> {
    let Some(profile_name) =
        resolve_step_profile_name(profile, data, action_name, "generate_image")?
    else {
        return Ok(None);
    };

    let config_file = config_path();
    let Some(config) = load_config() else {
        return Err(format!(
            "Action '{}' generate_image step references profile '{}', but no Cargo AI config was found at '{}'.",
            action_name,
            profile_name,
            config_file.display()
        ));
    };

    let Some(profile) = find_profile(&config, &profile_name) else {
        return Err(format!(
            "Action '{}' generate_image step references unknown profile '{}'.",
            action_name, profile_name
        ));
    };

    let provider = ProviderKind::from_server_value(profile.server.as_str()).ok_or_else(|| {
        format!(
            "Action '{}' generate_image step profile '{}' uses unsupported server '{}'.",
            action_name, profile.name, profile.server
        )
    })?;

    let mut server = String::new();
    let mut model = String::new();
    let mut profile_timeout_in_sec = 60;
    let mut profile_max_output_tokens = None;
    let mut url = String::new();
    let selected_profile = apply_profile(
        profile,
        &mut server,
        &mut model,
        &mut profile_timeout_in_sec,
        &mut profile_max_output_tokens,
        &mut url,
    );

    let resolved_token = match provider {
        ProviderKind::Anthropic | ProviderKind::Gemini | ProviderKind::Mistral | ProviderKind::Xai => ResolvedOpenAiToken {
            token: resolve_api_key_provider_token(provider, Some(&selected_profile))?,
            uses_account_session: false,
        },
        ProviderKind::OpenAi => {
            resolve_openai_token_for_request(Some(&selected_profile), Some(&config)).await?
        }
        ProviderKind::Ollama => match profile.auth_mode {
            ProfileAuthMode::None => ResolvedOpenAiToken {
                token: String::new(),
                uses_account_session: false,
            },
            ProfileAuthMode::ApiKey => ResolvedOpenAiToken {
                token: resolve_profile_api_token(&selected_profile)?,
                uses_account_session: false,
            },
            ProfileAuthMode::OpenaiAccount => {
                return Err(format!(
                    "Action '{}' generate_image step profile '{}' uses auth mode '{}', but server '{}' supports only '{}' or '{}'.",
                    action_name,
                    profile.name,
                    ProfileAuthMode::OpenaiAccount.as_str(),
                    profile.server,
                    ProfileAuthMode::None.as_str(),
                    ProfileAuthMode::ApiKey.as_str()
                ));
            }
        },
    };
    if url.is_empty() {
        if provider == ProviderKind::OpenAi && resolved_token.uses_account_session {
            url = OPENAI_ACCOUNT_RESPONSES_URL.to_string();
        } else {
            url = provider.default_url().to_string();
        }
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!(
            "Action '{}' generate_image step profile '{}' produced invalid URL '{}'. Use an absolute URL beginning with `http://` or `https://`.",
            action_name, profile.name, url
        ));
    }

    Ok(Some(ActionProviderContext {
        project_data: None,
        provider,
        profile_name: Some(profile.name.clone()),
        auth_mode: profile_auth_mode_display(profile.auth_mode).to_string(),
        model,
        url,
        token: resolved_token.token,
        inference_timeout_in_sec: invocation_timeout_in_sec,
        tool_resolver: None,
        usage_log: None,
    }))
}

fn resolve_generate_image_model(
    model: Option<&RunArg>,
    data: &serde_json::Value,
    action_name: &str,
    step_profile_context: Option<&ActionProviderContext>,
    provider_context: &ActionProviderContext,
) -> Result<String, String> {
    let Some(model) = model else {
        if let Some(step_profile_context) = step_profile_context {
            if !step_profile_context.model.trim().is_empty() {
                return Ok(step_profile_context.model.clone());
            }
        }
        if provider_context.model.trim().is_empty() {
            return Err(format!(
                "Action '{}' generate_image step omitted `model`, and no effective invocation model is configured. Set `generate_image.model`, pass `--model`, or configure a profile model.",
                action_name
            ));
        }
        return Ok(provider_context.model.clone());
    };

    match model {
        RunArg::Literal(literal) => {
            if literal.trim().is_empty() {
                return Err(format!(
                    "Action '{}' generate_image `model` must resolve to a non-empty string.",
                    action_name
                ));
            }
            Ok(literal.clone())
        }
        RunArg::Variable(variable) => {
            let Some(value) = lookup_action_variable(data, variable) else {
                return Err(format!(
                    "Action '{}' generate_image `model` references missing variable '{}'.",
                    action_name, variable
                ));
            };

            match value {
                serde_json::Value::String(text) if !text.trim().is_empty() => Ok(text.clone()),
                serde_json::Value::String(_) => Err(format!(
                    "Action '{}' generate_image `model` resolved to an empty string.",
                    action_name
                )),
                serde_json::Value::Bool(_) => Err(format!(
                    "Action '{}' generate_image `model` must resolve to a string, found boolean for variable '{}'.",
                    action_name, variable
                )),
                serde_json::Value::Number(_) => Err(format!(
                    "Action '{}' generate_image `model` must resolve to a string, found number for variable '{}'.",
                    action_name, variable
                )),
                serde_json::Value::Array(_) => Err(format!(
                    "Action '{}' generate_image `model` must resolve to a string, found array for variable '{}'.",
                    action_name, variable
                )),
                serde_json::Value::Object(_) => Err(format!(
                    "Action '{}' generate_image `model` must resolve to a string, found object for variable '{}'.",
                    action_name, variable
                )),
                serde_json::Value::Null => Err(format!(
                    "Action '{}' generate_image `model` must resolve to a string, found null for variable '{}'.",
                    action_name, variable
                )),
            }
        }
    }
}

fn profile_auth_mode_display(mode: ProfileAuthMode) -> &'static str {
    match mode {
        ProfileAuthMode::None => "none",
        ProfileAuthMode::ApiKey => "api_key",
        ProfileAuthMode::OpenaiAccount => "chatgpt_account",
    }
}

async fn run_agent_step(
    step: &RunStep,
    data: &serde_json::Value,
    named_inputs: &BTreeMap<String, Input>,
    action_index: usize,
    action_name: &str,
    step_index: usize,
    provider_context: &ActionProviderContext,
    action_execution_override: Option<ActionExecutionMode>,
    max_agent_depth: u32,
    runtime_budget: InvocationRuntimeBudget,
) -> Result<StepExecutionOutcome, String> {
    let artifact = step.agent.as_deref().ok_or_else(|| {
        format!(
            "Action '{}' agent step is missing required `artifact`.",
            action_name
        )
    })?;

    let current_depth = current_agent_action_depth();
    validate_agent_action_depth(current_depth, max_agent_depth, action_name)?;

    let invocation = resolve_child_artifact_invocation(artifact, action_name)?;
    let mut command = child_artifact_command(&invocation, artifact);
    if artifact.split_once("::").is_some() {
        command.current_dir(package_child_project_root(artifact, action_name)?);
    }
    if let Some(action_execution_override) = action_execution_override {
        command.arg("--action-execution");
        command.arg(match action_execution_override {
            ActionExecutionMode::Sequential => "sequential",
            ActionExecutionMode::Parallel => "parallel",
        });
    }
    if step.ignore_tools {
        command.arg("--ignore-tools");
    }
    if let Some(usage_log_path) = step.usage_log.as_deref() {
        let resolved_usage_log_path = resolve_child_usage_log_path_with_data(
            usage_log_path,
            action_name,
            provider_context.project_data.as_ref(),
        )?;
        ensure_child_usage_log_parent_exists(resolved_usage_log_path.as_path())?;
        command.arg("--usage-log");
        command.arg(resolved_usage_log_path.as_os_str());
    }
    let inherited_profile_name = if artifact_is_json_definition(artifact) {
        provider_context.profile_name.clone()
    } else {
        None
    };
    if let Some(profile_name) = resolve_step_profile_name(step.profile.as_ref(), data, action_name, "agent")?
        .or(inherited_profile_name)
    {
        let config_file = config_path();
        let Some(config) = load_config() else {
            return Err(format!(
                "Action '{}' agent step references profile '{}', but no Cargo AI config was found at '{}'.",
                action_name,
                profile_name,
                config_file.display()
            ));
        };
        if find_profile(&config, &profile_name).is_none() {
            return Err(format!(
                "Action '{}' agent step references unknown profile '{}'.",
                action_name, profile_name
            ));
        }
        command.arg("--profile");
        command.arg(profile_name);
    }
    let (child_args, resolution_notes) = child_input_args_with_data(
        step.run_vars.as_deref(),
        step.input_overrides.as_deref(),
        step.input_mode,
        step.inputs.as_deref(),
        data,
        action_name,
        named_inputs,
        provider_context.project_data.as_ref(),
    )?;
    for note in resolution_notes {
        print_action_line(action_index, action_name, note.as_str());
    }
    for argument in child_args {
        command.arg(argument);
    }
    command.env(AGENT_ACTION_DEPTH_ENV, (current_depth + 1).to_string());
    command.env(AGENT_ACTION_MAX_DEPTH_ENV, max_agent_depth.to_string());
    command.env(
        AGENT_ACTION_MAX_RUNTIME_SECS_ENV,
        runtime_budget.max_runtime_secs.to_string(),
    );
    command.env(
        AGENT_ACTION_RUNTIME_STARTED_AT_MS_ENV,
        runtime_budget.started_at_ms.to_string(),
    );
    command.env(
        AGENT_ACTION_RUNTIME_DEADLINE_MS_ENV,
        runtime_budget.deadline_ms.to_string(),
    );
    if let Some(usage_log) = provider_context.usage_log.as_ref() {
        for (key, value) in usage_log.direct_child_env(usage_log::UsageLaunchedBy {
            kind: "agent_step",
            action: Some(action_name.to_string()),
            tool: None,
            step_index: Some(step_index),
        }) {
            command.env(key, value);
        }
    }
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let remaining = remaining_runtime_duration(
        runtime_budget,
        &format!("before starting child agent '{}'", artifact),
    )
    .map_err(|context| {
        action_runtime_timeout_message(
            action_name,
            runtime_budget,
            context.as_str(),
        )
    })?;

    let child = command.spawn().map_err(|error| {
        format!(
            "Action '{}' failed to start child agent '{}': {}",
            action_name, artifact, error
        )
    })?;
    let mut child = child;
    let child_output = current_action_output();
    let child_action_name = action_name.to_string();
    let child_using_forwarder = child.stdout.take().map(|stdout| {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.starts_with("using: ") {
                    emit_using_line_with_output(
                        child_output.as_ref(),
                        action_index,
                        child_action_name.as_str(),
                        trimmed,
                    );
                }
            }
        })
    });
    let child_stderr_reader = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let mut stderr_lines = Vec::new();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    stderr_lines.push(trimmed.to_string());
                }
            }
            stderr_lines
        })
    });
    print_action_line(
        action_index,
        action_name,
        format!("child: started {}", artifact).as_str(),
    );

    let result = match tokio::time::timeout(remaining, child.wait()).await {
        Ok(Ok(status)) if status.success() => {
            if let Some(task) = child_using_forwarder {
                let _ = task.await;
            }
            if let Some(task) = child_stderr_reader {
                let _ = task.await;
            }
            print_action_line(action_index, action_name, "child: completed successfully");
            Ok(StepExecutionOutcome::Completed)
        }
        Ok(Ok(status)) => {
            if let Some(task) = child_using_forwarder {
                let _ = task.await;
            }
            let child_stderr = match child_stderr_reader {
                Some(task) => task.await.ok().unwrap_or_default(),
                None => Vec::new(),
            };
            print_action_line(
                action_index,
                action_name,
                format!("child: exited with status {}", status).as_str(),
            );
            let stderr_suffix = if child_stderr.is_empty() {
                String::new()
            } else {
                format!(" Child error: {}", child_stderr.join(" | "))
            };
            Err(format!(
                "Action '{}' child agent '{}' exited with status {} at depth {}.{}",
                action_name,
                artifact,
                status,
                current_depth + 1,
                stderr_suffix
            ))
        }
        Ok(Err(error)) => Err(format!(
            "Action '{}' failed while waiting for child agent '{}' at depth {}: {}",
            action_name,
            artifact,
            current_depth + 1,
            error
        )),
        Err(_) => {
            let _ = child.kill().await;
            if let Some(task) = child_using_forwarder {
                let _ = task.await;
            }
            if let Some(task) = child_stderr_reader {
                let _ = task.await;
            }
            print_action_line(
                action_index,
                action_name,
                format!("child: timed out {}", artifact).as_str(),
            );
            Err(action_runtime_timeout_message(
                action_name,
                runtime_budget,
                &format!("while waiting for child agent '{}' at depth {}", artifact, current_depth + 1),
            ))
        }
    };
    result
}

fn resolve_child_artifact_invocation(
    artifact: &str,
    action_name: &str,
) -> Result<ChildArtifactInvocation, String> {
    validate_agent_step_target(artifact, action_name)?;
    if artifact.split_once("::").is_some() {
        package_child_project_root(artifact, action_name)?;
        let cargo_ai_exists = command_exists_on_path("cargo-ai");
        if command_exists_on_path("cargo") && cargo_ai_exists {
            return Ok(ChildArtifactInvocation::CargoSubcommand);
        }
        if cargo_ai_exists {
            return Ok(ChildArtifactInvocation::StandaloneCargoAi);
        }
        return Err(format!(
            "Action '{}' package child '{}' requires Cargo AI to be available as `cargo ai` or `cargo-ai` on PATH so the declaring project's package dependency can be validated.",
            action_name, artifact
        ));
    }
    let artifact_path = Path::new(artifact);
    if !artifact_path.exists() {
        return Err(format!(
            "Action '{}' agent step artifact '{}' was not found relative to the current working directory.",
            action_name, artifact
        ));
    }

    if !artifact_is_json_definition(artifact) {
        return Ok(ChildArtifactInvocation::DirectExecutable(
            artifact_path.to_path_buf(),
        ));
    }

    let cargo_ai_exists = command_exists_on_path("cargo-ai");
    if command_exists_on_path("cargo") && cargo_ai_exists {
        return Ok(ChildArtifactInvocation::CargoSubcommand);
    }
    if cargo_ai_exists {
        return Ok(ChildArtifactInvocation::StandaloneCargoAi);
    }

    Err(format!(
        "Action '{}' agent step JSON artifact '{}' requires Cargo AI to be available as `cargo ai` or `cargo-ai` on PATH.",
        action_name, artifact
    ))
}

fn package_child_project_root(artifact: &str, action_name: &str) -> Result<PathBuf, String> {
    let current_dir = std::env::current_dir().map_err(|error| {
        format!(
            "Action '{}' package child '{}' could not inspect the current directory: {}",
            action_name, artifact, error
        )
    })?;
    package_child_project_root_from(&current_dir, artifact, action_name)
}

fn package_child_project_root_from(
    current_dir: &Path,
    artifact: &str,
    action_name: &str,
) -> Result<PathBuf, String> {
    let project_root = maybe_find_project_root(current_dir)?.ok_or_else(|| {
        format!(
            "Action '{}' package child '{}' requires execution inside a Cargo AI project. Hatched binaries fail closed when that declaring project context is unavailable.",
            action_name, artifact
        )
    })?;
    Ok(project_root)
}

fn child_artifact_command(
    invocation: &ChildArtifactInvocation,
    artifact: &str,
) -> tokio::process::Command {
    match invocation {
        ChildArtifactInvocation::DirectExecutable(path) => tokio::process::Command::new(path),
        ChildArtifactInvocation::CargoSubcommand => {
            let mut command = tokio::process::Command::new("cargo");
            command.arg("ai");
            command.arg("run");
            command.arg(artifact);
            command
        }
        ChildArtifactInvocation::StandaloneCargoAi => {
            let mut command = tokio::process::Command::new("cargo-ai");
            command.arg("run");
            command.arg(artifact);
            command
        }
    }
}

#[cfg(test)]
fn resolve_child_usage_log_path(raw_path: &str, action_name: &str) -> Result<PathBuf, String> {
    resolve_child_usage_log_path_with_data(raw_path, action_name, None)
}

fn resolve_child_usage_log_path_with_data(
    raw_path: &str,
    action_name: &str,
    project_data: Option<&runtime_data::DataRoot>,
) -> Result<PathBuf, String> {
    let path = Path::new(raw_path);
    validate_child_usage_log_path(path, action_name)?;
    match project_data {
        Some(root) => root.resolve(path),
        None => Ok(path.to_path_buf()),
    }
}

fn validate_child_usage_log_path(path: &Path, action_name: &str) -> Result<(), String> {
    let raw_path = path.to_string_lossy();
    if raw_path.trim().is_empty() {
        return Err(format!(
            "Action '{}' agent `usage_log` must be a non-empty relative path.",
            action_name
        ));
    }
    if path.is_absolute() {
        return Err(format!(
            "Action '{}' agent `usage_log` must be a relative path.",
            action_name
        ));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "Action '{}' agent `usage_log` must not use parent traversal (`..`).",
            action_name
        ));
    }
    Ok(())
}

fn ensure_child_usage_log_parent_exists(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Failed to create child usage log directory '{}': {}",
            parent.display(),
            error
        )
    })
}

fn action_completion_summary(outcomes: &[StepExecutionOutcome]) -> Option<&'static str> {
    if outcomes.is_empty()
        || outcomes.iter().any(|outcome| {
            matches!(
                outcome,
                StepExecutionOutcome::SoftFailureLogged | StepExecutionOutcome::SuccessAlreadyPrinted
            )
        })
    {
        None
    } else {
        Some("completed")
    }
}

fn format_top_level_action_failures(failures: &[String]) -> String {
    if failures.len() == 1 {
        failures
            .first()
            .expect("single failure should exist")
            .clone()
    } else {
        format!(
            "{} top-level actions failed:\n{}",
            failures.len(),
            failures.join("\n")
        )
    }
}

fn format_abort_summary(abort: &InvocationAbortRecord) -> String {
    format!(
        "Run aborted by {}: {}",
        action_lane_prefix(abort.action_index, abort.action_name.as_str()),
        abort.error
    )
}

fn run_completion_message_for_depth(depth: u32, elapsed: Duration) -> Option<String> {
    if depth == 0 {
        Some(format!("Run complete · {} total", format_elapsed_duration(elapsed)))
    } else {
        None
    }
}

fn root_run_completion_message(elapsed: Duration) -> Option<String> {
    run_completion_message_for_depth(current_agent_action_depth(), elapsed)
}

fn root_run_failure_message(elapsed: Duration) -> String {
    format!("x Run failed · {} total", format_elapsed_duration(elapsed))
}

fn root_run_abort_message(elapsed: Duration) -> String {
    format!("! Run aborted · {} total", format_elapsed_duration(elapsed))
}

fn current_action_output() -> Option<ActionOutput> {
    ACTION_OUTPUT.try_with(Clone::clone).ok()
}

fn finish_action_output() {
    if let Some(output) = current_action_output() {
        output.finish();
    }
}

fn note_action_step_started(
    action_index: usize,
    action_name: &str,
    step_kind: &str,
    step_number: usize,
    step_count: usize,
) {
    if let Some(output) = current_action_output() {
        output.action_step_started(action_index, action_name, step_kind, step_number, step_count);
    }
}

fn note_action_failure(action_index: usize, action_name: &str, error: &str) {
    if let Some(output) = current_action_output() {
        output.action_failed(action_index, action_name, error);
    }
}

fn note_action_abort(action_index: usize, action_name: &str, error: &str) {
    if let Some(output) = current_action_output() {
        output.action_aborted(action_index, action_name, error);
    }
}

fn note_action_stopped_by_abort(action_index: usize, action_name: &str) {
    if let Some(output) = current_action_output() {
        output.action_stopped_by_abort(action_index, action_name);
    }
}

fn action_execution_header(action_execution: ActionExecutionMode) -> &'static str {
    match action_execution {
        ActionExecutionMode::Sequential => "run: sequential",
        ActionExecutionMode::Parallel => "run: parallel",
    }
}

fn print_action_execution_header(action_execution: ActionExecutionMode) {
    if let Some(output) = current_action_output() {
        output.print_execution_header();
    } else {
        println!("{}", action_execution_header(action_execution));
    }
}

fn print_action_using_line_if_changed(action_index: usize, action_name: &str, using_line: &str) {
    if let Some(output) = current_action_output() {
        output.action_using_line_if_changed(action_index, action_name, using_line);
    } else {
        println!("{}", format_action_line(action_index, action_name, using_line));
    }
}

fn action_lane_prefix(action_index: usize, action_name: &str) -> String {
    format!("[Action {}: {}]", action_index + 1, action_name)
}

fn format_action_line(action_index: usize, action_name: &str, message: &str) -> String {
    format!("{} {}", action_lane_prefix(action_index, action_name), message)
}

fn format_action_failure(action_index: usize, action_name: &str, error: &str) -> String {
    format_action_line(action_index, action_name, format!("failed: {}", error).as_str())
}

fn print_action_line(action_index: usize, action_name: &str, message: &str) {
    if let Some(output) = current_action_output() {
        output.action_line(action_index, action_name, message);
    } else {
        println!("{}", format_action_line(action_index, action_name, message));
    }
}

fn emit_using_line_with_output(
    output: Option<&ActionOutput>,
    action_index: usize,
    action_name: &str,
    using_line: &str,
) {
    if let Some(output) = output {
        output.action_using_line_if_changed(action_index, action_name, using_line);
    } else {
        println!("{}", format_action_line(action_index, action_name, using_line));
    }
}

fn print_action_start(action_index: usize, action_name: &str) {
    if let Some(output) = current_action_output() {
        output.action_started(action_index, action_name);
    } else {
        print_action_line(action_index, action_name, "started");
    }
}

fn print_action_success(action_index: usize, action_name: &str, summary: &str) {
    if let Some(output) = current_action_output() {
        output.action_success(action_index, action_name, summary);
    } else {
        print_action_line(action_index, action_name, format!("{}.", summary).as_str());
    }
}

fn render_account_response_lines(response: &serde_json::Value) -> Vec<String> {
    if let Some(message) = format_backend_ui_message(response, true) {
        split_action_output_lines(message.as_str())
    } else {
        split_action_output_lines(pretty_backend_json(response).as_str())
    }
}

fn format_backend_error_message(response: &serde_json::Value) -> Option<String> {
    format_backend_ui_message(response, false)
}

fn format_backend_ui_message(
    response: &serde_json::Value,
    include_kind_prefix: bool,
) -> Option<String> {
    let ui = response.get("ui")?;
    if ui.get("schema").and_then(|value| value.as_str()) != Some("1.0") {
        return None;
    }

    let kind = ui.get("kind").and_then(|value| value.as_str()).unwrap_or("info");
    let title = ui
        .get("title")
        .and_then(|value| value.as_str())
        .unwrap_or("Status");
    let summary = ui
        .get("summary")
        .and_then(|value| value.as_str())
        .unwrap_or("Status response received.");

    let mut lines = Vec::new();
    if include_kind_prefix {
        let kind_prefix = match kind {
            "success" => "✅",
            "error" => "⚠️",
            "failure" => "❌",
            _ => "ℹ️",
        };
        lines.push(format!("{kind_prefix} {title}"));
    } else {
        lines.push(title.to_string());
    }
    lines.push(summary.to_string());

    if let Some(variant) = ui.get("variant").and_then(|value| value.as_str()) {
        if !variant.trim().is_empty() {
            lines.push(format!("Variant: {variant}"));
        }
    }

    if let Some(sections) = ui.get("sections").and_then(|value| value.as_array()) {
        for section in sections {
            append_backend_section_lines(section, &mut lines);
        }
    }

    if let Some(actions) = ui.get("actions").and_then(|value| value.as_array()) {
        let action_lines = actions
            .iter()
            .filter_map(|action| {
                let label = action.get("label").and_then(|value| value.as_str()).unwrap_or("");
                let command = action
                    .get("command")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");

                if label.is_empty() && command.is_empty() {
                    None
                } else if !label.is_empty() && !command.is_empty() {
                    Some(format!("- {}: {}", label, command))
                } else if !label.is_empty() {
                    Some(format!("- {}", label))
                } else {
                    Some(format!("- {}", command))
                }
            })
            .collect::<Vec<_>>();

        if !action_lines.is_empty() {
            lines.push(String::new());
            lines.push("Actions:".to_string());
            lines.extend(action_lines);
        }
    }

    if let Some(next_steps) = ui.get("next_steps").and_then(|value| value.as_array()) {
        let step_lines = next_steps
            .iter()
            .filter_map(|step| step.as_str())
            .map(str::trim)
            .filter(|step| !step.is_empty())
            .map(|step| format!("- {}", step))
            .collect::<Vec<_>>();

        if !step_lines.is_empty() {
            lines.push(String::new());
            lines.push("Next steps:".to_string());
            lines.extend(step_lines);
        }
    }

    Some(lines.join("\n"))
}

fn append_backend_section_lines(section: &serde_json::Value, lines: &mut Vec<String>) {
    let section_type = section
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let title = section
        .get("title")
        .and_then(|value| value.as_str())
        .unwrap_or("");

    if !title.is_empty() {
        lines.push(String::new());
        lines.push(format!("{title}:"));
    }

    match section_type {
        "kv" => {
            if let Some(items) = section.get("items").and_then(|value| value.as_array()) {
                for item in items {
                    let label = item.get("label").and_then(|value| value.as_str()).unwrap_or("");
                    let value = item
                        .get("value")
                        .map(backend_ui_value_to_string)
                        .unwrap_or_default();

                    if label.is_empty() && value.is_empty() {
                        continue;
                    }

                    if label.is_empty() {
                        lines.push(format!("- {}", value));
                    } else {
                        lines.push(format!("- {}: {}", label, value));
                    }
                }
            }
        }
        "list" => {
            if let Some(items) = section.get("items").and_then(|value| value.as_array()) {
                for item in items {
                    let value = backend_ui_value_to_string(item);
                    if !value.is_empty() {
                        lines.push(format!("- {}", value));
                    }
                }
            }
        }
        "notice" => {
            if let Some(message) = section.get("message").and_then(|value| value.as_str()) {
                if !message.trim().is_empty() {
                    lines.push(message.to_string());
                }
            }
        }
        "json" => {
            if let Some(data) = section.get("data") {
                match serde_json::to_string_pretty(data) {
                    Ok(pretty) => lines.extend(pretty.lines().map(str::to_string)),
                    Err(_) => lines.push(backend_ui_value_to_string(data)),
                }
            }
        }
        _ => {}
    }
}

fn backend_ui_value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(boolean) => boolean.to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::String(text) => text.to_string(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_default()
        }
    }
}

fn pretty_backend_json(response: &serde_json::Value) -> String {
    serde_json::to_string_pretty(response).unwrap_or_else(|_| format!("{response:?}"))
}

fn current_action_platform() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("macos")
    } else if cfg!(target_os = "linux") {
        Some("linux")
    } else if cfg!(target_os = "windows") {
        Some("windows")
    } else {
        None
    }
}

fn matching_run_steps<'a>(
    run_steps: &'a [RunStep],
    current_platform: Option<&str>,
) -> Vec<&'a RunStep> {
    run_steps
        .iter()
        .filter(|step| step_matches_platform(step.platforms.as_deref(), current_platform))
        .collect()
}

#[cfg(test)]
fn child_input_args(
    run_vars: Option<&[ActionRunVar]>,
    input_overrides: Option<&[ActionInputOverride]>,
    input_mode: Option<ActionInputMode>,
    inputs: Option<&[ActionInput]>,
    data: &serde_json::Value,
    action_name: &str,
    named_inputs: &BTreeMap<String, Input>,
) -> Result<(Vec<String>, Vec<String>), String> {
    child_input_args_with_data(
        run_vars,
        input_overrides,
        input_mode,
        inputs,
        data,
        action_name,
        named_inputs,
        None,
    )
}

fn child_input_args_with_data(
    run_vars: Option<&[ActionRunVar]>,
    input_overrides: Option<&[ActionInputOverride]>,
    input_mode: Option<ActionInputMode>,
    inputs: Option<&[ActionInput]>,
    data: &serde_json::Value,
    action_name: &str,
    named_inputs: &BTreeMap<String, Input>,
    project_data: Option<&runtime_data::DataRoot>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let mut args = Vec::new();
    let mut notes = Vec::new();

    if let Some(run_vars) = run_vars {
        for run_var in run_vars {
            let (resolved_value, resolution_note) =
                resolve_child_run_var_value(&run_var.value, data, action_name, &run_var.name)?;
            args.push("--run-var".to_string());
            args.push(format!("{}={}", run_var.name, resolved_value));
            if let Some(note) = resolution_note {
                notes.push(note);
            }
        }
    }

    if let Some(input_overrides) = input_overrides {
        for input_override in input_overrides {
            let (resolved_value, resolution_note) = resolve_child_input_override_value(
                &input_override.value,
                data,
                action_name,
                &input_override.name,
                named_inputs,
            )?;
            args.push("--input-override".to_string());
            args.push(format!("{}={}", input_override.name, resolved_value));
            if let Some(note) = resolution_note {
                notes.push(note);
            }
        }
    }

    if let Some(input_mode) = input_mode {
        if inputs.is_none() {
            return Err(format!(
                "Action '{}' child-agent `input_mode` requires `inputs`.",
                action_name
            ));
        }

        args.push("--input-mode".to_string());
        args.push(
            match input_mode {
                ActionInputMode::Replace => "replace",
                ActionInputMode::Append => "append",
                ActionInputMode::Prepend => "prepend",
            }
            .to_string(),
        );
    }

    if let Some(inputs) = inputs {
        for (index, input) in inputs.iter().enumerate() {
            match input {
                ActionInput::Text { text } => {
                    let resolved = resolve_string_parts(
                        text,
                        data,
                        action_name,
                        &format!("child-agent text input {}", index + 1),
                    )?;
                    args.push("--input-text".to_string());
                    args.push(resolved);
                    if child_input_uses_dynamic_parts(text) {
                        notes.push(format!(
                            "Action '{}' resolved dynamic child-agent text input {}.",
                            action_name,
                            index + 1
                        ));
                    }
                }
                ActionInput::Url { url } => {
                    let resolved = resolve_string_parts(
                        url,
                        data,
                        action_name,
                        &format!("child-agent url input {}", index + 1),
                    )?;
                    validate_child_input_url(&resolved, action_name, index + 1)?;
                    args.push("--input-url".to_string());
                    args.push(resolved.clone());
                    if child_input_uses_dynamic_parts(url) {
                        notes.push(format!(
                            "Action '{}' resolved dynamic child-agent url input {} -> {}.",
                            action_name,
                            index + 1,
                            resolved
                        ));
                    }
                }
                ActionInput::Image { path } => {
                    let resolved = resolve_string_parts(
                        path,
                        data,
                        action_name,
                        &format!("child-agent image path input {}", index + 1),
                    )?;
                    validate_child_input_path(&resolved, action_name, index + 1, "image")?;
                    let resolved =
                        match project_data.filter(|_| child_input_uses_dynamic_parts(path)) {
                            Some(root) => root
                                .resolve(Path::new(&resolved))?
                                .to_string_lossy()
                                .into_owned(),
                            None => resolved,
                        };
                    args.push("--input-image".to_string());
                    args.push(resolved.clone());
                    if child_input_uses_dynamic_parts(path) {
                        notes.push(format!(
                            "Action '{}' resolved dynamic child-agent image path input {} -> {}.",
                            action_name,
                            index + 1,
                            resolved
                        ));
                    }
                }
                ActionInput::File { path } => {
                    let resolved = resolve_string_parts(
                        path,
                        data,
                        action_name,
                        &format!("child-agent file path input {}", index + 1),
                    )?;
                    validate_child_input_path(&resolved, action_name, index + 1, "file")?;
                    let resolved =
                        match project_data.filter(|_| child_input_uses_dynamic_parts(path)) {
                            Some(root) => root
                                .resolve(Path::new(&resolved))?
                                .to_string_lossy()
                                .into_owned(),
                            None => resolved,
                        };
                    validate_child_file_extension(&resolved, action_name, index + 1)?;
                    args.push("--input-file".to_string());
                    args.push(resolved.clone());
                    if child_input_uses_dynamic_parts(path) {
                        notes.push(format!(
                            "Action '{}' resolved dynamic child-agent file path input {} -> {}.",
                            action_name,
                            index + 1,
                            resolved
                        ));
                    }
                }
                ActionInput::Named { input } => {
                    let forwarded = named_inputs.get(input).ok_or_else(|| {
                        format!(
                            "Action '{}' child-agent named input '{}' is not available for forwarding.",
                            action_name, input
                        )
                    })?;
                    let value = forwarded.value.as_deref().ok_or_else(|| {
                        format!(
                            "Action '{}' child-agent named input '{}' is required but unresolved for this invocation.",
                            action_name, input
                        )
                    })?;
                    let payload = Input {
                        name: Some(input.clone()),
                        kind: forwarded.kind,
                        value: Some(value.to_string()),
                    };
                    args.push("--forwarded-input".to_string());
                    args.push(
                        serde_json::to_string(&payload).map_err(|error| {
                            format!(
                                "Action '{}' failed to serialize forwarded named input '{}': {}",
                                action_name, input, error
                            )
                        })?,
                    );
                }
            }
        }
    }

    Ok((args, notes))
}

fn resolve_child_input_override_value(
    input: &ActionInputOverrideValue,
    data: &serde_json::Value,
    action_name: &str,
    override_name: &str,
    named_inputs: &BTreeMap<String, Input>,
) -> Result<(String, Option<String>), String> {
    match input {
        ActionInputOverrideValue::Literal(literal) => Ok((literal.clone(), None)),
        ActionInputOverrideValue::Variable(variable) => {
            let resolved = resolve_scalar_action_variable(
                data,
                variable,
                action_name,
                &format!("child-agent named input override '{}'", override_name),
            )?;
            Ok((
                resolved.clone(),
                Some(format!(
                    "Action '{}' resolved dynamic child-agent named override '{}'.",
                    action_name, override_name
                )),
            ))
        }
        ActionInputOverrideValue::NamedInput { input } => {
            let forwarded = named_inputs.get(input).ok_or_else(|| {
                format!(
                    "Action '{}' child-agent named input '{}' is not available for forwarding.",
                    action_name, input
                )
            })?;
            let value = forwarded.value.as_deref().ok_or_else(|| {
                format!(
                    "Action '{}' child-agent named input '{}' is required but unresolved for this invocation.",
                    action_name, input
                )
            })?;
            Ok((value.to_string(), None))
        }
    }
}

fn resolve_child_run_var_value(
    value: &ActionRunVarValue,
    data: &serde_json::Value,
    action_name: &str,
    run_var_name: &str,
) -> Result<(String, Option<String>), String> {
    match value {
        ActionRunVarValue::Literal(literal) => Ok((stringify_scalar_json_value(literal, action_name, &format!("child-agent runtime var '{}'", run_var_name))?, None)),
        ActionRunVarValue::Variable(variable) => {
            let resolved = resolve_scalar_action_variable(
                data,
                variable,
                action_name,
                &format!("child-agent runtime var '{}'", run_var_name),
            )?;
            Ok((
                resolved.clone(),
                Some(format!(
                    "Action '{}' resolved dynamic child-agent runtime var '{}'.",
                    action_name, run_var_name
                )),
            ))
        }
    }
}

fn resolve_scalar_action_variable(
    data: &serde_json::Value,
    variable: &str,
    action_name: &str,
    field_name: &str,
) -> Result<String, String> {
    let Some(value) = lookup_action_variable(data, variable) else {
        return Err(format!(
            "Action '{}' {} references missing variable '{}'.",
            action_name, field_name, variable
        ));
    };

    match value {
        serde_json::Value::String(text) => Ok(text.clone()),
        serde_json::Value::Bool(boolean) => Ok(boolean.to_string()),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        serde_json::Value::Array(_) => Err(format!(
            "Action '{}' {} references array-valued variable '{}', which is unsupported for scalar substitution.",
            action_name, field_name, variable
        )),
        serde_json::Value::Object(_) => Err(format!(
            "Action '{}' {} references object-valued variable '{}', which is unsupported for scalar substitution.",
            action_name, field_name, variable
        )),
        serde_json::Value::Null => Err(format!(
            "Action '{}' {} references null variable '{}', which is unsupported for scalar substitution.",
            action_name, field_name, variable
        )),
    }
}

fn stringify_scalar_json_value(
    value: &serde_json::Value,
    action_name: &str,
    field_name: &str,
) -> Result<String, String> {
    match value {
        serde_json::Value::String(text) => Ok(text.clone()),
        serde_json::Value::Bool(boolean) => Ok(boolean.to_string()),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        serde_json::Value::Array(_) => Err(format!(
            "Action '{}' {} cannot use an array literal here.",
            action_name, field_name
        )),
        serde_json::Value::Object(_) => Err(format!(
            "Action '{}' {} cannot use an object literal here.",
            action_name, field_name
        )),
        serde_json::Value::Null => Err(format!(
            "Action '{}' {} cannot use null here.",
            action_name, field_name
        )),
    }
}

fn child_input_uses_dynamic_parts(parts: &[RunArg]) -> bool {
    parts.iter().any(|part| matches!(part, RunArg::Variable(_)))
}

fn validate_generated_image_output_path(path: &Path, action_name: &str) -> Result<(), String> {
    let raw_path = path.to_string_lossy();
    if raw_path.trim().is_empty() {
        return Err(format!(
            "Action '{}' generate_image `path` must resolve to a non-empty relative path.",
            action_name
        ));
    }
    if path.is_absolute() {
        return Err(format!(
            "Action '{}' generate_image `path` must resolve to a relative path.",
            action_name
        ));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "Action '{}' generate_image `path` must not use parent traversal (`..`).",
            action_name
        ));
    }

    generated_image_output_format(raw_path.as_ref(), action_name).map(|_| ())
}

fn generated_image_output_format(raw_path: &str, action_name: &str) -> Result<&'static str, String> {
    let extension = Path::new(raw_path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());

    match extension.as_deref() {
        Some("png") => Ok("png"),
        Some("jpg") | Some("jpeg") => Ok("jpeg"),
        Some("webp") => Ok("webp"),
        _ => Err(format!(
            "Action '{}' generate_image `path` must use a supported extension: `.png`, `.jpg`, `.jpeg`, `.webp`.",
            action_name
        )),
    }
}

fn validate_generate_image_output_format_for_provider(
    provider: ProviderKind,
    output_format: &str,
    action_name: &str,
) -> Result<(), String> {
    if provider == ProviderKind::Ollama && output_format != "png" {
        return Err(format!(
            "Action '{}' generate_image step targeting Ollama currently requires a `.png` output path because the current Ollama compatibility slice only guarantees `b64_json` image payloads, not OpenAI-style output-format selection.",
            action_name
        ));
    }

    Ok(())
}

#[cfg(test)]
fn resolve_generate_image_reference_images(
    references: Option<&[GenerateImageReference]>,
    data: &serde_json::Value,
    action_name: &str,
    named_inputs: &BTreeMap<String, Input>,
) -> Result<Vec<providers::ImageReference>, String> {
    resolve_generate_image_reference_images_with_data(
        references,
        data,
        action_name,
        named_inputs,
        None,
    )
}

fn resolve_generate_image_reference_images_with_data(
    references: Option<&[GenerateImageReference]>,
    data: &serde_json::Value,
    action_name: &str,
    named_inputs: &BTreeMap<String, Input>,
    project_data: Option<&runtime_data::DataRoot>,
) -> Result<Vec<providers::ImageReference>, String> {
    let Some(references) = references else {
        return Ok(Vec::new());
    };

    let mut resolved = Vec::with_capacity(references.len());
    for (index, reference) in references.iter().enumerate() {
        let path = match reference {
            GenerateImageReference::Path { path } => {
                let resolved = resolve_string_parts(
                    path,
                    data,
                    action_name,
                    &format!("reference_images[{}].path", index),
                )
                .map_err(|error| format!("Action '{}': {error}", action_name))?;
                resolved
            }
            GenerateImageReference::Named { input } => {
                let named_input = named_inputs.get(input).ok_or_else(|| {
                    format!(
                        "Action '{}' generate_image reference image {} named input '{}' is not available.",
                        action_name,
                        index + 1,
                        input
                    )
                })?;
                if named_input.kind != InputKind::Image {
                    return Err(format!(
                        "Action '{}' generate_image reference image {} named input '{}' must have type `image`.",
                        action_name,
                        index + 1,
                        input
                    ));
                }
                named_input
                    .value
                    .as_deref()
                    .ok_or_else(|| {
                        format!(
                            "Action '{}' generate_image reference image {} named input '{}' is required but unresolved for this invocation.",
                            action_name,
                            index + 1,
                            input
                        )
                    })?
                    .to_string()
            }
        };

        validate_generate_image_reference_path(path.as_str(), action_name, index + 1)?;
        let path = match (reference, project_data) {
            (GenerateImageReference::Path { path: parts }, Some(root))
                if child_input_uses_dynamic_parts(parts) =>
            {
                root.resolve(Path::new(&path))?
                    .to_string_lossy()
                    .into_owned()
            }
            _ => path,
        };
        resolved.push(
            providers::load_image_reference(path.as_str()).map_err(|error| {
                format!(
                    "Action '{}' generate_image reference image {} could not be loaded: {}",
                    action_name,
                    index + 1,
                    error
                )
            })?,
        );
    }

    Ok(resolved)
}

fn validate_generate_image_reference_support_for_provider(
    provider: ProviderKind,
    reference_images: Option<&[GenerateImageReference]>,
    action_name: &str,
) -> Result<(), String> {
    if provider == ProviderKind::Ollama && reference_images.is_some_and(|images| !images.is_empty())
    {
        return Err(format!(
            "Action '{}' generate_image reference_images are not supported by provider 'ollama' for this profile. Remove reference_images or use an OpenAI image profile.",
            action_name
        ));
    }

    Ok(())
}

fn validate_generate_image_reference_path(
    path: &str,
    action_name: &str,
    input_index: usize,
) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err(format!(
            "Action '{}' generate_image reference image {} must resolve to a non-empty relative path.",
            action_name, input_index
        ));
    }

    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(format!(
            "Action '{}' generate_image reference image {} must stay at the current level or below; absolute paths are not allowed.",
            action_name, input_index
        ));
    }

    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "Action '{}' generate_image reference image {} must stay at the current level or below; parent traversal (`..`) is not allowed.",
            action_name, input_index
        ));
    }

    Ok(())
}

fn validate_child_input_url(url: &str, action_name: &str, input_index: usize) -> Result<(), String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(format!(
            "Action '{}' child-agent url input {} must resolve to an http(s) URL.",
            action_name, input_index
        ))
    }
}

fn validate_child_input_path(
    path: &str,
    action_name: &str,
    input_index: usize,
    input_kind: &str,
) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err(format!(
            "Action '{}' child-agent {} input {} must resolve to a non-empty relative path.",
            action_name, input_kind, input_index
        ));
    }

    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(format!(
            "Action '{}' child-agent {} input {} must stay at the current level or below; absolute paths are not allowed.",
            action_name, input_kind, input_index
        ));
    }

    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "Action '{}' child-agent {} input {} must stay at the current level or below; parent traversal (`..`) is not allowed.",
            action_name, input_kind, input_index
        ));
    }

    Ok(())
}

fn validate_child_file_extension(
    path: &str,
    action_name: &str,
    input_index: usize,
) -> Result<(), String> {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());

    match extension.as_deref() {
        Some(
            "pdf" | "docx" | "csv" | "xla" | "xlb" | "xlc" | "xlm" | "xls" | "xlsx" | "xlt"
            | "xlw" | "tsv" | "iif" | "doc" | "dot" | "odt" | "rtf" | "pot" | "ppa" | "pps"
            | "ppt" | "pptx" | "pwz" | "wiz",
        ) => Ok(()),
        _ => Err(format!(
            "Action '{}' child-agent file input {} must use a supported extension: {}.",
            action_name, input_index, SUPPORTED_FILE_EXTENSIONS_MESSAGE
        )),
    }
}

fn current_agent_action_depth() -> u32 {
    std::env::var(AGENT_ACTION_DEPTH_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0)
}

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn new_runtime_budget(max_runtime_secs: u64) -> InvocationRuntimeBudget {
    let started_at_ms = current_time_millis();
    InvocationRuntimeBudget {
        max_runtime_secs,
        started_at_ms,
        deadline_ms: started_at_ms.saturating_add(max_runtime_secs.saturating_mul(1000)),
    }
}

fn inherited_agent_action_runtime_budget() -> Option<InvocationRuntimeBudget> {
    let max_runtime_secs = std::env::var(AGENT_ACTION_MAX_RUNTIME_SECS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())?;
    let started_at_ms = std::env::var(AGENT_ACTION_RUNTIME_STARTED_AT_MS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())?;
    let deadline_ms = std::env::var(AGENT_ACTION_RUNTIME_DEADLINE_MS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())?;

    Some(InvocationRuntimeBudget {
        max_runtime_secs,
        started_at_ms,
        deadline_ms,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn configured_agent_action_runtime_budget(cli_override: Option<u64>) -> InvocationRuntimeBudget {
    configured_agent_action_runtime_budget_with_project_default(cli_override, None)
}

fn configured_agent_action_runtime_budget_with_project_default(
    cli_override: Option<u64>,
    project_default: Option<u64>,
) -> InvocationRuntimeBudget {
    cli_override
        .map(new_runtime_budget)
        .or_else(inherited_agent_action_runtime_budget)
        .or_else(|| project_default.map(new_runtime_budget))
        .unwrap_or_else(|| new_runtime_budget(DEFAULT_AGENT_ACTION_MAX_RUNTIME_SECS))
}

fn remaining_runtime_duration(
    runtime_budget: InvocationRuntimeBudget,
    exhausted_context: &str,
) -> Result<Duration, String> {
    let now = current_time_millis();
    if now >= runtime_budget.deadline_ms {
        return Err(exhausted_context.to_string());
    }

    Ok(Duration::from_millis(
        runtime_budget.deadline_ms.saturating_sub(now),
    ))
}

fn elapsed_runtime_secs(runtime_budget: InvocationRuntimeBudget) -> u64 {
    current_time_millis()
        .saturating_sub(runtime_budget.started_at_ms)
        .div_ceil(1000)
}

fn current_agent_runtime_timeout_message(
    runtime_budget: InvocationRuntimeBudget,
    context: &str,
) -> String {
    format!(
        "Current agent exceeded max-runtime-in-sec {} after {} seconds {}.",
        runtime_budget.max_runtime_secs,
        elapsed_runtime_secs(runtime_budget),
        context
    )
}

fn action_runtime_timeout_message(
    action_name: &str,
    runtime_budget: InvocationRuntimeBudget,
    context: &str,
) -> String {
    format!(
        "Action '{}' exceeded max-runtime-in-sec {} after {} seconds {}.",
        action_name,
        runtime_budget.max_runtime_secs,
        elapsed_runtime_secs(runtime_budget),
        context
    )
}

fn inherited_agent_action_max_depth() -> Option<u32> {
    std::env::var(AGENT_ACTION_MAX_DEPTH_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
}

#[cfg_attr(not(test), allow(dead_code))]
fn configured_agent_action_max_depth(cli_override: Option<u32>) -> u32 {
    configured_agent_action_max_depth_with_project_default(cli_override, None)
}

fn configured_agent_action_max_depth_with_project_default(
    cli_override: Option<u32>,
    project_default: Option<u32>,
) -> u32 {
    cli_override
        .or_else(inherited_agent_action_max_depth)
        .or(project_default)
        .unwrap_or(DEFAULT_AGENT_ACTION_MAX_DEPTH)
}

fn validate_agent_action_depth(
    current_depth: u32,
    max_agent_depth: u32,
    action_name: &str,
) -> Result<(), String> {
    if current_depth >= max_agent_depth {
        return Err(format!(
            "Action '{}' cannot invoke another agent because current depth {} has reached max-agent-depth {}.",
            action_name, current_depth, max_agent_depth
        ));
    }

    Ok(())
}

fn validate_agent_step_target(agent: &str, action_name: &str) -> Result<(), String> {
    if let Some((alias, entrypoint)) = agent.split_once("::") {
        let valid_identifier = |value: &str| {
            value
                .chars()
                .next()
                .map(|ch| ch.is_ascii_alphanumeric())
                .unwrap_or(false)
                && value
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        };
        if valid_identifier(alias) && valid_identifier(entrypoint) {
            return Ok(());
        }
        return Err(format!(
            "Action '{}' package child '{}' must use `alias::entrypoint`; each identifier must start with a letter or number and then use only letters, numbers, '-' or '_'.",
            action_name, agent
        ));
    }
    let agent_path = Path::new(agent);
    if agent.trim().is_empty() {
        return Err(format!(
            "Action '{}' agent step target '{}' must use explicit same-level './childagent' form.",
            action_name, agent
        ));
    }

    if agent_path.is_absolute() {
        return Err(format!(
            "Action '{}' agent step target '{}' must use explicit same-level './childagent' form; absolute paths are not allowed.",
            action_name, agent
        ));
    }

    if agent_path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "Action '{}' agent step target '{}' must use explicit same-level './childagent' form; parent traversal (`..`) is not allowed.",
            action_name, agent
        ));
    }

    if !agent.starts_with("./") {
        let message = if contains_explicit_path_separator(agent) {
            "must use explicit same-level './childagent' form; nested child-agent paths are not allowed."
        } else {
            "must use explicit same-level './childagent' form; bare child-agent names are not allowed."
        };
        return Err(format!(
            "Action '{}' agent step target '{}' {}",
            action_name, agent, message
        ));
    }

    let sibling = &agent[2..];
    if sibling.is_empty() || !is_single_normal_path_component(sibling) {
        return Err(format!(
            "Action '{}' agent step target '{}' must stay at the same level; nested child-agent paths such as './agents/childagent' are not allowed.",
            action_name, agent
        ));
    }

    Ok(())
}

fn artifact_is_json_definition(artifact: &str) -> bool {
    Path::new(artifact)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("json"))
        .unwrap_or(false)
}

fn command_exists_on_path(command: &str) -> bool {
    let Some(path_value) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&path_value).any(|directory| {
        command_candidates_for_directory(&directory, command)
            .into_iter()
            .any(|candidate| candidate.is_file())
    })
}

fn command_candidates_for_directory(directory: &Path, command: &str) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        if Path::new(command).extension().is_some() {
            return vec![directory.join(command)];
        }

        let pathext = std::env::var_os("PATHEXT")
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
        let candidates = pathext
            .to_string_lossy()
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| directory.join(format!("{command}{extension}")))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            vec![directory.join(command)]
        } else {
            candidates
        }
    }

    #[cfg(not(windows))]
    {
        vec![directory.join(command)]
    }
}

fn contains_explicit_path_separator(path: &str) -> bool {
    path.contains('/') || path.contains('\\')
}

fn is_single_normal_path_component(path: &str) -> bool {
    let mut components = Path::new(path).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn step_matches_platform(platforms: Option<&[String]>, current_platform: Option<&str>) -> bool {
    match platforms {
        None => true,
        Some(platforms) => current_platform.is_some_and(|platform| {
            platforms.iter().any(|candidate| candidate == platform)
        }),
    }
}

fn resolve_run_args(
    args: &[RunArg],
    data: &serde_json::Value,
    action_name: &str,
) -> Result<Vec<String>, String> {
    args.iter()
        .enumerate()
        .map(|(index, arg)| resolve_run_arg(arg, data, action_name, index))
        .collect()
}

fn resolve_string_parts(
    parts: &[RunArg],
    data: &serde_json::Value,
    action_name: &str,
    field_name: &str,
) -> Result<String, String> {
    let mut resolved = String::new();

    for (index, part) in parts.iter().enumerate() {
        let value = resolve_run_arg(part, data, action_name, index)?;
        resolved.push_str(&value);
    }

    if resolved.trim().is_empty() {
        return Err(format!(
            "Action '{}' {} resolved to an empty string.",
            action_name, field_name
        ));
    }

    Ok(resolved)
}

fn resolve_run_arg(
    arg: &RunArg,
    data: &serde_json::Value,
    action_name: &str,
    index: usize,
) -> Result<String, String> {
    match arg {
        RunArg::Literal(literal) => Ok(literal.clone()),
        RunArg::Variable(variable) => {
            let Some(value) = lookup_action_variable(data, variable) else {
                return Err(format!(
                    "Action '{}' arg {} references missing variable '{}'.",
                    action_name, index, variable
                ));
            };

            match value {
                serde_json::Value::String(text) => Ok(text.clone()),
                serde_json::Value::Bool(boolean) => Ok(boolean.to_string()),
                serde_json::Value::Number(number) => Ok(number.to_string()),
                serde_json::Value::Array(_) => Err(format!(
                    "Action '{}' arg {} references array-valued variable '{}', which is unsupported for arg substitution.",
                    action_name, index, variable
                )),
                serde_json::Value::Object(_) => Err(format!(
                    "Action '{}' arg {} references object-valued variable '{}', which is unsupported for arg substitution.",
                    action_name, index, variable
                )),
                serde_json::Value::Null => Err(format!(
                    "Action '{}' arg {} references null variable '{}', which is unsupported for arg substitution.",
                    action_name, index, variable
                )),
            }
        }
    }
}

fn lookup_action_variable<'a>(data: &'a serde_json::Value, variable: &str) -> Option<&'a serde_json::Value> {
    if let Some(runtime_name) = variable.strip_prefix("runtime.") {
        return data
            .get("runtime")
            .and_then(serde_json::Value::as_object)
            .and_then(|runtime| runtime.get(runtime_name));
    }

    if variable.contains('.') {
        return None;
    }

    data.get(variable)
}
