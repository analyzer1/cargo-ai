//! Action execution helpers for interpreted runtime flows.
use crate::config::adder::set_account_tokens;
use crate::config::loader::{config_path, find_profile, load_config};
use crate::config::schema::ProfileAuthMode;
use crate::credentials::openai_oauth;
use crate::credentials::store;
use crate::infra_api;
use jsonlogic::apply;
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};

const INFRA_BASE_URL: &str = "https://api.cargo-ai.org";
const AGENT_ACTION_DEPTH_ENV: &str = "CARGO_AI_AGENT_ACTION_DEPTH";
const AGENT_ACTION_MAX_DEPTH_ENV: &str = "CARGO_AI_AGENT_ACTION_MAX_DEPTH";
const AGENT_ACTION_MAX_RUNTIME_SECS_ENV: &str = "CARGO_AI_AGENT_MAX_RUNTIME_SECS";
const AGENT_ACTION_RUNTIME_STARTED_AT_MS_ENV: &str = "CARGO_AI_AGENT_RUNTIME_STARTED_AT_MS";
const AGENT_ACTION_RUNTIME_DEADLINE_MS_ENV: &str = "CARGO_AI_AGENT_RUNTIME_DEADLINE_MS";
const DEFAULT_AGENT_ACTION_MAX_RUNTIME_SECS: u64 = 600;
const SUPPORTED_FILE_EXTENSIONS_MESSAGE: &str = "pdf, docx, csv, xla, xlb, xlc, xlm, xls, xlsx, xlt, xlw, tsv, iif, doc, dot, odt, rtf, pot, ppa, pps, ppt, pptx, pwz, wiz";
const ACTION_LANE_OUTPUT_BUFFER_LIMIT: usize = 6;

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

tokio::task_local! {
    static ACTION_OUTPUT: ActionOutput;
}

tokio::task_local! {
    static DECLARING_PROJECT_ROOT: Option<PathBuf>;
}

pub(crate) async fn scope_declaring_project_root<F>(
    project_root: Option<PathBuf>,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    DECLARING_PROJECT_ROOT.scope(project_root, future).await
}

fn current_declaring_project_root() -> Option<PathBuf> {
    DECLARING_PROJECT_ROOT
        .try_with(|project_root| project_root.clone())
        .ok()
        .flatten()
}

#[derive(Clone)]
pub(crate) struct ActionOutput {
    inner: Arc<Mutex<ActionOutputState>>,
    live_refresh_stop: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionOutputMode {
    AppendOnly,
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestedActionRenderMode {
    Auto,
    Live,
    AppendOnly,
}

struct ActionOutputState {
    mode: ActionOutputMode,
    startup_notice: Option<&'static str>,
    action_execution: crate::ActionExecutionMode,
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

#[derive(Debug)]
enum ChildArtifactInvocation {
    DirectExecutable(PathBuf),
    CargoSubcommand(String),
    StandaloneCargoAi(String),
}

impl ActionOutput {
    pub(crate) fn new(
        action_execution: crate::ActionExecutionMode,
        requested_mode: RequestedActionRenderMode,
        run_started_at: Instant,
    ) -> Self {
        let (mode, startup_notice) =
            resolve_action_render_mode_for_capability(requested_mode, live_dashboard_supported());
        Self::new_for_mode_with_notice(action_execution, mode, startup_notice, run_started_at)
    }

    #[cfg(test)]
    fn new_for_mode(action_execution: crate::ActionExecutionMode, mode: ActionOutputMode) -> Self {
        Self::new_for_mode_with_notice(action_execution, mode, None, Instant::now())
    }

    fn new_for_mode_with_notice(
        action_execution: crate::ActionExecutionMode,
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

    pub(crate) fn print_execution_header(&self) {
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

    pub(crate) fn seed_using_line(&self, using_line: &str) {
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
                println!(
                    "{}",
                    format_action_line(action_index, action_name, "started")
                );
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
                lane.lane_finished_after =
                    lane.lane_started_at.map(|started_at| started_at.elapsed());
                lane.last_message = Some(summary.to_string());
                if append_only {
                    Some(format!(
                        "{} · {}",
                        summary,
                        format_elapsed_duration(
                            lane.lane_finished_after
                                .unwrap_or_else(|| Duration::from_secs(0))
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
                lane.lane_finished_after =
                    lane.lane_started_at.map(|started_at| started_at.elapsed());
                lane.last_message = compact_action_output_line(error)
                    .map(|line| format!("failed: {}", line))
                    .or_else(|| Some("failed".to_string()));
                push_lane_output_message(lane, error);
                if append_only {
                    Some(format!(
                        "failed · {}",
                        format_elapsed_duration(
                            lane.lane_finished_after
                                .unwrap_or_else(|| Duration::from_secs(0))
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
                lane.lane_finished_after =
                    lane.lane_started_at.map(|started_at| started_at.elapsed());
                lane.last_message = compact_action_output_line(error)
                    .map(|line| format!("abort requested: {}", line))
                    .or_else(|| Some("abort requested".to_string()));
                push_lane_output_message(lane, error);
                if append_only {
                    Some(format!(
                        "abort requested · {}: {}",
                        format_elapsed_duration(
                            lane.lane_finished_after
                                .unwrap_or_else(|| Duration::from_secs(0))
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
            lane.lane_finished_after = lane.lane_started_at.map(|started_at| started_at.elapsed());
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

    #[cfg(test)]
    fn snapshot_lines_for_test(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("action output lock should succeed")
            .snapshot_lines()
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
        && std::env::var("TERM")
            .map(|term| term != "dumb")
            .unwrap_or(true)
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
            println!(
                "{}",
                format_action_line(action_index, action_name, line.as_str())
            );
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
    } else if message.contains("no run steps matched") || message.contains("unsupported step kind")
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
        .map(|line| !line.starts_with("using: ") && !line.contains("resolved dynamic child-agent "))
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
        _ => lane.current_step.clone().unwrap_or_else(|| "-".to_string()),
    }
}

fn lane_last_message(lane: &ActionLaneState) -> Option<&str> {
    let message = lane.last_message.as_deref()?;

    if lane.status == ActionLaneStatus::Completed && matches!(message, "completed" | "completed.") {
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

fn run_header_line(
    action_execution: crate::ActionExecutionMode,
    elapsed: Option<Duration>,
) -> String {
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

#[derive(Debug, Clone, Copy)]
pub(crate) struct InvocationRuntimeBudget {
    pub(crate) max_runtime_secs: u64,
    pub(crate) started_at_ms: u64,
    pub(crate) deadline_ms: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ActionProviderContext {
    pub(crate) project_data: Option<super::runtime_data::DataRoot>,
    pub(crate) provider: crate::providers::ProviderKind,
    pub(crate) profile_name: Option<String>,
    pub(crate) auth_mode: String,
    pub(crate) model: String,
    pub(crate) url: String,
    pub(crate) token: String,
    pub(crate) inference_timeout_in_sec: u64,
    pub(crate) tool_resolver: Option<std::sync::Arc<crate::commands::tools::ToolResolver>>,
    pub(crate) package_context:
        Option<crate::commands::local_packages::InstalledPackageRuntimeContext>,
    pub(crate) usage_log: Option<crate::usage_log::UsageLogContext>,
}

impl ActionProviderContext {
    pub(crate) fn using_line(&self) -> String {
        self.using_line_with_model(self.model.as_str())
    }

    pub(crate) fn using_line_with_model(&self, model: &str) -> String {
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

fn provider_server_name(provider: crate::providers::ProviderKind) -> &'static str {
    match provider {
        crate::providers::ProviderKind::Anthropic => "anthropic",
        crate::providers::ProviderKind::Gemini => "gemini",
        crate::providers::ProviderKind::Mistral => "mistral",
        crate::providers::ProviderKind::Ollama => "ollama",
        crate::providers::ProviderKind::OpenAi => "openai",
        crate::providers::ProviderKind::Xai => "xai",
    }
}

fn using_line_model(model: &str) -> &str {
    if model.trim().is_empty() {
        "none"
    } else {
        model
    }
}

fn using_line_url(provider: crate::providers::ProviderKind, url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed == provider.default_url() {
        return None;
    }

    if provider == crate::providers::ProviderKind::OpenAi
        && trimmed == openai_oauth::OPENAI_ACCOUNT_RESPONSES_URL
    {
        return None;
    }

    Some(trimmed.to_string())
}

fn usage_provider_profile(context: &ActionProviderContext) -> Option<&str> {
    context.profile_name.as_deref()
}

fn usage_provider_error(error: &crate::providers::ProviderError) -> crate::usage_log::UsageError {
    crate::usage_log::UsageError::redacted(
        format!("{:?}", error.kind()).to_ascii_lowercase(),
        "Provider request failed.",
    )
}

fn usage_timeout_error() -> crate::usage_log::UsageError {
    crate::usage_log::UsageError::redacted("timeout", "Provider request timed out.")
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
        let mut state = self.inner.lock().expect("abort signal lock should succeed");
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

#[derive(Debug, Clone)]
struct AccountAuth {
    access_token: String,
    refresh_token: Option<String>,
}

#[derive(Debug)]
enum RefreshAccessError {
    MissingRefreshToken,
    RequestFailed(String),
    MissingRefreshedToken(serde_json::Value),
}

/// Applies configured action rules to model output and executes matching steps.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn apply_actions(
    output: &crate::Output,
    actions: &[crate::Action],
    runtime_vars: &serde_json::Map<String, serde_json::Value>,
    named_inputs: &[crate::Input],
    action_execution: crate::ActionExecutionMode,
    action_execution_override: Option<crate::ActionExecutionMode>,
    requested_render_mode: RequestedActionRenderMode,
    provider_context: &ActionProviderContext,
    max_agent_depth: u32,
    runtime_budget: InvocationRuntimeBudget,
) -> Result<(), String> {
    let output_data = serde_json::to_value(output)
        .map_err(|error| format!("Failed to serialize output for action evaluation: {error}"))?;
    apply_actions_with_data(
        &output_data,
        actions,
        runtime_vars,
        named_inputs,
        action_execution,
        action_execution_override,
        requested_render_mode,
        provider_context,
        max_agent_depth,
        runtime_budget,
        Instant::now(),
        None,
    )
    .await
}

pub(crate) async fn apply_actions_with_data(
    output: &serde_json::Value,
    actions: &[crate::Action],
    runtime_vars: &serde_json::Map<String, serde_json::Value>,
    named_inputs: &[crate::Input],
    action_execution: crate::ActionExecutionMode,
    action_execution_override: Option<crate::ActionExecutionMode>,
    requested_render_mode: RequestedActionRenderMode,
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
                let data = match action_data_from_output_value(output, runtime_vars) {
                    Ok(data) => data,
                    Err(error) => {
                        eprintln!("❌ Failed to prepare output for action evaluation: {error}");
                        return Err(format!(
                            "Failed to prepare output for action evaluation: {error}"
                        ));
                    }
                };
                let current_platform = current_action_platform();
                let named_input_lookup = named_input_lookup(named_inputs);
                print_action_execution_header(action_execution);
                let top_level_failures = match action_execution {
                    crate::ActionExecutionMode::Sequential => {
                        apply_actions_sequential(
                            actions,
                            &data,
                            &named_input_lookup,
                            current_platform,
                            action_execution_override,
                            provider_context,
                            max_agent_depth,
                            runtime_budget,
                            &abort_signal,
                        )
                        .await?
                    }
                    crate::ActionExecutionMode::Parallel => {
                        apply_actions_parallel(
                            actions,
                            &data,
                            &named_input_lookup,
                            current_platform,
                            action_execution_override,
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
    actions: &[crate::Action],
    data: &serde_json::Value,
    named_inputs: &BTreeMap<String, crate::Input>,
    current_platform: Option<&'static str>,
    action_execution_override: Option<crate::ActionExecutionMode>,
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
    actions: &[crate::Action],
    data: &serde_json::Value,
    named_inputs: &BTreeMap<String, crate::Input>,
    current_platform: Option<&'static str>,
    action_execution_override: Option<crate::ActionExecutionMode>,
    provider_context: &ActionProviderContext,
    max_agent_depth: u32,
    runtime_budget: InvocationRuntimeBudget,
    abort_signal: &InvocationAbortSignal,
) -> Result<Vec<String>, String> {
    let mut matched_actions = Vec::new();
    let mut lane_tasks = Vec::new();
    let action_output = current_action_output();
    let declaring_project_root = current_declaring_project_root();

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
        let declaring_project_root_clone = declaring_project_root.clone();

        lane_tasks.push(tokio::spawn(async move {
            let lane_future = async move {
                run_matching_action_steps(
                    action_index,
                    &action_clone,
                    &data_clone,
                    &named_inputs_clone,
                    current_platform,
                    action_execution_override,
                    &provider_context_clone,
                    max_agent_depth,
                    runtime_budget,
                    &abort_signal_clone,
                )
                .await
            };
            let lane_future =
                DECLARING_PROJECT_ROOT.scope(declaring_project_root_clone, lane_future);

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

fn named_input_lookup(inputs: &[crate::Input]) -> BTreeMap<String, crate::Input> {
    let mut named = BTreeMap::new();
    for input in inputs {
        if let Some(name) = input.name.as_ref() {
            named.insert(name.clone(), input.clone());
        }
    }
    named
}

fn action_logic_matches(
    action_index: usize,
    action: &crate::Action,
    data: &serde_json::Value,
) -> bool {
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
    action: &crate::Action,
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
    action: &crate::Action,
    data: &serde_json::Value,
    named_inputs: &BTreeMap<String, crate::Input>,
    current_platform: Option<&'static str>,
    action_execution_override: Option<crate::ActionExecutionMode>,
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
            run_exec_step(
                step,
                &action_data,
                action_index,
                &action.name,
                provider_context.package_context.as_ref(),
                runtime_budget,
            )
            .await
            .map(|captured_output| (StepExecutionOutcome::Completed, captured_output))
        } else if step.kind.eq_ignore_ascii_case("email_me") {
            run_email_me_step(
                step,
                &action_data,
                action_index,
                &action.name,
                runtime_budget,
                single_step_action,
            )
            .await
            .map(|outcome| (outcome, None))
        } else if step.kind.eq_ignore_ascii_case("agent") {
            run_agent_step_with_provider_context(
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
                    crate::FailureMode::Continue => {
                        print_action_line(action_index, action.name.as_str(), error.as_str());
                        outcomes.push(StepExecutionOutcome::SoftFailureLogged);
                    }
                    crate::FailureMode::Stop => {
                        return Ok(ActionExecutionResult::Failed(error));
                    }
                    crate::FailureMode::Abort => {
                        abort_signal.trigger(action_index, action.name.as_str(), error.as_str());
                        return Ok(ActionExecutionResult::Aborted(error));
                    }
                }
            }
        }
    }

    Ok(ActionExecutionResult::Completed(outcomes))
}

#[allow(dead_code)]
fn action_data_from_output(
    output: &crate::Output,
    runtime_vars: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, serde_json::Error> {
    let output_data = serde_json::to_value(output)?;
    action_data_from_output_value(&output_data, runtime_vars).map_err(serde_json::Error::io)
}

fn action_data_from_output_value(
    output: &serde_json::Value,
    runtime_vars: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, std::io::Error> {
    let mut data = output.clone();
    if let Some(object) = data.as_object_mut() {
        object.insert(
            "runtime".to_string(),
            serde_json::Value::Object(runtime_vars.clone()),
        );
        return Ok(data);
    }

    Err(std::io::Error::other(
        "validated output must serialize to a top-level object",
    ))
}

async fn run_exec_step(
    step: &crate::RunStep,
    data: &serde_json::Value,
    action_index: usize,
    action_name: &str,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
    runtime_budget: InvocationRuntimeBudget,
) -> Result<Option<(String, String)>, String> {
    if let Some(context) = package_context {
        if !crate::commands::local_packages::hosted_package_allows_subprocess(context) {
            return Err(format!(
                "Action '{}' exec step is blocked for hosted package alias '{}'. The installed package permission profile does not allow unconstrained subprocess execution.",
                action_name, context.alias
            ));
        }
    }
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
        let mut command = tokio::process::Command::new(program);
        command.args(&resolved_args);
        if let Some(context) = package_context {
            command.current_dir(context.package_data_root.as_path());
        }
        let child = command
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
        let mut command = tokio::process::Command::new(program);
        command.args(&resolved_args);
        if let Some(context) = package_context {
            command.current_dir(context.package_data_root.as_path());
        }
        let child = command
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
            Err(_) => Err(action_runtime_timeout_message(
                action_name,
                runtime_budget,
                &format!("while waiting for command '{}'", program),
            )),
        }
    }
}

async fn run_tool_step(
    step: &crate::RunStep,
    data: &serde_json::Value,
    action_index: usize,
    action_name: &str,
    step_index: usize,
    provider_context: &ActionProviderContext,
    action_execution_override: Option<crate::ActionExecutionMode>,
    max_agent_depth: u32,
    runtime_budget: InvocationRuntimeBudget,
) -> Result<Option<(String, String)>, String> {
    let tool_name = step.tool_name.as_deref().ok_or_else(|| {
        format!(
            "Action '{}' tool step is missing required `name`.",
            action_name
        )
    })?;
    if let Some(context) = provider_context.package_context.as_ref() {
        if !crate::commands::local_packages::hosted_package_allows_subprocess(context) {
            return Err(format!(
                "Action '{}' tool step '{}' is blocked for hosted package alias '{}'. The installed package permission profile does not allow unconstrained tool subprocess execution.",
                action_name, tool_name, context.alias
            ));
        }
    }
    let mut usage_tool_guard = provider_context.usage_log.as_ref().map(|usage_log| {
        usage_log.start_tool_run(crate::usage_log::UsageTool {
            name: tool_name.to_string(),
            action: action_name.to_string(),
            step_index: Some(step_index),
        })
    });
    let resolver = provider_context.tool_resolver.as_ref().ok_or_else(|| {
        format!(
            "Action '{}' tool step '{}' cannot resolve tools because no tool resolver is available.",
            action_name, tool_name
        )
    })?;
    let contract = resolver.resolve_contract(tool_name)?;
    let params = crate::commands::tools::resolve_tool_invoke_params(
        step,
        data,
        action_name,
        &contract.describe,
    )?;
    let current_depth = current_agent_action_depth();
    let usage_log_bridge_context = provider_context
        .usage_log
        .as_ref()
        .map(|usage_log| usage_log.tool_bridge_context(tool_name, action_name, Some(step_index)));
    let artifact_root = if provider_context.project_data.is_some() {
        Some(std::env::current_dir().map_err(|error| error.to_string())?)
    } else if let Some(context) = provider_context.package_context.as_ref() {
        Some(
            context
                .current_entrypoint_path
                .as_deref()
                .and_then(|entrypoint| Path::new(entrypoint).parent())
                .map(|parent| context.package_payload_root.join(parent))
                .unwrap_or_else(|| context.package_payload_root.clone()),
        )
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
                    crate::ActionExecutionMode::Sequential => "sequential",
                    crate::ActionExecutionMode::Parallel => "parallel",
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
    if let Some(context) = provider_context.package_context.as_ref() {
        command.current_dir(context.package_data_root.as_path());
    } else if let Some(root) = provider_context.project_data.as_ref() {
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

    let result =
        crate::commands::tools::validate_tool_invoke_response(&contract.resolved, &output.stdout)?;
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
    step: &crate::RunStep,
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
    step: &crate::RunStep,
    error: &str,
    action_name: &str,
) -> Result<(), String> {
    let Some(name) = step.error_variable.as_deref() else {
        return Ok(());
    };

    insert_action_string_variable(data, name, error.to_string(), action_name)
}

fn step_failure_mode(step: &crate::RunStep) -> crate::FailureMode {
    step.failure_mode
        .clone()
        .unwrap_or(crate::FailureMode::Stop)
}

fn should_run_step(
    step: &crate::RunStep,
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
    step: &crate::RunStep,
    data: &serde_json::Value,
    action_index: usize,
    action_name: &str,
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

    let response = tokio::time::timeout(remaining, async {
        let auth = load_account_auth()?;
        let access_token_owned = auth.access_token;
        let refresh_token = auth.refresh_token;

        let mut response = infra_api::account::send_mail::send_test_mail(
            INFRA_BASE_URL,
            access_token_owned.as_str(),
            subject.as_str(),
            text.as_str(),
        )
        .await
        .map_err(|error| format!("Request failed: {error:?}"))?;

        let is_expired_error = response
            .get("type")
            .and_then(|v| v.as_str())
            .map(|t| t == "access_token_expired")
            .unwrap_or(false);

        if is_expired_error {
            response = match refresh_access_token_for_retry(
                access_token_owned.as_str(),
                refresh_token.as_deref(),
            )
            .await
            {
                Err(RefreshAccessError::MissingRefreshToken) => {
                    return Err(
                        "Access token expired, and no refresh token exists in credential store. Run `cargo ai account status` or re-confirm account."
                            .to_string(),
                    );
                }
                Err(RefreshAccessError::RequestFailed(error)) => {
                    return Err(format!("Request failed while refreshing session: {error}"));
                }
                Err(RefreshAccessError::MissingRefreshedToken(refresh_response)) => {
                    return Err(
                        format_backend_error_message(&refresh_response).unwrap_or_else(|| {
                            "Session refresh did not return a new access token. Cannot retry email_me action."
                                .to_string()
                        }),
                    );
                }
                Ok((retry_access_token, refreshed_expires_in)) => {
                    if let Some(rt) = refresh_token.as_deref() {
                        persist_refreshed_access_token(
                            retry_access_token.as_str(),
                            rt,
                            refreshed_expires_in,
                        );
                    }

                    infra_api::account::send_mail::send_test_mail(
                        INFRA_BASE_URL,
                        retry_access_token.as_str(),
                        subject.as_str(),
                        text.as_str(),
                    )
                    .await
                    .map_err(|error| format!("Request failed after session refresh: {error:?}"))?
                }
            };
        }

        let succeeded = response
            .get("status")
            .and_then(|v| v.as_str())
            .map(|status| status.eq_ignore_ascii_case("success"))
            .unwrap_or(false);

        if succeeded {
            Ok(response)
        } else {
            Err(format_backend_error_message(&response).unwrap_or_else(|| {
                format!(
                    "Action '{}' email_me request failed.\n{}",
                    action_name,
                    pretty_backend_json(&response)
                )
            }))
        }
    })
    .await;

    let response = match response {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(error),
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
    for line in render_backend_ui_or_json_lines(&response) {
        print_action_line(action_index, action_name, line.as_str());
    }
    Ok(if single_step_action {
        StepExecutionOutcome::SuccessAlreadyPrinted
    } else {
        StepExecutionOutcome::Completed
    })
}

async fn run_generate_image_step(
    step: &crate::RunStep,
    data: &serde_json::Value,
    named_inputs: &BTreeMap<String, crate::Input>,
    action_index: usize,
    action_name: &str,
    step_index: usize,
    provider_context: &ActionProviderContext,
    runtime_budget: InvocationRuntimeBudget,
) -> Result<StepExecutionOutcome, String> {
    let step_profile_context = resolve_generate_image_step_profile_context(
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
        effective_provider_context
            .using_line_with_model(model.as_str())
            .as_str(),
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
        provider_context.package_context.as_ref(),
        provider_context.project_data.as_ref(),
    )?;

    let remaining = remaining_runtime_duration(
        runtime_budget,
        &format!("before starting image generation with model '{}'", model),
    )
    .map_err(|context| {
        action_runtime_timeout_message(action_name, runtime_budget, context.as_str())
    })?;

    let provider_started_at = Instant::now();
    let image_response = match tokio::time::timeout(remaining, async {
        match effective_provider_context.provider {
            crate::providers::ProviderKind::Anthropic => {
                Err(crate::providers::ProviderError::invalid_request(
                    crate::providers::ProviderKind::Anthropic,
                    "Anthropic image generation is not supported.",
                ))
            }
            crate::providers::ProviderKind::Gemini => {
                Err(crate::providers::ProviderError::invalid_request(
                    crate::providers::ProviderKind::Gemini,
                    "Gemini image generation is not supported.",
                ))
            }
            crate::providers::ProviderKind::Mistral => {
                Err(crate::providers::ProviderError::invalid_request(
                    crate::providers::ProviderKind::Mistral,
                    "Mistral image generation is not supported.",
                ))
            }
            crate::providers::ProviderKind::OpenAi => {
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
            crate::providers::ProviderKind::Ollama => {
                crate::providers::send_ollama_image_request(
                    &effective_provider_context.url,
                    &model,
                    prompt.as_str(),
                    effective_provider_context.inference_timeout_in_sec,
                    &effective_provider_context.token,
                )
                .await
            }
            crate::providers::ProviderKind::Xai => {
                Err(crate::providers::ProviderError::invalid_request(
                    crate::providers::ProviderKind::Xai,
                    "xAI image generation is not supported.",
                ))
            }
        }
    })
    .await
    {
        Ok(Ok(response)) => {
            if let Some(usage_log) = provider_context.usage_log.as_ref() {
                usage_log.record_provider_request(crate::usage_log::UsageProviderRequest {
                    provider: effective_provider_context.provider,
                    profile_name: usage_provider_profile(effective_provider_context),
                    auth_mode: effective_provider_context.auth_mode.as_str(),
                    model: model.as_str(),
                    step: crate::usage_log::UsageStep {
                        kind: "generate_image",
                        action: Some(action_name.to_string()),
                        step_index: Some(step_index),
                    },
                    usage: response.usage.as_ref(),
                    duration: provider_started_at.elapsed(),
                    status: crate::usage_log::UsageStatus::Success,
                    error: None,
                });
            }
            response
        }
        Ok(Err(error)) => {
            if let Some(usage_log) = provider_context.usage_log.as_ref() {
                usage_log.record_provider_request(crate::usage_log::UsageProviderRequest {
                    provider: effective_provider_context.provider,
                    profile_name: usage_provider_profile(effective_provider_context),
                    auth_mode: effective_provider_context.auth_mode.as_str(),
                    model: model.as_str(),
                    step: crate::usage_log::UsageStep {
                        kind: "generate_image",
                        action: Some(action_name.to_string()),
                        step_index: Some(step_index),
                    },
                    usage: None,
                    duration: provider_started_at.elapsed(),
                    status: crate::usage_log::UsageStatus::Failed,
                    error: Some(usage_provider_error(&error)),
                });
            }
            let mut lines = vec![format!(
                "Action '{}' generate_image step failed.",
                action_name
            )];
            lines.extend(crate::providers::provider_error_messages(&error));
            return Err(lines.join("\n"));
        }
        Err(_) => {
            if let Some(usage_log) = provider_context.usage_log.as_ref() {
                usage_log.record_provider_request(crate::usage_log::UsageProviderRequest {
                    provider: effective_provider_context.provider,
                    profile_name: usage_provider_profile(effective_provider_context),
                    auth_mode: effective_provider_context.auth_mode.as_str(),
                    model: model.as_str(),
                    step: crate::usage_log::UsageStep {
                        kind: "generate_image",
                        action: Some(action_name.to_string()),
                        step_index: Some(step_index),
                    },
                    usage: None,
                    duration: provider_started_at.elapsed(),
                    status: crate::usage_log::UsageStatus::Failed,
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

    let output_path_ref = resolve_generated_image_output_path_with_data(
        output_path.as_str(),
        action_name,
        provider_context.package_context.as_ref(),
        provider_context.project_data.as_ref(),
    )?;
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

    std::fs::write(output_path_ref.as_path(), image_bytes).map_err(|error| {
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
    profile: Option<&crate::RunArg>,
    data: &serde_json::Value,
    action_name: &str,
    step_kind: &str,
) -> Result<Option<String>, String> {
    let Some(profile) = profile else {
        return Ok(None);
    };

    let profile_name = match profile {
        crate::RunArg::Literal(literal) => literal.clone(),
        crate::RunArg::Variable(variable) => {
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

fn resolve_profile_api_token_for_action_step(
    profile: &crate::config::schema::Profile,
) -> Result<String, String> {
    match store::load_profile_token(&profile.name) {
        Ok(Some(token)) if !token.trim().is_empty() => Ok(token),
        Ok(Some(_)) | Ok(None) => profile
            .token
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

async fn resolve_generate_image_step_profile_context(
    profile: Option<&crate::RunArg>,
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

    let provider = crate::providers::ProviderKind::from_server_value(profile.server.as_str())
        .ok_or_else(|| {
            format!(
                "Action '{}' generate_image step profile '{}' uses unsupported server '{}'.",
                action_name, profile.name, profile.server
            )
        })?;

    let mut url = profile.url.clone().unwrap_or_default();
    let token = match profile.auth_mode {
        ProfileAuthMode::ApiKey => resolve_profile_api_token_for_action_step(profile)?,
        ProfileAuthMode::OpenaiAccount => {
            if provider != crate::providers::ProviderKind::OpenAi {
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
            url = openai_oauth::OPENAI_ACCOUNT_RESPONSES_URL.to_string();
            openai_oauth::resolve_session_for_runtime()
                .await
                .map(|session| session.access_token)?
        }
        ProfileAuthMode::None => match provider {
            crate::providers::ProviderKind::Anthropic => {
                return Err(format!(
                    "Action '{}' generate_image step profile '{}' auth mode is '{}'. Anthropic requires '{}'.",
                    action_name,
                    profile.name,
                    ProfileAuthMode::None.as_str(),
                    ProfileAuthMode::ApiKey.as_str()
                ));
            }
            crate::providers::ProviderKind::Gemini => {
                return Err(format!(
                    "Action '{}' generate_image step profile '{}' auth mode is '{}'. Gemini requires '{}'.",
                    action_name,
                    profile.name,
                    ProfileAuthMode::None.as_str(),
                    ProfileAuthMode::ApiKey.as_str()
                ));
            }
            crate::providers::ProviderKind::Mistral => {
                return Err(format!(
                    "Action '{}' generate_image step profile '{}' auth mode is '{}'. Mistral requires '{}'.",
                    action_name,
                    profile.name,
                    ProfileAuthMode::None.as_str(),
                    ProfileAuthMode::ApiKey.as_str()
                ));
            }
            crate::providers::ProviderKind::Ollama => String::new(),
            crate::providers::ProviderKind::OpenAi => {
                return Err(format!(
                    "Action '{}' generate_image step profile '{}' auth mode is '{}'. Set it to '{}' or '{}' before using it here.",
                    action_name,
                    profile.name,
                    ProfileAuthMode::None.as_str(),
                    ProfileAuthMode::ApiKey.as_str(),
                    ProfileAuthMode::OpenaiAccount.as_str()
                ));
            }
            crate::providers::ProviderKind::Xai => {
                return Err(format!(
                    "Action '{}' generate_image step profile '{}' auth mode is '{}'. xAI requires '{}'.",
                    action_name,
                    profile.name,
                    ProfileAuthMode::None.as_str(),
                    ProfileAuthMode::ApiKey.as_str()
                ));
            }
        },
    };

    if url.trim().is_empty() {
        url = provider.default_url().to_string();
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
        model: profile.model.clone(),
        url,
        token,
        inference_timeout_in_sec: invocation_timeout_in_sec,
        tool_resolver: None,
        package_context: None,
        usage_log: None,
    }))
}

fn resolve_generate_image_model(
    model: Option<&crate::RunArg>,
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
        crate::RunArg::Literal(literal) => {
            if literal.trim().is_empty() {
                return Err(format!(
                    "Action '{}' generate_image `model` must resolve to a non-empty string.",
                    action_name
                ));
            }
            Ok(literal.clone())
        }
        crate::RunArg::Variable(variable) => {
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

async fn run_agent_step_with_provider_context(
    step: &crate::RunStep,
    data: &serde_json::Value,
    named_inputs: &BTreeMap<String, crate::Input>,
    action_index: usize,
    action_name: &str,
    step_index: usize,
    provider_context: &ActionProviderContext,
    action_execution_override: Option<crate::ActionExecutionMode>,
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
    let invocation = resolve_child_artifact_invocation(
        artifact,
        action_name,
        provider_context.package_context.as_ref(),
    )?;
    let mut command = child_artifact_command(&invocation);
    if crate::commands::package_dependencies::is_package_reference(artifact) {
        let declaring_project_root = package_child_project_root(
            provider_context.package_context.as_ref(),
            action_name,
            artifact,
        )?;
        command.current_dir(declaring_project_root);
    } else if let Some(context) = provider_context
        .package_context
        .as_ref()
        .filter(|context| context.source_kind == "hosted" || artifact_is_json_definition(artifact))
    {
        command.current_dir(context.package_data_root.as_path());
    }
    if let Some(action_execution_override) = action_execution_override {
        command.arg("--action-execution");
        command.arg(match action_execution_override {
            crate::ActionExecutionMode::Sequential => "sequential",
            crate::ActionExecutionMode::Parallel => "parallel",
        });
    }
    if step.ignore_tools {
        command.arg("--ignore-tools");
    }
    if let Some(usage_log_path) = step.usage_log.as_deref() {
        let resolved_usage_log_path = resolve_child_usage_log_path_with_data(
            usage_log_path,
            action_name,
            provider_context.package_context.as_ref(),
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
    if let Some(profile_name) =
        resolve_step_profile_name(step.profile.as_ref(), data, action_name, "agent")?
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
    let (child_args, resolution_notes) = child_input_args_in_context(
        step.run_vars.as_deref(),
        step.input_overrides.as_deref(),
        step.input_mode,
        step.inputs.as_deref(),
        data,
        action_name,
        named_inputs,
        provider_context.package_context.as_ref(),
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
        for (key, value) in usage_log.direct_child_env(crate::usage_log::UsageLaunchedBy {
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
        action_runtime_timeout_message(action_name, runtime_budget, context.as_str())
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
                &format!(
                    "while waiting for child agent '{}' at depth {}",
                    artifact,
                    current_depth + 1
                ),
            ))
        }
    };
    result
}

#[cfg(test)]
async fn run_agent_step(
    step: &crate::RunStep,
    data: &serde_json::Value,
    named_inputs: &BTreeMap<String, crate::Input>,
    action_index: usize,
    action_name: &str,
    action_execution_override: Option<crate::ActionExecutionMode>,
    max_agent_depth: u32,
    runtime_budget: InvocationRuntimeBudget,
) -> Result<StepExecutionOutcome, String> {
    let provider_context = ActionProviderContext {
        project_data: None,
        provider: crate::providers::ProviderKind::OpenAi,
        profile_name: None,
        auth_mode: "none".to_string(),
        model: String::new(),
        url: crate::providers::ProviderKind::OpenAi
            .default_url()
            .to_string(),
        token: String::new(),
        inference_timeout_in_sec: 60,
        tool_resolver: None,
        package_context: None,
        usage_log: None,
    };
    run_agent_step_with_provider_context(
        step,
        data,
        named_inputs,
        action_index,
        action_name,
        1,
        &provider_context,
        action_execution_override,
        max_agent_depth,
        runtime_budget,
    )
    .await
}

fn resolve_child_artifact_invocation(
    artifact: &str,
    action_name: &str,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
) -> Result<ChildArtifactInvocation, String> {
    if crate::commands::package_dependencies::is_package_reference(artifact) {
        if let Some(context) = package_context.filter(|context| context.source_kind == "hosted") {
            if !crate::commands::local_packages::hosted_package_allows_subprocess(context) {
                return Err(format!(
                    "Action '{}' cross-package child '{}' is blocked for hosted package alias '{}'. This invocation starts another Cargo AI process and requires an explicitly accepted subprocess permission.",
                    action_name, artifact, context.alias
                ));
            }
        }
        let project_root = package_child_project_root(package_context, action_name, artifact)?;
        crate::commands::local_packages::resolve_entrypoint_reference_for_project(
            artifact,
            false,
            Some(project_root.as_path()),
        )?
        .ok_or_else(|| {
            format!(
                "Action '{}' package child '{}' is not a valid `alias::entrypoint` reference.",
                action_name, artifact
            )
        })?;

        let cargo_ai_exists = command_exists_on_path("cargo-ai");
        if command_exists_on_path("cargo") && cargo_ai_exists {
            return Ok(ChildArtifactInvocation::CargoSubcommand(
                artifact.to_string(),
            ));
        }
        if cargo_ai_exists {
            return Ok(ChildArtifactInvocation::StandaloneCargoAi(
                artifact.to_string(),
            ));
        }
        return Err(format!(
            "Action '{}' package child '{}' requires Cargo AI to be available as `cargo ai` or `cargo-ai` on PATH.",
            action_name, artifact
        ));
    }

    validate_agent_step_target(artifact, action_name)?;
    if let Some(context) = package_context
        .filter(|context| context.source_kind == "hosted" || artifact_is_json_definition(artifact))
    {
        let (package_relative_path, artifact_path) =
            crate::commands::local_packages::resolve_package_payload_path_from_current_entrypoint(
                context, artifact,
            )
            .map_err(|error| format!("Action '{}': {}", action_name, error))?;
        if !artifact_is_json_definition(artifact) {
            if !crate::commands::local_packages::hosted_package_allows_subprocess(context) {
                return Err(format!(
                    "Action '{}' child executable '{}' is blocked for hosted package alias '{}'. Reinstall or transition the package with an explicitly accepted subprocess permission before direct child execution.",
                    action_name, artifact, context.alias
                ));
            }
            if !artifact_path.is_file() {
                return Err(format!(
                    "Action '{}' child executable '{}' was not found in the verified package payload.",
                    action_name, artifact
                ));
            }
            return Ok(ChildArtifactInvocation::DirectExecutable(artifact_path));
        }

        let exported = context
            .entrypoints
            .iter()
            .find(|entrypoint| {
                entrypoint.runnable && entrypoint.path == package_relative_path
            })
            .ok_or_else(|| {
                let source_label = if context.source_kind == "hosted" {
                    "hosted"
                } else {
                    "installed"
                };
                format!(
                    "Action '{}' {} child agent '{}' must resolve to a declared runnable export in package alias '{}'.",
                    action_name, source_label, artifact, context.alias
                )
            })?;
        let reference = format!("{}::{}", context.alias, exported.name);
        let cargo_ai_exists = command_exists_on_path("cargo-ai");
        if command_exists_on_path("cargo") && cargo_ai_exists {
            return Ok(ChildArtifactInvocation::CargoSubcommand(reference));
        }
        if cargo_ai_exists {
            return Ok(ChildArtifactInvocation::StandaloneCargoAi(reference));
        }
        let source_label = if context.source_kind == "hosted" {
            "hosted"
        } else {
            "installed"
        };
        return Err(format!(
            "Action '{}' {} child agent '{}' requires Cargo AI to be available as `cargo ai` or `cargo-ai` on PATH.",
            action_name, source_label, artifact
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
        return Ok(ChildArtifactInvocation::CargoSubcommand(
            artifact.to_string(),
        ));
    }
    if cargo_ai_exists {
        return Ok(ChildArtifactInvocation::StandaloneCargoAi(
            artifact.to_string(),
        ));
    }

    Err(format!(
        "Action '{}' agent step JSON artifact '{}' requires Cargo AI to be available as `cargo ai` or `cargo-ai` on PATH.",
        action_name, artifact
    ))
}

fn package_child_project_root(
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
    action_name: &str,
    artifact: &str,
) -> Result<PathBuf, String> {
    let declaring_project_root = current_declaring_project_root();
    let current_dir = std::env::current_dir().ok();
    package_child_project_root_from(
        package_context,
        declaring_project_root.as_deref(),
        current_dir.as_deref(),
        action_name,
        artifact,
    )
}

fn package_child_project_root_from(
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
    declaring_project_root: Option<&Path>,
    current_dir: Option<&Path>,
    action_name: &str,
    artifact: &str,
) -> Result<PathBuf, String> {
    let selected_root = package_context
        .map(|context| context.package_payload_root.clone())
        .or_else(|| declaring_project_root.map(Path::to_path_buf));
    let selected_root = match selected_root {
        Some(root) => Some(root),
        None => match current_dir {
            Some(current_dir) => {
                crate::commands::package_dependencies::find_project_root(current_dir)?
            }
            None => None,
        },
    };
    selected_root.ok_or_else(|| {
            format!(
                "Action '{}' package child '{}' requires a project `.cargo-ai/project.toml` with a matching `[package_dependencies.<alias>]` declaration.",
                action_name, artifact
            )
        })
}

fn child_artifact_command(invocation: &ChildArtifactInvocation) -> tokio::process::Command {
    match invocation {
        ChildArtifactInvocation::DirectExecutable(path) => tokio::process::Command::new(path),
        ChildArtifactInvocation::CargoSubcommand(reference) => {
            let mut command = tokio::process::Command::new("cargo");
            command.arg("ai");
            command.arg("run");
            command.arg(reference);
            command
        }
        ChildArtifactInvocation::StandaloneCargoAi(reference) => {
            let mut command = tokio::process::Command::new("cargo-ai");
            command.arg("run");
            command.arg(reference);
            command
        }
    }
}

#[cfg(test)]
fn resolve_child_usage_log_path(
    raw_path: &str,
    action_name: &str,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
) -> Result<PathBuf, String> {
    resolve_child_usage_log_path_with_data(raw_path, action_name, package_context, None)
}

fn resolve_child_usage_log_path_with_data(
    raw_path: &str,
    action_name: &str,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
    project_data: Option<&super::runtime_data::DataRoot>,
) -> Result<PathBuf, String> {
    let path = Path::new(raw_path);
    validate_child_usage_log_path(path, action_name)?;
    if let Some(context) = package_context {
        return crate::commands::local_packages::resolve_package_data_path(context, path)
            .map_err(|error| format!("Action '{}': {}", action_name, error));
    }
    match project_data {
        Some(root) => root.resolve(path),
        None => Ok(path.to_path_buf()),
    }
}

fn validate_child_usage_log_path(path: &Path, action_name: &str) -> Result<(), String> {
    let raw_path = path.to_string_lossy();
    crate::commands::local_packages::normalize_portable_relative_path(
        raw_path.as_ref(),
        format!("Action '{}' agent `usage_log`", action_name).as_str(),
    )?;
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
                StepExecutionOutcome::SoftFailureLogged
                    | StepExecutionOutcome::SuccessAlreadyPrinted
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
        Some(format!(
            "Run complete · {} total",
            format_elapsed_duration(elapsed)
        ))
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
        output.action_step_started(
            action_index,
            action_name,
            step_kind,
            step_number,
            step_count,
        );
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

fn action_execution_header(action_execution: crate::ActionExecutionMode) -> &'static str {
    match action_execution {
        crate::ActionExecutionMode::Sequential => "run: sequential",
        crate::ActionExecutionMode::Parallel => "run: parallel",
    }
}

fn print_action_execution_header(action_execution: crate::ActionExecutionMode) {
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
        println!(
            "{}",
            format_action_line(action_index, action_name, using_line)
        );
    }
}

fn action_lane_prefix(action_index: usize, action_name: &str) -> String {
    format!("[Action {}: {}]", action_index + 1, action_name)
}

fn format_action_line(action_index: usize, action_name: &str, message: &str) -> String {
    format!(
        "{} {}",
        action_lane_prefix(action_index, action_name),
        message
    )
}

fn format_action_failure(action_index: usize, action_name: &str, error: &str) -> String {
    format_action_line(
        action_index,
        action_name,
        format!("failed: {}", error).as_str(),
    )
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
        println!(
            "{}",
            format_action_line(action_index, action_name, using_line)
        );
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

fn render_backend_ui_or_json_lines(response: &serde_json::Value) -> Vec<String> {
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

    let kind = ui
        .get("kind")
        .and_then(|value| value.as_str())
        .unwrap_or("info");
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
                let label = action
                    .get("label")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
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
                    let label = item
                        .get("label")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
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

fn load_account_auth() -> Result<AccountAuth, String> {
    let cfg = load_config().ok_or_else(|| {
        format!(
            "❌ No local config file found at '{}'. Run `cargo ai account register <email>` on this machine, or copy your config from another machine.",
            config_path().display()
        )
    })?;

    let acct = cfg.account.as_ref().ok_or_else(|| {
        "❌ No account found in config. You must confirm your account first.".to_string()
    })?;

    if let Some(account_tokens) = store::load_account_tokens()
        .map_err(|error| format!("❌ Failed to load account credentials: {error}"))?
    {
        return Ok(AccountAuth {
            access_token: account_tokens.access_token,
            refresh_token: account_tokens.refresh_token,
        });
    }

    let access_token = acct.access_token.as_ref().cloned().ok_or_else(|| {
        "❌ No access token found in credentials store or legacy config. Run `cargo ai account confirm <code>` first."
            .to_string()
    })?;

    Ok(AccountAuth {
        access_token,
        refresh_token: acct.refresh_token.clone(),
    })
}

async fn refresh_access_token_for_retry(
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(String, Option<i32>), RefreshAccessError> {
    let rt = refresh_token.ok_or(RefreshAccessError::MissingRefreshToken)?;

    let refresh_response =
        infra_api::account::status::fetch_status(INFRA_BASE_URL, access_token, Some(rt))
            .await
            .map_err(|error| RefreshAccessError::RequestFailed(format!("{error:?}")))?;

    let refreshed_access_token = refresh_response
        .get("session")
        .and_then(|session| session.get("access_token"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());

    let refreshed_expires_in = refresh_response
        .get("session")
        .and_then(|session| session.get("expires_in_seconds"))
        .and_then(|value| value.as_i64())
        .and_then(|value| i32::try_from(value).ok());

    match refreshed_access_token {
        Some(token) => Ok((token, refreshed_expires_in)),
        None => Err(RefreshAccessError::MissingRefreshedToken(refresh_response)),
    }
}

fn persist_refreshed_access_token(
    refreshed_access_token: &str,
    refresh_token: &str,
    refreshed_expires_in: Option<i32>,
) {
    if let Some(expires_in) = refreshed_expires_in {
        if let Err(error) = set_account_tokens(
            refreshed_access_token.to_string(),
            refresh_token.to_string(),
            expires_in,
        ) {
            eprintln!("⚠️ Failed to update account tokens in credential store: {error}");
        }
    }
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
pub(crate) fn configured_agent_action_runtime_budget(
    cli_override: Option<u64>,
) -> InvocationRuntimeBudget {
    configured_agent_action_runtime_budget_with_project_default(cli_override, None)
}

pub(crate) fn configured_agent_action_runtime_budget_with_project_default(
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

fn validate_agent_step_target(agent: &str, action_name: &str) -> Result<(), String> {
    crate::commands::local_packages::normalize_portable_relative_path(
        agent,
        format!("Action '{}' agent step target", action_name).as_str(),
    )?;
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

        let pathext = std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
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
    run_steps: &'a [crate::RunStep],
    current_platform: Option<&str>,
) -> Vec<&'a crate::RunStep> {
    run_steps
        .iter()
        .filter(|step| step_matches_platform(step.platforms.as_deref(), current_platform))
        .collect()
}

#[cfg(test)]
fn child_input_args(
    run_vars: Option<&[crate::ActionRunVar]>,
    input_overrides: Option<&[crate::ActionInputOverride]>,
    input_mode: Option<crate::ActionInputMode>,
    inputs: Option<&[crate::ActionInput]>,
    data: &serde_json::Value,
    action_name: &str,
    named_inputs: &BTreeMap<String, crate::Input>,
) -> Result<(Vec<String>, Vec<String>), String> {
    child_input_args_with_package_context(
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

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn child_input_args_with_package_context(
    run_vars: Option<&[crate::ActionRunVar]>,
    input_overrides: Option<&[crate::ActionInputOverride]>,
    input_mode: Option<crate::ActionInputMode>,
    inputs: Option<&[crate::ActionInput]>,
    data: &serde_json::Value,
    action_name: &str,
    named_inputs: &BTreeMap<String, crate::Input>,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
) -> Result<(Vec<String>, Vec<String>), String> {
    child_input_args_in_context(
        run_vars,
        input_overrides,
        input_mode,
        inputs,
        data,
        action_name,
        named_inputs,
        package_context,
        None,
    )
}

fn child_input_args_in_context(
    run_vars: Option<&[crate::ActionRunVar]>,
    input_overrides: Option<&[crate::ActionInputOverride]>,
    input_mode: Option<crate::ActionInputMode>,
    inputs: Option<&[crate::ActionInput]>,
    data: &serde_json::Value,
    action_name: &str,
    named_inputs: &BTreeMap<String, crate::Input>,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
    project_data: Option<&super::runtime_data::DataRoot>,
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
            validate_hosted_child_input_override(
                &input_override.value,
                resolved_value.as_str(),
                action_name,
                &input_override.name,
                named_inputs,
                package_context,
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
                crate::ActionInputMode::Replace => "replace",
                crate::ActionInputMode::Append => "append",
                crate::ActionInputMode::Prepend => "prepend",
            }
            .to_string(),
        );
    }

    if let Some(inputs) = inputs {
        for (index, input) in inputs.iter().enumerate() {
            match input {
                crate::ActionInput::Text { text } => {
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
                crate::ActionInput::Url { url } => {
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
                crate::ActionInput::Image { path } => {
                    let resolved_path = resolve_string_parts(
                        path,
                        data,
                        action_name,
                        &format!("child-agent image path input {}", index + 1),
                    )?;
                    let resolved = resolve_installed_child_input_path_with_data(
                        resolved_path.as_str(),
                        child_input_uses_dynamic_parts(path),
                        action_name,
                        index + 1,
                        "image",
                        package_context,
                        project_data,
                    )?;
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
                crate::ActionInput::File { path } => {
                    let resolved_path = resolve_string_parts(
                        path,
                        data,
                        action_name,
                        &format!("child-agent file path input {}", index + 1),
                    )?;
                    let resolved = resolve_installed_child_input_path_with_data(
                        resolved_path.as_str(),
                        child_input_uses_dynamic_parts(path),
                        action_name,
                        index + 1,
                        "file",
                        package_context,
                        project_data,
                    )?;
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
                crate::ActionInput::Named { input } => {
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
                    if package_context.is_some_and(|context| context.source_kind == "hosted")
                        && !matches!(
                            forwarded.kind,
                            crate::InputKind::Image | crate::InputKind::File
                        )
                        && child_override_looks_like_external_path(value)
                    {
                        return Err(format!(
                            "Action '{}' child-agent named input '{}' is blocked because hosted package code cannot reinterpret text or URL input as an external filesystem path.",
                            action_name, input
                        ));
                    }
                    args.push("--input-override".to_string());
                    args.push(format!("{}={}", input, value));
                }
            }
        }
    }

    Ok((args, notes))
}

fn validate_hosted_child_input_override(
    source: &crate::ActionInputOverrideValue,
    resolved_value: &str,
    action_name: &str,
    override_name: &str,
    named_inputs: &BTreeMap<String, crate::Input>,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
) -> Result<(), String> {
    if package_context.is_none_or(|context| context.source_kind != "hosted") {
        return Ok(());
    }

    let inherits_constrained_path = match source {
        crate::ActionInputOverrideValue::NamedInput { input } => {
            named_inputs.get(input).is_some_and(|value| {
                matches!(value.kind, crate::InputKind::Image | crate::InputKind::File)
            })
        }
        crate::ActionInputOverrideValue::Literal(_)
        | crate::ActionInputOverrideValue::Variable(_) => false,
    };
    if inherits_constrained_path || !child_override_looks_like_external_path(resolved_value) {
        return Ok(());
    }

    Err(format!(
        "Action '{}' child-agent named input override '{}' is blocked because hosted package code cannot introduce an absolute, prefixed, or parent-traversing path. Forward a declared image/file input supplied by the caller, or use a relative path under the package data directory.",
        action_name, override_name
    ))
}

fn child_override_looks_like_external_path(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.starts_with("http://") || value.starts_with("https://") {
        return false;
    }

    let portable = value.replace('\\', "/");
    let bytes = portable.as_bytes();
    Path::new(value).is_absolute()
        || portable.starts_with('/')
        || portable.starts_with("file:")
        || portable.split('/').any(|component| component == "..")
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

#[cfg(test)]
fn resolve_installed_child_input_path(
    raw_path: &str,
    dynamic: bool,
    action_name: &str,
    input_index: usize,
    input_kind: &str,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
) -> Result<String, String> {
    resolve_installed_child_input_path_with_data(
        raw_path,
        dynamic,
        action_name,
        input_index,
        input_kind,
        package_context,
        None,
    )
}

fn resolve_installed_child_input_path_with_data(
    raw_path: &str,
    dynamic: bool,
    action_name: &str,
    input_index: usize,
    input_kind: &str,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
    project_data: Option<&super::runtime_data::DataRoot>,
) -> Result<String, String> {
    validate_child_input_path(raw_path, action_name, input_index, input_kind)?;
    let Some(context) = package_context else {
        return match project_data.filter(|_| dynamic) {
            Some(root) => root
                .resolve(Path::new(raw_path))
                .map(|path| path.to_string_lossy().into_owned()),
            None => Ok(raw_path.to_string()),
        };
    };

    let resolved = if dynamic {
        crate::commands::local_packages::resolve_package_data_path(context, Path::new(raw_path))
            .map_err(|error| format!("Action '{}': {}", action_name, error))?
    } else {
        crate::commands::local_packages::resolve_package_payload_path(context, Path::new(raw_path))
            .map_err(|error| format!("Action '{}': {}", action_name, error))?
    };
    Ok(resolved.to_string_lossy().to_string())
}

fn resolve_child_input_override_value(
    input: &crate::ActionInputOverrideValue,
    data: &serde_json::Value,
    action_name: &str,
    override_name: &str,
    named_inputs: &BTreeMap<String, crate::Input>,
) -> Result<(String, Option<String>), String> {
    match input {
        crate::ActionInputOverrideValue::Literal(literal) => Ok((literal.clone(), None)),
        crate::ActionInputOverrideValue::Variable(variable) => {
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
        crate::ActionInputOverrideValue::NamedInput { input } => {
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
    value: &crate::ActionRunVarValue,
    data: &serde_json::Value,
    action_name: &str,
    run_var_name: &str,
) -> Result<(String, Option<String>), String> {
    match value {
        crate::ActionRunVarValue::Literal(literal) => Ok((
            stringify_scalar_json_value(
                literal,
                action_name,
                &format!("child-agent runtime var '{}'", run_var_name),
            )?,
            None,
        )),
        crate::ActionRunVarValue::Variable(variable) => {
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

fn child_input_uses_dynamic_parts(parts: &[crate::RunArg]) -> bool {
    parts
        .iter()
        .any(|part| matches!(part, crate::RunArg::Variable(_)))
}

fn validate_generated_image_output_path(path: &Path, action_name: &str) -> Result<(), String> {
    let raw_path = path.to_string_lossy();
    crate::commands::local_packages::normalize_portable_relative_path(
        raw_path.as_ref(),
        format!("Action '{}' generate_image `path`", action_name).as_str(),
    )?;
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

fn generated_image_output_format(
    raw_path: &str,
    action_name: &str,
) -> Result<&'static str, String> {
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
    provider: crate::providers::ProviderKind,
    output_format: &str,
    action_name: &str,
) -> Result<(), String> {
    if provider == crate::providers::ProviderKind::Ollama && output_format != "png" {
        return Err(format!(
            "Action '{}' generate_image step targeting Ollama currently requires a `.png` output path because the current Ollama compatibility slice only guarantees `b64_json` image payloads, not OpenAI-style output-format selection.",
            action_name
        ));
    }

    Ok(())
}

#[cfg(test)]
fn resolve_generated_image_output_path(
    raw_path: &str,
    action_name: &str,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
) -> Result<PathBuf, String> {
    resolve_generated_image_output_path_with_data(raw_path, action_name, package_context, None)
}

fn resolve_generated_image_output_path_with_data(
    raw_path: &str,
    action_name: &str,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
    project_data: Option<&super::runtime_data::DataRoot>,
) -> Result<PathBuf, String> {
    let output_path_ref = Path::new(raw_path);
    validate_generated_image_output_path(output_path_ref, action_name)?;
    if let Some(context) = package_context {
        return crate::commands::local_packages::resolve_package_data_path(
            context,
            output_path_ref,
        )
        .map_err(|error| format!("Action '{}': {}", action_name, error));
    }
    match project_data {
        Some(root) => root.resolve(output_path_ref),
        None => Ok(output_path_ref.to_path_buf()),
    }
}

#[cfg(test)]
fn resolve_generate_image_reference_images(
    references: Option<&[crate::GenerateImageReference]>,
    data: &serde_json::Value,
    action_name: &str,
    named_inputs: &BTreeMap<String, crate::Input>,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
) -> Result<Vec<crate::providers::ImageReference>, String> {
    resolve_generate_image_reference_images_with_data(
        references,
        data,
        action_name,
        named_inputs,
        package_context,
        None,
    )
}

fn resolve_generate_image_reference_images_with_data(
    references: Option<&[crate::GenerateImageReference]>,
    data: &serde_json::Value,
    action_name: &str,
    named_inputs: &BTreeMap<String, crate::Input>,
    package_context: Option<&crate::commands::local_packages::InstalledPackageRuntimeContext>,
    project_data: Option<&super::runtime_data::DataRoot>,
) -> Result<Vec<crate::providers::ImageReference>, String> {
    let Some(references) = references else {
        return Ok(Vec::new());
    };

    let mut resolved = Vec::with_capacity(references.len());
    for (index, reference) in references.iter().enumerate() {
        let path = match reference {
            crate::GenerateImageReference::Path { path } => {
                let resolved = resolve_string_parts(
                    path,
                    data,
                    action_name,
                    &format!("reference_images[{}].path", index),
                )
                .map_err(|error| format!("Action '{}': {error}", action_name))?;
                validate_generate_image_reference_path(resolved.as_str(), action_name, index + 1)?;
                if let Some(context) = package_context {
                    let resolved = if child_input_uses_dynamic_parts(path) {
                        crate::commands::local_packages::resolve_package_data_path(
                            context,
                            Path::new(resolved.as_str()),
                        )
                        .map_err(|error| format!("Action '{}': {}", action_name, error))?
                    } else {
                        crate::commands::local_packages::resolve_package_payload_path(
                            context,
                            Path::new(resolved.as_str()),
                        )
                        .map_err(|error| format!("Action '{}': {}", action_name, error))?
                    };
                    resolved.to_string_lossy().to_string()
                } else if let Some(root) =
                    project_data.filter(|_| child_input_uses_dynamic_parts(path))
                {
                    root.resolve(Path::new(&resolved))?
                        .to_string_lossy()
                        .into_owned()
                } else {
                    resolved
                }
            }
            crate::GenerateImageReference::Named { input } => {
                let named_input = named_inputs.get(input).ok_or_else(|| {
                    format!(
                        "Action '{}' generate_image reference image {} named input '{}' is not available.",
                        action_name,
                        index + 1,
                        input
                    )
                })?;
                if named_input.kind != crate::InputKind::Image {
                    return Err(format!(
                        "Action '{}' generate_image reference image {} named input '{}' must have type `image`.",
                        action_name,
                        index + 1,
                        input
                    ));
                }
                named_input.value.as_deref().ok_or_else(|| {
                    format!(
                        "Action '{}' generate_image reference image {} named input '{}' is required but unresolved for this invocation.",
                        action_name,
                        index + 1,
                        input
                    )
                })?.to_string()
            }
        };

        resolved.push(
            crate::providers::load_image_reference(path.as_str()).map_err(|error| {
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
    provider: crate::providers::ProviderKind,
    reference_images: Option<&[crate::GenerateImageReference]>,
    action_name: &str,
) -> Result<(), String> {
    if provider == crate::providers::ProviderKind::Ollama
        && reference_images.is_some_and(|images| !images.is_empty())
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
    crate::commands::local_packages::normalize_portable_relative_path(
        path,
        format!(
            "Action '{}' generate_image reference image {}",
            action_name, input_index
        )
        .as_str(),
    )?;
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

fn validate_child_input_url(
    url: &str,
    action_name: &str,
    input_index: usize,
) -> Result<(), String> {
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
    crate::commands::local_packages::normalize_portable_relative_path(
        path,
        format!(
            "Action '{}' child-agent {} input {}",
            action_name, input_kind, input_index
        )
        .as_str(),
    )?;
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
            "pdf" | "docx" | "csv" | "xla" | "xlb" | "xlc" | "xlm" | "xls" | "xlsx" | "xlt" | "xlw"
            | "tsv" | "iif" | "doc" | "dot" | "odt" | "rtf" | "pot" | "ppa" | "pps" | "ppt"
            | "pptx" | "pwz" | "wiz",
        ) => Ok(()),
        _ => Err(format!(
            "Action '{}' child-agent file input {} must use a supported extension: {}.",
            action_name, input_index, SUPPORTED_FILE_EXTENSIONS_MESSAGE
        )),
    }
}

pub(crate) fn current_agent_action_depth() -> u32 {
    std::env::var(AGENT_ACTION_DEPTH_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0)
}

fn step_matches_platform(platforms: Option<&[String]>, current_platform: Option<&str>) -> bool {
    match platforms {
        None => true,
        Some(platforms) => current_platform
            .is_some_and(|platform| platforms.iter().any(|candidate| candidate == platform)),
    }
}

fn resolve_run_args(
    args: &[crate::RunArg],
    data: &serde_json::Value,
    action_name: &str,
) -> Result<Vec<String>, String> {
    args.iter()
        .enumerate()
        .map(|(index, arg)| resolve_run_arg(arg, data, action_name, index))
        .collect()
}

fn resolve_string_parts(
    parts: &[crate::RunArg],
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
    arg: &crate::RunArg,
    data: &serde_json::Value,
    action_name: &str,
    index: usize,
) -> Result<String, String> {
    match arg {
        crate::RunArg::Literal(literal) => Ok(literal.clone()),
        crate::RunArg::Variable(variable) => {
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

fn lookup_action_variable<'a>(
    data: &'a serde_json::Value,
    variable: &str,
) -> Option<&'a serde_json::Value> {
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

#[cfg(test)]
mod tests {
    use super::{
        action_completion_summary, action_execution_header, action_lane_prefix, apply_actions,
        child_input_args, configured_agent_action_runtime_budget, format_backend_error_message,
        format_backend_ui_message, format_elapsed_duration, insert_action_output_variable,
        matching_run_steps,
        resolve_action_render_mode_for_capability as resolve_action_output_mode_for_capability,
        resolve_child_artifact_invocation, resolve_generate_image_reference_images,
        resolve_generate_image_step_profile_context, resolve_installed_child_input_path,
        resolve_run_args, resolve_string_parts, run_agent_step,
        run_agent_step_with_provider_context, run_completion_message_for_depth, run_exec_step,
        run_generate_image_step, run_header_line, run_tool_step, step_matches_platform,
        validate_agent_action_depth, ActionOutput, ActionOutputMode, ActionProviderContext,
        ChildArtifactInvocation, RequestedActionRenderMode as RequestedActionOutputMode,
        StepExecutionOutcome, ACTION_OUTPUT,
    };
    use crate::credentials::openai_oauth;
    use crate::providers::ProviderKind;
    use serde_json::json;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, MutexGuard};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    struct TestCargoHome {
        _guard: MutexGuard<'static, ()>,
        original_cargo_home: Option<OsString>,
        original_disable_keychain: Option<OsString>,
        root: PathBuf,
    }

    impl TestCargoHome {
        fn new(config_toml: &str) -> Self {
            let guard = super::TEST_ENV_LOCK
                .lock()
                .expect("environment lock should not be poisoned");
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be valid")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("cargo-ai-runtime-actions-{unique}"));
            let config_path = root.join(".cargo-ai").join("config.toml");
            fs::create_dir_all(
                config_path
                    .parent()
                    .expect("config path should have a parent directory"),
            )
            .expect("temp config dir should be created");
            fs::write(&config_path, config_toml).expect("temp config should be written");

            let original_cargo_home = std::env::var_os("CARGO_HOME");
            let original_disable_keychain = std::env::var_os("CARGO_AI_DISABLE_KEYCHAIN");
            std::env::set_var("CARGO_HOME", &root);
            std::env::set_var("CARGO_AI_DISABLE_KEYCHAIN", "1");

            Self {
                _guard: guard,
                original_cargo_home,
                original_disable_keychain,
                root,
            }
        }
    }

    impl Drop for TestCargoHome {
        fn drop(&mut self) {
            match &self.original_cargo_home {
                Some(value) => std::env::set_var("CARGO_HOME", value),
                None => std::env::remove_var("CARGO_HOME"),
            }
            match &self.original_disable_keychain {
                Some(value) => std::env::set_var("CARGO_AI_DISABLE_KEYCHAIN", value),
                None => std::env::remove_var("CARGO_AI_DISABLE_KEYCHAIN"),
            }

            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct TestPathCommands {
        _guard: MutexGuard<'static, ()>,
        original_path: Option<OsString>,
        root: PathBuf,
    }

    impl TestPathCommands {
        fn new() -> Self {
            let guard = super::TEST_ENV_LOCK
                .lock()
                .expect("environment lock should not be poisoned");
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be valid")
                .as_nanos();
            let root = std::env::temp_dir().join(format!("cargo-ai-path-test-{unique}"));
            fs::create_dir_all(&root).expect("temp PATH dir should be created");

            let original_path = std::env::var_os("PATH");
            seed_passthrough_rustc(&root, original_path.as_ref());
            std::env::set_var("PATH", &root);

            Self {
                _guard: guard,
                original_path,
                root,
            }
        }

        fn write_command(&self, name: &str, body: &str) -> PathBuf {
            #[cfg(windows)]
            let command_name = format!("{name}.cmd");
            #[cfg(not(windows))]
            let command_name = name.to_string();

            let command_path = self.root.join(command_name);
            fs::write(&command_path, body).expect("test command should be written");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                let mut permissions = fs::metadata(&command_path)
                    .expect("command metadata should load")
                    .permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&command_path, permissions)
                    .expect("test command should be executable");
            }

            command_path
        }
    }

    impl Drop for TestPathCommands {
        fn drop(&mut self) {
            match &self.original_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }

            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn seed_passthrough_rustc(root: &Path, original_path: Option<&OsString>) {
        let Some(rustc_path) = find_command_on_path(original_path, "rustc") else {
            return;
        };

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let _ = symlink(&rustc_path, root.join("rustc"));
        }

        #[cfg(windows)]
        {
            let _ = fs::copy(&rustc_path, root.join("rustc.exe"));
        }
    }

    fn find_command_on_path(path: Option<&OsString>, command: &str) -> Option<PathBuf> {
        let Some(path) = path else {
            return None;
        };

        for directory in std::env::split_paths(path) {
            for candidate in super::command_candidates_for_directory(&directory, command) {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }

        None
    }

    fn run_step(
        program: &str,
        platforms: Option<&[&str]>,
        args: Vec<crate::RunArg>,
    ) -> crate::RunStep {
        crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "exec".to_string(),
            program: Some(program.to_string()),
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args,
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: platforms.map(|platforms| {
                platforms
                    .iter()
                    .map(|platform| platform.to_string())
                    .collect()
            }),
        }
    }

    fn provider_context() -> ActionProviderContext {
        ActionProviderContext {
            project_data: None,
            provider: ProviderKind::OpenAi,
            profile_name: Some("test_profile".to_string()),
            auth_mode: "api_key".to_string(),
            model: "gpt-5.2".to_string(),
            url: "https://api.openai.com/v1/chat/completions".to_string(),
            token: "test-token".to_string(),
            inference_timeout_in_sec: 60,
            tool_resolver: None,
            package_context: None,
            usage_log: None,
        }
    }

    fn hosted_package_context(
        install_root: &Path,
        subprocess: &str,
    ) -> crate::commands::local_packages::InstalledPackageRuntimeContext {
        let package_payload_root = install_root.join("package");
        let package_data_root = install_root.join("data");
        fs::create_dir_all(package_payload_root.join("agents"))
            .expect("package agents directory should exist");
        fs::create_dir_all(&package_data_root).expect("package data directory should exist");
        crate::commands::local_packages::InstalledPackageRuntimeContext {
            alias: "image_generator".to_string(),
            source_kind: "hosted".to_string(),
            package_payload_root,
            package_data_root,
            current_entrypoint_path: Some("agents/observer.json".to_string()),
            entrypoints: vec![
                crate::commands::local_packages::InstalledPackageEntrypointDocument {
                    name: "observer".to_string(),
                    path: "agents/observer.json".to_string(),
                    runnable: true,
                    hatchable: false,
                },
                crate::commands::local_packages::InstalledPackageEntrypointDocument {
                    name: "child".to_string(),
                    path: "agents/child.json".to_string(),
                    runnable: true,
                    hatchable: false,
                },
            ],
            permissions: crate::commands::local_packages::PackagePermissionProfileDocument {
                subprocess: subprocess.to_string(),
                ..crate::commands::local_packages::PackagePermissionProfileDocument::default()
            },
        }
    }

    fn local_package_context(
        install_root: &Path,
    ) -> crate::commands::local_packages::InstalledPackageRuntimeContext {
        let mut context = hosted_package_context(install_root, "blocked_without_explicit_grant");
        context.source_kind = "local".to_string();
        context
    }

    fn profile_config_with_server_and_auth(
        profile_name: &str,
        server: &str,
        server_url: &str,
        model: &str,
        auth_mode: &str,
    ) -> String {
        format!(
            r#"
[[profile]]
name = "{profile_name}"
server = "{server}"
model = "{model}"
url = "{server_url}"
timeout_in_sec = 42
auth_mode = "{auth_mode}"
"#,
        ) + if auth_mode == "api_key" {
            "token = \"profile-token\"\n"
        } else {
            ""
        }
    }

    fn profile_config(profile_name: &str, server_url: &str, model: &str) -> String {
        profile_config_with_server_and_auth(profile_name, "openai", server_url, model, "api_key")
    }

    fn ollama_profile_config(profile_name: &str, server_url: &str, model: &str) -> String {
        profile_config_with_server_and_auth(profile_name, "ollama", server_url, model, "none")
    }

    fn ollama_provider_context(server_url: &str, model: &str) -> ActionProviderContext {
        ActionProviderContext {
            project_data: None,
            provider: ProviderKind::Ollama,
            profile_name: Some("ollama_profile".to_string()),
            auth_mode: "none".to_string(),
            model: model.to_string(),
            url: server_url.to_string(),
            token: String::new(),
            inference_timeout_in_sec: 60,
            tool_resolver: None,
            package_context: None,
            usage_log: None,
        }
    }

    fn ollama_image_response_bytes(body: &[u8]) -> String {
        use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

        format!(
            r#"{{"data":[{{"b64_json":"{}"}}]}}"#,
            BASE64_STANDARD.encode(body)
        )
    }

    fn action(run: Vec<crate::RunStep>) -> crate::Action {
        crate::Action {
            name: "demo".to_string(),
            logic: json!({ "==": [{ "var": "answer" }, 4] }),
            run,
        }
    }

    fn runtime_vars(
        entries: &[(&str, serde_json::Value)],
    ) -> serde_json::Map<String, serde_json::Value> {
        entries
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect()
    }

    fn no_named_inputs() -> std::collections::BTreeMap<String, crate::Input> {
        std::collections::BTreeMap::new()
    }

    fn named_input(name: &str, kind: crate::InputKind, value: Option<&str>) -> crate::Input {
        crate::Input {
            name: Some(name.to_string()),
            kind,
            value: value.map(str::to_string),
        }
    }

    #[test]
    fn platformless_steps_match_supported_platforms() {
        assert!(step_matches_platform(None, Some("macos")));
        assert!(step_matches_platform(None, Some("linux")));
        assert!(step_matches_platform(None, None));
    }

    #[test]
    fn explicit_platforms_match_only_listed_platforms() {
        let platforms = vec!["macos".to_string(), "linux".to_string()];
        assert!(step_matches_platform(Some(&platforms), Some("macos")));
        assert!(step_matches_platform(Some(&platforms), Some("linux")));
        assert!(!step_matches_platform(Some(&platforms), Some("windows")));
        assert!(!step_matches_platform(Some(&platforms), None));
    }

    #[test]
    fn matching_run_steps_preserve_declared_order() {
        let run_steps = vec![
            run_step("first", Some(&["windows"]), vec![]),
            run_step("second", None, vec![]),
            run_step("third", Some(&["macos", "linux"]), vec![]),
            run_step("fourth", None, vec![]),
        ];

        let matching = matching_run_steps(&run_steps, Some("macos"));
        let programs = matching
            .iter()
            .map(|step| {
                step.program
                    .as_deref()
                    .expect("exec test steps have a program")
            })
            .collect::<Vec<_>>();

        assert_eq!(programs, vec!["second", "third", "fourth"]);
    }

    #[test]
    fn resolves_literal_and_variable_args() {
        let resolved = resolve_run_args(
            &[
                crate::RunArg::Literal("value=".to_string()),
                crate::RunArg::Variable("answer".to_string()),
                crate::RunArg::Variable("raining".to_string()),
            ],
            &json!({
                "answer": 4,
                "raining": true
            }),
            "demo",
        )
        .expect("args should resolve");

        assert_eq!(resolved, vec!["value=", "4", "true"]);
    }

    #[test]
    fn rejects_missing_variable_args() {
        let error = resolve_run_args(
            &[crate::RunArg::Variable("answer".to_string())],
            &json!({}),
            "demo",
        )
        .unwrap_err();

        assert!(error.contains("missing variable 'answer'"));
    }

    #[test]
    fn rejects_array_valued_variable_args() {
        let error = resolve_run_args(
            &[crate::RunArg::Variable("numbers".to_string())],
            &json!({
                "numbers": [1, 2, 3]
            }),
            "demo",
        )
        .unwrap_err();

        assert!(error.contains("array-valued variable 'numbers'"));
    }

    #[test]
    fn resolves_runtime_variable_args() {
        let resolved = resolve_run_args(
            &[
                crate::RunArg::Literal("mode=".to_string()),
                crate::RunArg::Variable("runtime.image_model".to_string()),
                crate::RunArg::Variable("runtime.generate_images".to_string()),
            ],
            &json!({
                "runtime": {
                    "image_model": "gpt-image-1",
                    "generate_images": true
                }
            }),
            "demo",
        )
        .expect("runtime args should resolve");

        assert_eq!(resolved, vec!["mode=", "gpt-image-1", "true"]);
    }

    #[test]
    fn resolves_runtime_variable_string_parts() {
        let resolved = resolve_string_parts(
            &[
                crate::RunArg::Literal("subject=".to_string()),
                crate::RunArg::Variable("runtime.report_suffix".to_string()),
            ],
            &json!({
                "runtime": {
                    "report_suffix": "nightly"
                }
            }),
            "demo",
            "subject",
        )
        .expect("runtime string parts should resolve");

        assert_eq!(resolved, "subject=nightly");
    }

    #[test]
    fn resolves_string_parts_without_implicit_spaces() {
        let resolved = resolve_string_parts(
            &[
                crate::RunArg::Literal("raining=".to_string()),
                crate::RunArg::Variable("raining".to_string()),
            ],
            &json!({
                "raining": true
            }),
            "demo",
            "text",
        )
        .expect("string parts should resolve");

        assert_eq!(resolved, "raining=true");
    }

    #[test]
    fn child_input_args_map_to_runtime_flags() {
        let (args, notes) = child_input_args(
            None,
            None,
            None,
            Some(&[
                crate::ActionInput::Text {
                    text: vec![crate::RunArg::Literal("hello".to_string())],
                },
                crate::ActionInput::Url {
                    url: vec![crate::RunArg::Literal("https://example.com".to_string())],
                },
                crate::ActionInput::Image {
                    path: vec![crate::RunArg::Literal("./diagram.png".to_string())],
                },
                crate::ActionInput::File {
                    path: vec![crate::RunArg::Literal("./report.pdf".to_string())],
                },
            ]),
            &json!({}),
            "demo",
            &no_named_inputs(),
        )
        .expect("child input args should resolve");

        assert_eq!(
            args,
            vec![
                "--input-text",
                "hello",
                "--input-url",
                "https://example.com",
                "--input-image",
                "./diagram.png",
                "--input-file",
                "./report.pdf",
            ]
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn child_input_args_include_explicit_input_mode() {
        let (args, notes) = child_input_args(
            None,
            None,
            Some(crate::ActionInputMode::Prepend),
            Some(&[crate::ActionInput::Text {
                text: vec![crate::RunArg::Literal("hello".to_string())],
            }]),
            &json!({}),
            "demo",
            &no_named_inputs(),
        )
        .expect("child input args should resolve");

        assert_eq!(
            args,
            vec!["--input-mode", "prepend", "--input-text", "hello"]
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn child_input_args_reject_input_mode_without_inputs() {
        let error = child_input_args(
            None,
            None,
            Some(crate::ActionInputMode::Append),
            None,
            &json!({}),
            "demo",
            &no_named_inputs(),
        )
        .unwrap_err();

        assert!(error.contains("child-agent `input_mode` requires `inputs`"));
    }

    #[test]
    fn child_input_args_resolve_dynamic_text_and_file_path() {
        let (args, notes) = child_input_args(
            None,
            None,
            None,
            Some(&[
                crate::ActionInput::Text {
                    text: vec![
                        crate::RunArg::Literal("hello ".to_string()),
                        crate::RunArg::Variable("customer".to_string()),
                    ],
                },
                crate::ActionInput::File {
                    path: vec![
                        crate::RunArg::Literal("./reports/".to_string()),
                        crate::RunArg::Variable("report_filename".to_string()),
                    ],
                },
            ]),
            &json!({
                "customer": "Acme",
                "report_filename": "q1.pdf"
            }),
            "demo",
            &no_named_inputs(),
        )
        .expect("dynamic child input args should resolve");

        assert_eq!(
            args,
            vec![
                "--input-text",
                "hello Acme",
                "--input-file",
                "./reports/q1.pdf"
            ]
        );
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("dynamic child-agent text input 1"));
        assert!(notes[1].contains("./reports/q1.pdf"));
    }

    #[test]
    fn child_input_args_reject_invalid_dynamic_url() {
        let error = child_input_args(
            None,
            None,
            None,
            Some(&[crate::ActionInput::Url {
                url: vec![crate::RunArg::Variable("source_url".to_string())],
            }]),
            &json!({
                "source_url": "ftp://example.com/report"
            }),
            "demo",
            &no_named_inputs(),
        )
        .unwrap_err();

        assert!(error.contains("must resolve to an http(s) URL"));
    }

    #[test]
    fn child_input_args_reject_invalid_dynamic_file_extension() {
        let error = child_input_args(
            None,
            None,
            None,
            Some(&[crate::ActionInput::File {
                path: vec![
                    crate::RunArg::Literal("./reports/".to_string()),
                    crate::RunArg::Variable("report_filename".to_string()),
                ],
            }]),
            &json!({
                "report_filename": "q1.exe"
            }),
            "demo",
            &no_named_inputs(),
        )
        .unwrap_err();

        assert!(error.contains("supported extension"));
    }

    #[test]
    fn child_input_args_reject_parent_traversal_in_dynamic_path() {
        let error = child_input_args(
            None,
            None,
            None,
            Some(&[crate::ActionInput::Image {
                path: vec![crate::RunArg::Variable("image_path".to_string())],
            }]),
            &json!({
                "image_path": "../diagram.png"
            }),
            "demo",
            &no_named_inputs(),
        )
        .unwrap_err();

        assert!(error.contains("parent traversal"));
    }

    #[test]
    fn child_input_args_forward_named_inputs_as_standard_overrides() {
        let (args, notes) = child_input_args(
            None,
            None,
            None,
            Some(&[crate::ActionInput::Named {
                input: "menu_image".to_string(),
            }]),
            &json!({}),
            "demo",
            &std::collections::BTreeMap::from([(
                "menu_image".to_string(),
                named_input(
                    "menu_image",
                    crate::InputKind::Image,
                    Some("./artifacts/menu.png"),
                ),
            )]),
        )
        .expect("named child input args should resolve");

        assert_eq!(
            args,
            vec!["--input-override", "menu_image=./artifacts/menu.png"]
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn child_input_args_reject_unresolved_named_inputs() {
        let error = child_input_args(
            None,
            None,
            None,
            Some(&[crate::ActionInput::Named {
                input: "menu_image".to_string(),
            }]),
            &json!({}),
            "demo",
            &std::collections::BTreeMap::from([(
                "menu_image".to_string(),
                named_input("menu_image", crate::InputKind::Image, None),
            )]),
        )
        .unwrap_err();

        assert!(error.contains("required but unresolved"));
    }

    #[test]
    fn child_input_args_emit_named_override_flags_before_runtime_inputs() {
        let (args, notes) = child_input_args(
            None,
            Some(&[
                crate::ActionInputOverride {
                    name: "menu_note".to_string(),
                    value: crate::ActionInputOverrideValue::Literal("spring menu".to_string()),
                },
                crate::ActionInputOverride {
                    name: "source_url".to_string(),
                    value: crate::ActionInputOverrideValue::Literal(
                        "https://example.com/menu".to_string(),
                    ),
                },
            ]),
            Some(crate::ActionInputMode::Append),
            Some(&[crate::ActionInput::Text {
                text: vec![crate::RunArg::Literal("extra context".to_string())],
            }]),
            &json!({}),
            "demo",
            &no_named_inputs(),
        )
        .expect("child input args should resolve");

        assert_eq!(
            args,
            vec![
                "--input-override",
                "menu_note=spring menu",
                "--input-override",
                "source_url=https://example.com/menu",
                "--input-mode",
                "append",
                "--input-text",
                "extra context",
            ]
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn child_input_args_support_parent_named_inputs_inside_input_overrides() {
        let (args, notes) = child_input_args(
            None,
            Some(&[crate::ActionInputOverride {
                name: "menu_image".to_string(),
                value: crate::ActionInputOverrideValue::NamedInput {
                    input: "menu_image".to_string(),
                },
            }]),
            None,
            None,
            &json!({}),
            "demo",
            &std::collections::BTreeMap::from([(
                "menu_image".to_string(),
                named_input(
                    "menu_image",
                    crate::InputKind::Image,
                    Some("./artifacts/menu.png"),
                ),
            )]),
        )
        .expect("parent named input should resolve inside child override");

        assert_eq!(
            args,
            vec!["--input-override", "menu_image=./artifacts/menu.png"]
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn hosted_child_override_rejects_external_variable_path() {
        let install_root = std::env::temp_dir().join(format!(
            "cai2102-hosted-child-override-{}",
            uuid::Uuid::new_v4()
        ));
        let context =
            hosted_package_context(install_root.as_path(), "blocked_without_explicit_grant");
        let error = super::child_input_args_with_package_context(
            None,
            Some(&[crate::ActionInputOverride {
                name: "source_doc".to_string(),
                value: crate::ActionInputOverrideValue::Variable("source_doc".to_string()),
            }]),
            None,
            None,
            &json!({ "source_doc": "../../private/report.pdf" }),
            "demo",
            &no_named_inputs(),
            Some(&context),
        )
        .expect_err("hosted package variables must not introduce external paths");

        assert!(error.contains("hosted package code cannot introduce"));
        let _ = fs::remove_dir_all(install_root);
    }

    #[test]
    fn hosted_child_override_allows_constrained_parent_path_input() {
        let install_root = std::env::temp_dir().join(format!(
            "cai2102-hosted-child-parent-input-{}",
            uuid::Uuid::new_v4()
        ));
        let context =
            hosted_package_context(install_root.as_path(), "blocked_without_explicit_grant");
        let explicit_path = std::env::temp_dir().join("caller-selected-report.pdf");
        let named_inputs = std::collections::BTreeMap::from([(
            "source_doc".to_string(),
            named_input("source_doc", crate::InputKind::File, explicit_path.to_str()),
        )]);
        let (args, notes) = super::child_input_args_with_package_context(
            None,
            Some(&[crate::ActionInputOverride {
                name: "source_doc".to_string(),
                value: crate::ActionInputOverrideValue::NamedInput {
                    input: "source_doc".to_string(),
                },
            }]),
            None,
            None,
            &json!({}),
            "demo",
            &named_inputs,
            Some(&context),
        )
        .expect("a declared parent path input should preserve the caller grant");

        assert_eq!(
            args,
            vec![
                "--input-override".to_string(),
                format!("source_doc={}", explicit_path.display()),
            ]
        );
        assert!(notes.is_empty());
        let _ = fs::remove_dir_all(install_root);
    }

    #[test]
    fn hosted_child_named_text_cannot_be_reinterpreted_as_external_path() {
        let install_root = std::env::temp_dir().join(format!(
            "cai2102-hosted-child-text-input-{}",
            uuid::Uuid::new_v4()
        ));
        let context =
            hosted_package_context(install_root.as_path(), "blocked_without_explicit_grant");
        let error = super::child_input_args_with_package_context(
            None,
            None,
            None,
            Some(&[crate::ActionInput::Named {
                input: "menu_note".to_string(),
            }]),
            &json!({}),
            "demo",
            &std::collections::BTreeMap::from([(
                "menu_note".to_string(),
                named_input(
                    "menu_note",
                    crate::InputKind::Text,
                    Some("/private/report.pdf"),
                ),
            )]),
            Some(&context),
        )
        .expect_err("hosted text input must not become an implicit filesystem grant");

        assert!(error.contains("cannot reinterpret text or URL input"));
        let _ = fs::remove_dir_all(install_root);
    }

    #[test]
    fn child_input_args_resolve_dynamic_named_override_values() {
        let (args, notes) = child_input_args(
            None,
            Some(&[
                crate::ActionInputOverride {
                    name: "menu_note".to_string(),
                    value: crate::ActionInputOverrideValue::Variable("menu_note_value".to_string()),
                },
                crate::ActionInputOverride {
                    name: "source_doc".to_string(),
                    value: crate::ActionInputOverrideValue::Variable(
                        "source_doc_value".to_string(),
                    ),
                },
            ]),
            None,
            None,
            &json!({
                "menu_note_value": "hello Acme",
                "source_doc_value": "./reports/q1.pdf"
            }),
            "demo",
            &no_named_inputs(),
        )
        .expect("dynamic named overrides should resolve");

        assert_eq!(
            args,
            vec![
                "--input-override",
                "menu_note=hello Acme",
                "--input-override",
                "source_doc=./reports/q1.pdf",
            ]
        );
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("named override 'menu_note'"));
        assert!(notes[1].contains("named override 'source_doc'"));
        assert!(!notes[0].contains("hello Acme"));
        assert!(!notes[1].contains("./reports/q1.pdf"));
    }

    #[test]
    fn child_input_args_reject_object_valued_named_override_variable() {
        let error = child_input_args(
            None,
            Some(&[crate::ActionInputOverride {
                name: "source_url".to_string(),
                value: crate::ActionInputOverrideValue::Variable("source_url".to_string()),
            }]),
            None,
            None,
            &json!({
                "source_url": { "bad": true }
            }),
            "demo",
            &no_named_inputs(),
        )
        .unwrap_err();

        assert!(error.contains("named input override 'source_url'"));
        assert!(error.contains("object-valued variable"));
    }

    #[test]
    fn child_input_args_emit_run_var_flags_before_overrides() {
        let (args, notes) = child_input_args(
            Some(&[
                crate::ActionRunVar {
                    name: "year".to_string(),
                    value: crate::ActionRunVarValue::Literal(json!(2026)),
                },
                crate::ActionRunVar {
                    name: "month".to_string(),
                    value: crate::ActionRunVarValue::Literal(json!("08")),
                },
            ]),
            Some(&[crate::ActionInputOverride {
                name: "menu_note".to_string(),
                value: crate::ActionInputOverrideValue::Literal("spring menu".to_string()),
            }]),
            None,
            Some(&[crate::ActionInput::Text {
                text: vec![crate::RunArg::Literal("extra context".to_string())],
            }]),
            &json!({}),
            "demo",
            &no_named_inputs(),
        )
        .expect("child run_vars should resolve");

        assert_eq!(
            args,
            vec![
                "--run-var",
                "year=2026",
                "--run-var",
                "month=08",
                "--input-override",
                "menu_note=spring menu",
                "--input-text",
                "extra context",
            ]
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn child_input_args_resolve_dynamic_run_var_values() {
        let (args, notes) = child_input_args(
            Some(&[
                crate::ActionRunVar {
                    name: "year".to_string(),
                    value: crate::ActionRunVarValue::Variable("runtime.year".to_string()),
                },
                crate::ActionRunVar {
                    name: "generate_images".to_string(),
                    value: crate::ActionRunVarValue::Variable(
                        "runtime.generate_images".to_string(),
                    ),
                },
            ]),
            None,
            None,
            None,
            &json!({
                "runtime": {
                    "year": 2026,
                    "generate_images": true
                }
            }),
            "demo",
            &no_named_inputs(),
        )
        .expect("dynamic child run_vars should resolve");

        assert_eq!(
            args,
            vec![
                "--run-var",
                "year=2026",
                "--run-var",
                "generate_images=true",
            ]
        );
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("runtime var 'year'"));
        assert!(notes[1].contains("runtime var 'generate_images'"));
        assert!(!notes[0].contains("2026"));
        assert!(!notes[1].contains("true"));
    }

    #[test]
    fn child_input_args_reject_object_valued_run_var_variable() {
        let error = child_input_args(
            Some(&[crate::ActionRunVar {
                name: "year".to_string(),
                value: crate::ActionRunVarValue::Variable("runtime.year".to_string()),
            }]),
            None,
            None,
            None,
            &json!({
                "runtime": {
                    "year": { "bad": true }
                }
            }),
            "demo",
            &no_named_inputs(),
        )
        .unwrap_err();

        assert!(error.contains("runtime var 'year'"));
        assert!(error.contains("object-valued variable"));
    }

    #[test]
    fn action_completion_summary_uses_completed_for_clean_runs() {
        let summary = action_completion_summary(&[StepExecutionOutcome::Completed]);
        assert_eq!(summary, Some("completed"));
    }

    #[test]
    fn action_completion_summary_suppresses_duplicate_single_step_email_success() {
        let summary = action_completion_summary(&[StepExecutionOutcome::SuccessAlreadyPrinted]);
        assert_eq!(summary, None);
    }

    #[test]
    fn action_completion_summary_suppresses_final_success_after_soft_failure() {
        let summary = action_completion_summary(&[
            StepExecutionOutcome::Completed,
            StepExecutionOutcome::SoftFailureLogged,
        ]);
        assert_eq!(summary, None);
    }

    #[test]
    fn run_completion_message_for_depth_prints_for_root_runs_only() {
        assert_eq!(
            run_completion_message_for_depth(0, std::time::Duration::from_secs(32)),
            Some("Run complete · 32s total".to_string())
        );
        assert_eq!(
            run_completion_message_for_depth(1, std::time::Duration::from_secs(32)),
            None
        );
    }

    #[test]
    fn action_execution_header_uses_effective_mode() {
        assert_eq!(
            action_execution_header(crate::ActionExecutionMode::Sequential),
            "run: sequential"
        );
        assert_eq!(
            action_execution_header(crate::ActionExecutionMode::Parallel),
            "run: parallel"
        );
    }

    #[test]
    fn run_header_line_uses_elapsed_when_present() {
        assert_eq!(
            run_header_line(
                crate::ActionExecutionMode::Sequential,
                Some(std::time::Duration::from_millis(9_800))
            ),
            "run: sequential · 9.8s"
        );
    }

    #[test]
    fn format_elapsed_duration_is_millisecond_aware() {
        assert_eq!(
            format_elapsed_duration(std::time::Duration::from_millis(428)),
            "428ms"
        );
        assert_eq!(
            format_elapsed_duration(std::time::Duration::from_millis(1_500)),
            "1.5s"
        );
        assert_eq!(
            format_elapsed_duration(std::time::Duration::from_secs(17)),
            "17s"
        );
        assert_eq!(
            format_elapsed_duration(std::time::Duration::from_secs(72)),
            "1m 12s"
        );
    }

    #[test]
    fn action_lane_prefix_uses_json_order_and_name() {
        assert_eq!(
            action_lane_prefix(0, "generate_images"),
            "[Action 1: generate_images]"
        );
        assert_eq!(
            action_lane_prefix(2, "child_summary"),
            "[Action 3: child_summary]"
        );
    }

    #[test]
    fn live_dashboard_snapshot_shows_lane_state() {
        let output = ActionOutput::new_for_mode(
            crate::ActionExecutionMode::Parallel,
            ActionOutputMode::Live,
        );

        output.action_started(0, "generate_images");
        output.action_step_started(0, "generate_images", "generate_image", 1, 2);
        output.action_line(
            0,
            "generate_images",
            "wrote generated image to './artifacts/hero.png'.",
        );

        let snapshot = output.snapshot_lines_for_test();
        assert!(snapshot[0].starts_with("run: parallel · "));
        assert!(snapshot
            .iter()
            .any(|line| line.starts_with("[Action 1: generate_images] running · ")));
        assert!(snapshot
            .iter()
            .any(|line| line == "  step: 1/2 generate_image"));
        assert!(snapshot
            .iter()
            .any(|line| line == "  last: wrote generated image to './artifacts/hero.png'."));
        assert!(!snapshot.iter().any(|line| line == "  output:"));
    }

    #[test]
    fn live_dashboard_snapshot_shows_run_elapsed_before_actions_start() {
        let output = ActionOutput::new_for_mode_with_notice(
            crate::ActionExecutionMode::Sequential,
            ActionOutputMode::Live,
            None,
            Instant::now() - std::time::Duration::from_secs(4),
        );

        let snapshot = output.snapshot_lines_for_test();
        assert!(snapshot[0].starts_with("run: sequential · "));
        assert_ne!(snapshot[0], "run: sequential");
    }

    #[test]
    fn live_dashboard_snapshot_shows_waiting_message_for_long_running_step() {
        let output = ActionOutput::new_for_mode(
            crate::ActionExecutionMode::Parallel,
            ActionOutputMode::Live,
        );

        output.action_started(0, "generate_images");
        output.action_step_started(0, "generate_images", "generate_image", 2, 2);

        let snapshot = output.snapshot_lines_for_test();
        assert!(snapshot
            .iter()
            .any(|line| line.starts_with("[Action 1: generate_images] running · ")));
        assert!(snapshot
            .iter()
            .any(|line| line == "  step: 2/2 generate_image"));
        assert!(snapshot
            .iter()
            .any(|line| line == "  last: waiting for provider response..."));
    }

    #[test]
    fn live_dashboard_snapshot_marks_lane_completion_with_elapsed_time() {
        let output = ActionOutput::new_for_mode(
            crate::ActionExecutionMode::Sequential,
            ActionOutputMode::Live,
        );

        output.action_started(0, "generate_images");
        output.action_success(0, "generate_images", "completed");

        let snapshot = output.snapshot_lines_for_test();
        assert!(snapshot
            .iter()
            .any(|line| line.starts_with("[Action 1: generate_images] completed · ")));
        assert!(snapshot.iter().any(|line| line == "  step: ✓ done"));
        assert!(!snapshot.iter().any(|line| line == "  last: completed"));
        assert!(!snapshot.iter().any(|line| line == "  last: completed."));
    }

    #[test]
    fn live_dashboard_snapshot_marks_lane_failures() {
        let output = ActionOutput::new_for_mode(
            crate::ActionExecutionMode::Sequential,
            ActionOutputMode::Live,
        );

        output.action_started(1, "child_summary");
        output.action_failed(1, "child_summary", "child exited with status 1");

        let snapshot = output.snapshot_lines_for_test();
        assert!(snapshot
            .iter()
            .any(|line| line.starts_with("[Action 2: child_summary] failed · ")));
        assert!(snapshot.iter().any(|line| line == "  step: x failed"));
        assert!(snapshot
            .iter()
            .any(|line| line == "  last: failed: child exited with status 1"));
        assert!(!snapshot.iter().any(|line| line == "  output:"));
    }

    #[test]
    fn using_line_hides_standard_openai_account_url() {
        let provider_context = ActionProviderContext {
            project_data: None,
            provider: ProviderKind::OpenAi,
            profile_name: Some("codex_account".to_string()),
            auth_mode: "chatgpt_account".to_string(),
            model: "gpt-5.2".to_string(),
            url: openai_oauth::OPENAI_ACCOUNT_RESPONSES_URL.to_string(),
            token: "test-token".to_string(),
            inference_timeout_in_sec: 60,
            tool_resolver: None,
            package_context: None,
            usage_log: None,
        };

        assert_eq!(
            provider_context.using_line(),
            "using: profile=codex_account auth=chatgpt_account server=openai model=gpt-5.2"
        );
    }

    #[test]
    fn using_line_includes_custom_url_when_material() {
        let provider_context = ActionProviderContext {
            project_data: None,
            provider: ProviderKind::OpenAi,
            profile_name: None,
            auth_mode: "api_key".to_string(),
            model: "gpt-5.2".to_string(),
            url: "https://custom.example.test/v1/chat/completions".to_string(),
            token: "test-token".to_string(),
            inference_timeout_in_sec: 60,
            tool_resolver: None,
            package_context: None,
            usage_log: None,
        };

        assert_eq!(
            provider_context.using_line(),
            "using: profile=none auth=api_key server=openai model=gpt-5.2 url=https://custom.example.test/v1/chat/completions"
        );
    }

    #[test]
    fn live_dashboard_snapshot_suppresses_duplicate_using_lines() {
        let output = ActionOutput::new_for_mode(
            crate::ActionExecutionMode::Parallel,
            ActionOutputMode::Live,
        );
        let root_using = "using: profile=parent auth=api_key server=openai model=gpt-5.2";

        output.seed_using_line(root_using);
        output.action_started(0, "child_summary");
        output.action_step_started(0, "child_summary", "agent", 1, 1);
        output.action_using_line_if_changed(0, "child_summary", root_using);

        let snapshot = output.snapshot_lines_for_test();
        assert!(!snapshot.iter().any(|line| line.contains(root_using)));
    }

    #[test]
    fn live_dashboard_snapshot_suppresses_changed_using_lines() {
        let output = ActionOutput::new_for_mode(
            crate::ActionExecutionMode::Parallel,
            ActionOutputMode::Live,
        );
        let changed_using = "using: profile=child_profile auth=api_key server=openai model=gpt-5.2";

        output.seed_using_line("using: profile=parent auth=api_key server=openai model=gpt-5.2");
        output.action_started(0, "child_summary");
        output.action_step_started(0, "child_summary", "agent", 1, 1);
        output.action_using_line_if_changed(0, "child_summary", changed_using);

        let snapshot = output.snapshot_lines_for_test();
        assert!(snapshot
            .iter()
            .any(|line| line == "  last: waiting for child agent to finish..."));
        assert!(!snapshot.iter().any(|line| line.contains(changed_using)));
    }

    #[test]
    fn live_dashboard_snapshot_suppresses_dynamic_child_resolution_lines() {
        let output = ActionOutput::new_for_mode(
            crate::ActionExecutionMode::Parallel,
            ActionOutputMode::Live,
        );

        output.action_started(0, "child_summary");
        output.action_step_started(0, "child_summary", "agent", 1, 1);
        output.action_line(
            0,
            "child_summary",
            "Action 'child_summary' resolved dynamic child-agent runtime var 'year' -> 2026.",
        );

        let snapshot = output.snapshot_lines_for_test();
        assert!(snapshot
            .iter()
            .any(|line| line == "  last: waiting for child agent to finish..."));
        assert!(!snapshot
            .iter()
            .any(|line| line.contains("resolved dynamic child-agent runtime var")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_step_captures_output_variable_on_success() {
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "exec".to_string(),
            program: Some("/bin/sh".to_string()),
            model: None,
            profile: None,
            output_variable: Some("report_listing".to_string()),
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: vec![
                crate::RunArg::Literal("-lc".to_string()),
                crate::RunArg::Literal("printf 'alpha\\nbeta\\n'".to_string()),
            ],
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let captured_output =
            run_exec_step(&step, &json!({}), 0, "capture_exec", None, runtime_budget)
                .await
                .expect("exec capture should succeed");

        assert_eq!(
            captured_output,
            Some(("report_listing".to_string(), "alpha\nbeta".to_string()))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_step_buckets_raw_output_into_live_lane() {
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "exec".to_string(),
            program: Some("/bin/sh".to_string()),
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: vec![
                crate::RunArg::Literal("-lc".to_string()),
                crate::RunArg::Literal("printf 'alpha\\n'; printf 'beta\\n' >&2".to_string()),
            ],
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let output = ActionOutput::new_for_mode(
            crate::ActionExecutionMode::Parallel,
            ActionOutputMode::Live,
        );
        output.action_started(0, "raw_exec");
        output.action_step_started(0, "raw_exec", "exec", 1, 1);

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = ACTION_OUTPUT
            .scope(output.clone(), async {
                run_exec_step(&step, &json!({}), 0, "raw_exec", None, runtime_budget).await
            })
            .await;

        assert!(result.is_ok(), "raw exec step should succeed: {result:?}");
        output.action_success(0, "raw_exec", "completed");

        let snapshot = output.snapshot_lines_for_test();
        assert!(snapshot
            .iter()
            .any(|line| line.starts_with("[Action 1: raw_exec] completed · ")));
        assert!(snapshot.iter().any(|line| line == "  step: ✓ done"));
        assert!(!snapshot.iter().any(|line| line == "  last: completed"));
        assert!(!snapshot.iter().any(|line| line == "  last: completed."));
        assert!(!snapshot.iter().any(|line| line == "  output:"));
        assert!(!snapshot.iter().any(|line| line.contains("alpha")));
        assert!(!snapshot.iter().any(|line| line.contains("beta")));
    }

    #[tokio::test]
    async fn generate_image_step_writes_single_output_file() {
        use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

        let mut server = mockito::Server::new_async().await;
        let expected_bytes = b"fake-png";
        let encoded_image = BASE64_STANDARD.encode(expected_bytes);
        let _mock = server
            .mock("POST", "/v1/images/generations")
            .expect(2)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"data":[{{"b64_json":"{}"}}]}}"#,
                encoded_image
            ))
            .create_async()
            .await;

        let output_name = format!(".tmp-cai2054-generated-image-{}.png", std::process::id());
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: Some(crate::RunArg::Literal("gpt-image-1".to_string())),
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![
                crate::RunArg::Literal("Create an image for ".to_string()),
                crate::RunArg::Variable("customer".to_string()),
            ]),
            path: Some(vec![crate::RunArg::Literal(output_name.clone())]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let provider_context = ActionProviderContext {
            project_data: None,
            provider: ProviderKind::OpenAi,
            profile_name: Some("test_profile".to_string()),
            auth_mode: "api_key".to_string(),
            model: "gpt-5.2".to_string(),
            url: format!("{}/v1/chat/completions", server.url()),
            token: "test-token".to_string(),
            inference_timeout_in_sec: 60,
            tool_resolver: None,
            package_context: None,
            usage_log: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_generate_image_step(
            &step,
            &json!({ "customer": "Acme" }),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &provider_context,
            runtime_budget,
        )
        .await;

        assert!(
            result.is_ok(),
            "image generation should succeed: {result:?}"
        );

        let written_bytes =
            std::fs::read(&output_name).expect("generated image file should be written");
        let _ = std::fs::remove_file(&output_name);
        assert_eq!(written_bytes, expected_bytes);
        let project = std::env::temp_dir().join(format!("image-data-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(project.join(".cargo-ai")).unwrap();
        fs::write(
            project.join(".cargo-ai/project.toml"),
            "[runtime]\ndata_root = '.cargo-ai/data'",
        )
        .unwrap();
        let mut project_context = provider_context.clone();
        project_context.project_data =
            crate::commands::runtime_data::project_data_root(Some(&project)).unwrap();
        assert!(!project.join(".cargo-ai/data").exists());
        run_generate_image_step(
            &step,
            &json!({"customer":"Acme"}),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &project_context,
            runtime_budget,
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read(project.join(".cargo-ai/data").join(&output_name)).unwrap(),
            expected_bytes
        );
        assert!(!Path::new(&output_name).exists());
        _mock.assert_async().await;
        fs::remove_dir_all(project).unwrap();
    }

    #[tokio::test]
    async fn generate_image_step_sends_named_reference_images_to_openai_edits() {
        use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

        let mut server = mockito::Server::new_async().await;
        let expected_bytes = b"fake-png-edit";
        let encoded_image = BASE64_STANDARD.encode(expected_bytes);
        let _mock = server
            .mock("POST", "/v1/images/edits")
            .match_header(
                "content-type",
                mockito::Matcher::Regex("multipart/form-data; boundary=".into()),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"data":[{{"b64_json":"{}"}}]}}"#,
                encoded_image
            ))
            .create_async()
            .await;

        let reference_name = format!(".tmp-cai2097-reference-{}.png", std::process::id());
        std::fs::write(&reference_name, b"fake-reference")
            .expect("reference image fixture should write");
        let output_name = format!(
            ".tmp-cai2097-generated-image-edit-{}.png",
            std::process::id()
        );
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: Some(crate::RunArg::Literal("gpt-image-2".to_string())),
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image using the reference".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(output_name.clone())]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: Some(vec![crate::GenerateImageReference::Named {
                input: "front_photo".to_string(),
            }]),
            input_mode: None,
            platforms: None,
        };
        let named_inputs = std::collections::BTreeMap::from([(
            "front_photo".to_string(),
            crate::Input {
                name: Some("front_photo".to_string()),
                kind: crate::InputKind::Image,
                value: Some(reference_name.clone()),
            },
        )]);
        let provider_context = ActionProviderContext {
            project_data: None,
            provider: ProviderKind::OpenAi,
            profile_name: Some("test_profile".to_string()),
            auth_mode: "api_key".to_string(),
            model: "gpt-5.2".to_string(),
            url: format!("{}/v1/chat/completions", server.url()),
            token: "test-token".to_string(),
            inference_timeout_in_sec: 60,
            tool_resolver: None,
            package_context: None,
            usage_log: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_generate_image_step(
            &step,
            &json!({}),
            &named_inputs,
            0,
            "generate_art",
            1,
            &provider_context,
            runtime_budget,
        )
        .await;

        let _ = std::fs::remove_file(&reference_name);
        assert!(
            result.is_ok(),
            "reference-image generation should succeed: {result:?}"
        );

        let written_bytes =
            std::fs::read(&output_name).expect("generated image file should be written");
        let _ = std::fs::remove_file(&output_name);
        assert_eq!(written_bytes, expected_bytes);
    }

    #[tokio::test]
    async fn generate_image_step_rejects_missing_reference_image() {
        let missing_reference =
            format!(".tmp-cai2097-missing-reference-{}.png", std::process::id());
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: Some(crate::RunArg::Literal("gpt-image-2".to_string())),
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image using the reference".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(
                "./artifacts/reference-output.png".to_string(),
            )]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: Some(vec![crate::GenerateImageReference::Path {
                path: vec![crate::RunArg::Literal(missing_reference)],
            }]),
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let error = run_generate_image_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &provider_context(),
            runtime_budget,
        )
        .await
        .expect_err("missing reference image should fail before provider request");

        assert!(error.contains("reference image 1 could not be loaded"));
        assert!(error.contains("Failed to read reference image"));
    }

    #[tokio::test]
    async fn generate_image_step_resolves_model_from_runtime_variable() {
        use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

        let mut server = mockito::Server::new_async().await;
        let expected_bytes = b"fake-png-runtime";
        let encoded_image = BASE64_STANDARD.encode(expected_bytes);
        let _mock = server
            .mock("POST", "/v1/images/generations")
            .match_body(mockito::Matcher::PartialJson(
                json!({ "model": "gpt-image-1" }),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"data":[{{"b64_json":"{}"}}]}}"#,
                encoded_image
            ))
            .create_async()
            .await;

        let output_name = format!(
            ".tmp-cai2055-generated-image-runtime-{}.png",
            std::process::id()
        );
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: Some(crate::RunArg::Variable("runtime.image_model".to_string())),
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image for Acme".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(output_name.clone())]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let provider_context = ActionProviderContext {
            project_data: None,
            provider: ProviderKind::OpenAi,
            profile_name: Some("test_profile".to_string()),
            auth_mode: "api_key".to_string(),
            model: "gpt-5.2".to_string(),
            url: format!("{}/v1/chat/completions", server.url()),
            token: "test-token".to_string(),
            inference_timeout_in_sec: 60,
            tool_resolver: None,
            package_context: None,
            usage_log: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_generate_image_step(
            &step,
            &json!({
                "runtime": {
                    "image_model": "gpt-image-1"
                }
            }),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &provider_context,
            runtime_budget,
        )
        .await;

        assert!(
            result.is_ok(),
            "runtime-image generation should succeed: {result:?}"
        );

        let written_bytes =
            std::fs::read(&output_name).expect("generated image file should be written");
        let _ = std::fs::remove_file(&output_name);
        assert_eq!(written_bytes, expected_bytes);
    }

    #[tokio::test]
    async fn generate_image_step_falls_back_to_effective_invocation_model() {
        use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

        let mut server = mockito::Server::new_async().await;
        let expected_bytes = b"fake-png-fallback";
        let encoded_image = BASE64_STANDARD.encode(expected_bytes);
        let _mock = server
            .mock("POST", "/v1/images/generations")
            .match_body(mockito::Matcher::PartialJson(
                json!({ "model": "gpt-image-1.5" }),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"data":[{{"b64_json":"{}"}}]}}"#,
                encoded_image
            ))
            .create_async()
            .await;

        let output_name = format!(
            ".tmp-cai2067-generated-image-fallback-{}.png",
            std::process::id()
        );
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image for Acme".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(output_name.clone())]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let provider_context = ActionProviderContext {
            project_data: None,
            provider: ProviderKind::OpenAi,
            profile_name: Some("test_profile".to_string()),
            auth_mode: "api_key".to_string(),
            model: "gpt-image-1.5".to_string(),
            url: format!("{}/v1/chat/completions", server.url()),
            token: "test-token".to_string(),
            inference_timeout_in_sec: 60,
            tool_resolver: None,
            package_context: None,
            usage_log: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_generate_image_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &provider_context,
            runtime_budget,
        )
        .await;

        assert!(
            result.is_ok(),
            "fallback-image generation should succeed: {result:?}"
        );

        let written_bytes =
            std::fs::read(&output_name).expect("generated image file should be written");
        let _ = std::fs::remove_file(&output_name);
        assert_eq!(written_bytes, expected_bytes);
    }

    #[tokio::test]
    async fn generate_image_step_requires_model_when_step_and_invocation_omit_it() {
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image for Acme".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(
                "./artifacts/missing-model.png".to_string(),
            )]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let provider_context = ActionProviderContext {
            project_data: None,
            provider: ProviderKind::OpenAi,
            profile_name: Some("test_profile".to_string()),
            auth_mode: "api_key".to_string(),
            model: String::new(),
            url: "https://api.openai.com/v1/chat/completions".to_string(),
            token: "test-token".to_string(),
            inference_timeout_in_sec: 60,
            tool_resolver: None,
            package_context: None,
            usage_log: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let error = run_generate_image_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &provider_context,
            runtime_budget,
        )
        .await
        .expect_err("missing step and invocation model should fail");

        assert!(error.contains("omitted `model`"));
        assert!(error.contains("pass `--model`"));
    }

    #[tokio::test]
    async fn generate_image_step_uses_step_profile_model_when_explicit_model_omitted() {
        use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

        let mut server = mockito::Server::new_async().await;
        let expected_bytes = b"fake-png-step-profile";
        let encoded_image = BASE64_STANDARD.encode(expected_bytes);
        let _mock = server
            .mock("POST", "/v1/images/generations")
            .match_body(mockito::Matcher::PartialJson(
                json!({ "model": "gpt-image-step-profile" }),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"data":[{{"b64_json":"{}"}}]}}"#,
                encoded_image
            ))
            .create_async()
            .await;

        let config = profile_config(
            "image_profile",
            format!("{}/v1/chat/completions", server.url()).as_str(),
            "gpt-image-step-profile",
        );
        let _test_env = TestCargoHome::new(&config);

        let output_name = format!(
            ".tmp-cai2067-generated-image-step-profile-{}.png",
            std::process::id()
        );
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: None,
            profile: Some(crate::RunArg::Literal("image_profile".to_string())),
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image for Acme".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(output_name.clone())]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let output = ActionOutput::new_for_mode(
            crate::ActionExecutionMode::Sequential,
            ActionOutputMode::Live,
        );
        output.seed_using_line(provider_context().using_line_with_model("gpt-5.2").as_str());
        let result = ACTION_OUTPUT
            .scope(output.clone(), async {
                run_generate_image_step(
                    &step,
                    &json!({}),
                    &no_named_inputs(),
                    0,
                    "generate_art",
                    1,
                    &provider_context(),
                    runtime_budget,
                )
                .await
            })
            .await;

        assert!(
            result.is_ok(),
            "profile-backed image generation should succeed: {result:?}"
        );

        let written_bytes =
            std::fs::read(&output_name).expect("generated image file should be written");
        let _ = std::fs::remove_file(&output_name);
        assert_eq!(written_bytes, expected_bytes);

        let snapshot = output.snapshot_lines_for_test();
        assert!(!snapshot
            .iter()
            .any(|line| line.contains("using: profile=image_profile")));
        assert!(!snapshot
            .iter()
            .any(|line| line.contains("url=http://127.0.0.1")));
    }

    #[tokio::test]
    async fn generate_image_step_profile_inherits_invocation_timeout() {
        let config = profile_config(
            "image_profile",
            "https://api.openai.com/v1/chat/completions",
            "gpt-image-step-profile",
        );
        let _test_env = TestCargoHome::new(&config);

        let context = resolve_generate_image_step_profile_context(
            Some(&crate::RunArg::Literal("image_profile".to_string())),
            &json!({}),
            "generate_art",
            180,
        )
        .await
        .expect("profile lookup should succeed")
        .expect("profile context should resolve");

        assert_eq!(context.inference_timeout_in_sec, 180);
        assert_eq!(context.model, "gpt-image-step-profile");
    }

    #[tokio::test]
    async fn generate_image_step_explicit_model_overrides_step_profile_model() {
        use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

        let mut server = mockito::Server::new_async().await;
        let expected_bytes = b"fake-png-explicit-step-model";
        let encoded_image = BASE64_STANDARD.encode(expected_bytes);
        let _mock = server
            .mock("POST", "/v1/images/generations")
            .match_body(mockito::Matcher::PartialJson(
                json!({ "model": "gpt-image-explicit" }),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!(
                r#"{{"data":[{{"b64_json":"{}"}}]}}"#,
                encoded_image
            ))
            .create_async()
            .await;

        let config = profile_config(
            "image_profile",
            format!("{}/v1/chat/completions", server.url()).as_str(),
            "gpt-image-step-profile",
        );
        let _test_env = TestCargoHome::new(&config);

        let output_name = format!(
            ".tmp-cai2067-generated-image-step-explicit-{}.png",
            std::process::id()
        );
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: Some(crate::RunArg::Literal("gpt-image-explicit".to_string())),
            profile: Some(crate::RunArg::Literal("image_profile".to_string())),
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image for Acme".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(output_name.clone())]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_generate_image_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &provider_context(),
            runtime_budget,
        )
        .await;

        assert!(
            result.is_ok(),
            "explicit-model image generation should succeed: {result:?}"
        );

        let written_bytes =
            std::fs::read(&output_name).expect("generated image file should be written");
        let _ = std::fs::remove_file(&output_name);
        assert_eq!(written_bytes, expected_bytes);
    }

    #[tokio::test]
    async fn generate_image_step_rejects_unknown_step_profile() {
        let config = profile_config(
            "other_profile",
            "https://api.openai.com/v1/chat/completions",
            "gpt-image-step-profile",
        );
        let _test_env = TestCargoHome::new(&config);

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: Some(crate::RunArg::Literal("gpt-image-explicit".to_string())),
            profile: Some(crate::RunArg::Literal("missing_profile".to_string())),
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image for Acme".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(
                "./artifacts/missing-profile.png".to_string(),
            )]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let error = run_generate_image_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &provider_context(),
            runtime_budget,
        )
        .await
        .expect_err("missing profile should fail");

        assert!(error.contains("unknown profile 'missing_profile'"));
    }

    #[tokio::test]
    async fn generate_image_step_rejects_reference_images_for_ollama() {
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image for Acme".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(
                "./artifacts/generated.png".to_string(),
            )]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: Some(vec![crate::GenerateImageReference::Path {
                path: vec![crate::RunArg::Literal("./reference.png".to_string())],
            }]),
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let error = run_generate_image_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &ollama_provider_context(
                "http://localhost:11434/v1/chat/completions",
                "x/flux2-klein:4b",
            ),
            runtime_budget,
        )
        .await
        .expect_err("ollama reference-image generation should fail clearly");

        assert!(error.contains("reference_images are not supported by provider 'ollama'"));
        assert!(error.contains("use an OpenAI image profile"));
    }

    #[tokio::test]
    async fn generate_image_step_supports_direct_ollama_provider_context() {
        let mut server = mockito::Server::new_async().await;
        let expected_bytes = b"fake-ollama-png";
        let _mock = server
            .mock("POST", "/v1/images/generations")
            .match_body(mockito::Matcher::PartialJson(json!({
                "model": "x/flux2-klein:4b",
                "prompt": "Create an image for Acme",
                "n": 1,
                "size": "1024x1024",
                "response_format": "b64_json"
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(ollama_image_response_bytes(expected_bytes))
            .create_async()
            .await;

        let output_name = format!(
            ".tmp-cai2057-generated-image-ollama-direct-{}.png",
            std::process::id()
        );
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image for Acme".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(output_name.clone())]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_generate_image_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &ollama_provider_context(
                format!("{}/v1/chat/completions", server.url()).as_str(),
                "x/flux2-klein:4b",
            ),
            runtime_budget,
        )
        .await;

        assert!(
            result.is_ok(),
            "direct ollama image generation should succeed: {result:?}"
        );

        let written_bytes =
            std::fs::read(&output_name).expect("generated image file should be written");
        let _ = std::fs::remove_file(&output_name);
        assert_eq!(written_bytes, expected_bytes);
    }

    #[tokio::test]
    async fn generate_image_step_supports_ollama_step_profile_from_openai_parent() {
        let mut server = mockito::Server::new_async().await;
        let expected_bytes = b"fake-ollama-profile-png";
        let _mock = server
            .mock("POST", "/v1/images/generations")
            .match_body(mockito::Matcher::PartialJson(json!({
                "model": "x/flux2-klein:4b",
                "prompt": "Create an image for Acme",
                "n": 1,
                "size": "1024x1024",
                "response_format": "b64_json"
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(ollama_image_response_bytes(expected_bytes))
            .create_async()
            .await;

        let config = ollama_profile_config(
            "ollama_images",
            format!("{}/v1/chat/completions", server.url()).as_str(),
            "x/flux2-klein:4b",
        );
        let _test_env = TestCargoHome::new(&config);

        let output_name = format!(
            ".tmp-cai2057-generated-image-ollama-profile-{}.png",
            std::process::id()
        );
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: None,
            profile: Some(crate::RunArg::Literal("ollama_images".to_string())),
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image for Acme".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(output_name.clone())]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_generate_image_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &provider_context(),
            runtime_budget,
        )
        .await;

        assert!(
            result.is_ok(),
            "mixed-provider ollama image generation should succeed: {result:?}"
        );

        let written_bytes =
            std::fs::read(&output_name).expect("generated image file should be written");
        let _ = std::fs::remove_file(&output_name);
        assert_eq!(written_bytes, expected_bytes);
    }

    #[tokio::test]
    async fn generate_image_step_rejects_non_png_output_for_ollama() {
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "generate_image".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: Some(vec![crate::RunArg::Literal(
                "Create an image for Acme".to_string(),
            )]),
            path: Some(vec![crate::RunArg::Literal(
                "./artifacts/generated.webp".to_string(),
            )]),
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let error = run_generate_image_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "generate_art",
            1,
            &ollama_provider_context(
                "http://localhost:11434/v1/chat/completions",
                "x/flux2-klein:4b",
            ),
            runtime_budget,
        )
        .await
        .expect_err("non-png ollama image generation should fail");

        assert!(error.contains("requires a `.png` output path"));
        assert!(error.contains("Ollama"));
    }

    #[test]
    fn child_usage_log_resolves_under_package_data_root() {
        let data_root =
            std::env::temp_dir().join(format!("cai2102-package-data-{}", std::process::id()));
        let context = crate::commands::local_packages::InstalledPackageRuntimeContext {
            alias: "image_generator".to_string(),
            source_kind: "hosted".to_string(),
            package_payload_root: data_root
                .parent()
                .expect("data root should have a parent")
                .join("package"),
            package_data_root: data_root.clone(),
            current_entrypoint_path: Some("agents/observer.json".to_string()),
            entrypoints: Vec::new(),
            permissions: crate::commands::local_packages::PackagePermissionProfileDocument::default(
            ),
        };

        let resolved = super::resolve_child_usage_log_path(
            "usage/child.jsonl",
            "observe_usage",
            Some(&context),
        )
        .expect("package child usage log should resolve");

        assert_eq!(resolved, data_root.join("usage/child.jsonl"));
    }

    #[test]
    fn child_usage_log_rejects_parent_traversal() {
        let error = super::resolve_child_usage_log_path("../usage.jsonl", "observe_usage", None)
            .expect_err("parent traversal should be rejected");

        assert!(error.contains("parent traversal"));
    }

    #[test]
    fn child_usage_log_rejects_windows_drive_relative_path() {
        let error = super::resolve_child_usage_log_path("C:usage.jsonl", "observe_usage", None)
            .expect_err("Windows drive-relative path should be rejected on every platform");

        assert!(error.contains("drive-relative") || error.contains("prefix"));
    }

    #[test]
    fn hosted_child_static_and_dynamic_paths_use_payload_and_data_roots() {
        let install_root = std::env::temp_dir().join(format!(
            "cai2102-hosted-child-inputs-{}",
            uuid::Uuid::new_v4()
        ));
        let context =
            hosted_package_context(install_root.as_path(), "blocked_without_explicit_grant");
        fs::create_dir_all(context.package_payload_root.join("assets"))
            .expect("payload assets should exist");
        fs::write(
            context.package_payload_root.join("assets/reference.png"),
            "image",
        )
        .expect("payload reference should exist");

        let static_path = resolve_installed_child_input_path(
            "assets/reference.png",
            false,
            "invoke_child",
            1,
            "image",
            Some(&context),
        )
        .expect("static package path should resolve from payload");
        assert_eq!(
            PathBuf::from(static_path),
            context.package_payload_root.join("assets/reference.png")
        );

        let dynamic_path = resolve_installed_child_input_path(
            "images/generated.png",
            true,
            "invoke_child",
            1,
            "image",
            Some(&context),
        )
        .expect("dynamic package path should resolve from data");
        assert_eq!(
            PathBuf::from(dynamic_path),
            context.package_data_root.join("images/generated.png")
        );

        let _ = fs::remove_dir_all(install_root);
    }

    #[test]
    fn local_installed_child_static_and_dynamic_paths_use_payload_and_data_roots() {
        let install_root = std::env::temp_dir().join(format!(
            "local-installed-child-inputs-{}",
            uuid::Uuid::new_v4()
        ));
        let context = local_package_context(install_root.as_path());
        fs::create_dir_all(context.package_payload_root.join("assets"))
            .expect("payload assets should exist");
        fs::write(
            context.package_payload_root.join("assets/reference.png"),
            "image",
        )
        .expect("payload reference should exist");

        let static_path = resolve_installed_child_input_path(
            "assets/reference.png",
            false,
            "invoke_child",
            1,
            "image",
            Some(&context),
        )
        .expect("static package path should resolve from payload");
        assert_eq!(
            PathBuf::from(static_path),
            context.package_payload_root.join("assets/reference.png")
        );

        let dynamic_path = resolve_installed_child_input_path(
            "images/generated.png",
            true,
            "invoke_child",
            1,
            "image",
            Some(&context),
        )
        .expect("dynamic package path should resolve from data");
        assert_eq!(
            PathBuf::from(dynamic_path),
            context.package_data_root.join("images/generated.png")
        );

        let _ = fs::remove_dir_all(install_root);
    }

    #[test]
    fn local_installed_reference_paths_use_payload_and_data_roots() {
        let install_root = std::env::temp_dir().join(format!(
            "local-installed-reference-inputs-{}",
            uuid::Uuid::new_v4()
        ));
        let context = local_package_context(install_root.as_path());
        let static_path = context
            .package_payload_root
            .join("assets")
            .join("reference.png");
        let dynamic_path = context
            .package_data_root
            .join("images")
            .join("generated.png");
        fs::create_dir_all(
            static_path
                .parent()
                .expect("static reference should have a parent"),
        )
        .expect("payload assets should exist");
        fs::create_dir_all(
            dynamic_path
                .parent()
                .expect("dynamic reference should have a parent"),
        )
        .expect("package data images should exist");
        fs::write(&static_path, "static image").expect("static reference should exist");
        fs::write(&dynamic_path, "dynamic image").expect("dynamic reference should exist");

        let references = vec![
            crate::GenerateImageReference::Path {
                path: vec![crate::RunArg::Literal("assets/reference.png".to_string())],
            },
            crate::GenerateImageReference::Path {
                path: vec![crate::RunArg::Variable("generated_path".to_string())],
            },
        ];
        let resolved = resolve_generate_image_reference_images(
            Some(&references),
            &json!({ "generated_path": "images/generated.png" }),
            "generate_art",
            &no_named_inputs(),
            Some(&context),
        )
        .expect("installed reference paths should resolve from package storage");

        assert_eq!(PathBuf::from(&resolved[0].source), static_path);
        assert_eq!(PathBuf::from(&resolved[1].source), dynamic_path);

        let _ = fs::remove_dir_all(install_root);
    }

    #[cfg(unix)]
    #[test]
    fn hosted_json_child_resolves_through_declared_alias_export() {
        let test_path = TestPathCommands::new();
        test_path.write_command("cargo", "#!/bin/sh\nexit 0\n");
        test_path.write_command("cargo-ai", "#!/bin/sh\nexit 0\n");
        let install_root = std::env::temp_dir().join(format!(
            "cai2102-hosted-child-export-{}",
            uuid::Uuid::new_v4()
        ));
        let context =
            hosted_package_context(install_root.as_path(), "blocked_without_explicit_grant");
        fs::write(
            context.package_payload_root.join("agents/observer.json"),
            "{}",
        )
        .expect("observer should exist");
        fs::write(context.package_payload_root.join("agents/child.json"), "{}")
            .expect("child should exist");

        let invocation =
            resolve_child_artifact_invocation("./child.json", "invoke_child", Some(&context))
                .expect("declared hosted child should resolve");
        match invocation {
            ChildArtifactInvocation::CargoSubcommand(reference) => {
                assert_eq!(reference, "image_generator::child");
            }
            _ => panic!("hosted JSON child should use the cargo subcommand"),
        }

        let _ = fs::remove_dir_all(install_root);
    }

    #[cfg(unix)]
    #[test]
    fn local_installed_json_child_resolves_through_declared_alias_export() {
        let test_path = TestPathCommands::new();
        test_path.write_command("cargo", "#!/bin/sh\nexit 0\n");
        test_path.write_command("cargo-ai", "#!/bin/sh\nexit 0\n");
        let install_root = std::env::temp_dir().join(format!(
            "local-installed-child-export-{}",
            uuid::Uuid::new_v4()
        ));
        let context = local_package_context(install_root.as_path());
        fs::write(
            context.package_payload_root.join("agents/observer.json"),
            "{}",
        )
        .expect("observer should exist");
        fs::write(context.package_payload_root.join("agents/child.json"), "{}")
            .expect("child should exist");

        let invocation =
            resolve_child_artifact_invocation("./child.json", "invoke_child", Some(&context))
                .expect("declared local installed child should resolve");
        match invocation {
            ChildArtifactInvocation::CargoSubcommand(reference) => {
                assert_eq!(reference, "image_generator::child");
            }
            _ => panic!("local installed JSON child should use the cargo subcommand"),
        }

        let _ = fs::remove_dir_all(install_root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_installed_json_child_runs_from_package_data_root() {
        let test_path = TestPathCommands::new();
        test_path.write_command("cargo", "#!/bin/sh\npwd > child-cwd.txt\n");
        test_path.write_command("cargo-ai", "#!/bin/sh\nexit 0\n");
        let install_root = std::env::temp_dir().join(format!(
            "local-installed-child-cwd-{}",
            uuid::Uuid::new_v4()
        ));
        let context = local_package_context(install_root.as_path());
        fs::write(
            context.package_payload_root.join("agents/observer.json"),
            "{}",
        )
        .expect("observer should exist");
        fs::write(context.package_payload_root.join("agents/child.json"), "{}")
            .expect("child should exist");

        let mut provider = provider_context();
        provider.profile_name = None;
        provider.package_context = Some(context.clone());
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some("./child.json".to_string()),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let result = run_agent_step_with_provider_context(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "invoke_child",
            1,
            &provider,
            None,
            5,
            configured_agent_action_runtime_budget(Some(600)),
        )
        .await;
        assert!(
            result.is_ok(),
            "local installed child should run: {result:?}"
        );
        let recorded = fs::read_to_string(context.package_data_root.join("child-cwd.txt"))
            .expect("child should record its working directory");
        assert_eq!(
            PathBuf::from(recorded.trim()),
            fs::canonicalize(&context.package_data_root)
                .expect("package data root should canonicalize")
        );

        let _ = fs::remove_dir_all(install_root);
    }

    #[cfg(unix)]
    #[test]
    fn hosted_json_child_rejects_undeclared_payload_file() {
        let test_path = TestPathCommands::new();
        test_path.write_command("cargo", "#!/bin/sh\nexit 0\n");
        test_path.write_command("cargo-ai", "#!/bin/sh\nexit 0\n");
        let install_root = std::env::temp_dir().join(format!(
            "cai2102-hosted-child-private-{}",
            uuid::Uuid::new_v4()
        ));
        let context =
            hosted_package_context(install_root.as_path(), "blocked_without_explicit_grant");
        fs::write(
            context.package_payload_root.join("agents/private.json"),
            "{}",
        )
        .expect("private child should exist");

        let error =
            resolve_child_artifact_invocation("./private.json", "invoke_child", Some(&context))
                .expect_err("undeclared hosted child should be rejected");
        assert!(error.contains("declared runnable export"));

        let _ = fs::remove_dir_all(install_root);
    }

    #[cfg(unix)]
    #[test]
    fn local_installed_json_child_rejects_undeclared_payload_file() {
        let test_path = TestPathCommands::new();
        test_path.write_command("cargo", "#!/bin/sh\nexit 0\n");
        test_path.write_command("cargo-ai", "#!/bin/sh\nexit 0\n");
        let install_root = std::env::temp_dir().join(format!(
            "local-installed-child-private-{}",
            uuid::Uuid::new_v4()
        ));
        let context = local_package_context(install_root.as_path());
        fs::write(
            context.package_payload_root.join("agents/private.json"),
            "{}",
        )
        .expect("private child should exist");

        let error =
            resolve_child_artifact_invocation("./private.json", "invoke_child", Some(&context))
                .expect_err("undeclared local installed child should be rejected");
        assert!(error.contains("declared runnable export"));

        let _ = fs::remove_dir_all(install_root);
    }

    #[test]
    fn hosted_direct_child_requires_accepted_subprocess_permission() {
        let install_root = std::env::temp_dir().join(format!(
            "cai2102-hosted-child-exec-{}",
            uuid::Uuid::new_v4()
        ));
        let blocked =
            hosted_package_context(install_root.as_path(), "blocked_without_explicit_grant");
        fs::write(
            blocked.package_payload_root.join("agents/child_exec"),
            "binary",
        )
        .expect("child executable fixture should exist");

        let error =
            resolve_child_artifact_invocation("./child_exec", "invoke_child", Some(&blocked))
                .expect_err("direct hosted child should be blocked");
        assert!(error.contains("explicitly accepted subprocess permission"));

        let allowed = hosted_package_context(install_root.as_path(), "allowed");
        let invocation =
            resolve_child_artifact_invocation("./child_exec", "invoke_child", Some(&allowed))
                .expect("accepted direct hosted child should resolve");
        match invocation {
            ChildArtifactInvocation::DirectExecutable(path) => {
                assert_eq!(path, allowed.package_payload_root.join("agents/child_exec"));
            }
            _ => panic!("direct hosted child should resolve to its verified payload path"),
        }

        let _ = fs::remove_dir_all(install_root);
    }

    #[test]
    fn hosted_cross_package_child_checks_subprocess_permission_before_resolution() {
        let install_root = std::env::temp_dir().join(format!(
            "hosted-package-dependency-permission-{}",
            uuid::Uuid::new_v4()
        ));
        let blocked =
            hosted_package_context(install_root.as_path(), "blocked_without_explicit_grant");

        let error = resolve_child_artifact_invocation(
            "reports::daily",
            "invoke_dependency",
            Some(&blocked),
        )
        .expect_err("hosted package dependency should not bypass subprocess permission");
        assert!(error.contains("explicitly accepted subprocess permission"));
        let _ = fs::remove_dir_all(install_root);
    }

    #[test]
    fn package_child_uses_declaring_package_payload_as_project_root() {
        let install_root = std::env::temp_dir().join(format!(
            "hosted-package-dependency-root-{}",
            uuid::Uuid::new_v4()
        ));
        let context = hosted_package_context(install_root.as_path(), "allowed");

        let project_root = super::package_child_project_root(
            Some(&context),
            "invoke_dependency",
            "reports::daily",
        )
        .expect("installed package context should supply its declaring project root");
        assert_eq!(project_root, context.package_payload_root);
        let _ = fs::remove_dir_all(install_root);
    }

    #[test]
    fn package_child_fails_closed_without_declaring_project_context() {
        let arbitrary_root = std::env::temp_dir().join(format!(
            "package-dependency-arbitrary-cwd-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&arbitrary_root).expect("arbitrary directory should exist");

        let error = super::package_child_project_root_from(
            None,
            None,
            Some(arbitrary_root.as_path()),
            "invoke_dependency",
            "reports::daily",
        )
        .expect_err("package child must not resolve without declaring project context");
        assert!(error.contains("[package_dependencies.<alias>]"));
        let _ = fs::remove_dir_all(arbitrary_root);
    }

    #[test]
    fn package_child_prefers_absolute_definition_project_over_process_cwd() {
        let project_a = std::env::temp_dir().join(format!(
            "declaring-project-definition-root-{}",
            uuid::Uuid::new_v4()
        ));
        let project_b = std::env::temp_dir().join(format!(
            "declaring-project-process-cwd-{}",
            uuid::Uuid::new_v4()
        ));
        for root in [&project_a, &project_b] {
            fs::create_dir_all(root.join(".cargo-ai")).expect("project metadata dir should exist");
            fs::write(root.join(".cargo-ai/project.toml"), "format_version = 1\n")
                .expect("project metadata should exist");
        }

        let selected = super::package_child_project_root_from(
            None,
            Some(project_a.as_path()),
            Some(project_b.as_path()),
            "invoke_dependency",
            "reports::daily",
        )
        .expect("declaring project should be selected before process cwd");
        assert_eq!(selected, project_a);

        let _ = fs::remove_dir_all(project_a);
        let _ = fs::remove_dir_all(project_b);
    }

    #[tokio::test]
    async fn declaring_project_scope_can_be_propagated_into_spawned_tasks() {
        let project_root = std::env::temp_dir().join(format!(
            "declaring-project-task-local-{}",
            uuid::Uuid::new_v4()
        ));
        let expected = project_root.clone();

        let observed = super::scope_declaring_project_root(Some(project_root), async move {
            let captured = super::current_declaring_project_root();
            tokio::spawn(async move {
                super::scope_declaring_project_root(captured, async {
                    super::current_declaring_project_root()
                })
                .await
            })
            .await
            .expect("spawned task should complete")
        })
        .await;

        assert_eq!(observed.as_deref(), Some(expected.as_path()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn accepted_hosted_direct_child_runs_from_package_data_root() {
        use std::os::unix::fs::PermissionsExt;

        let install_root =
            std::env::temp_dir().join(format!("cai2102-hosted-child-cwd-{}", uuid::Uuid::new_v4()));
        let context = hosted_package_context(install_root.as_path(), "allowed");
        let child_path = context.package_payload_root.join("agents/child_exec");
        fs::write(&child_path, "#!/bin/sh\npwd > child-cwd.txt\n")
            .expect("child script should exist");
        let mut permissions = fs::metadata(&child_path)
            .expect("child script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&child_path, permissions).expect("child script should be executable");

        let mut provider = provider_context();
        provider.package_context = Some(context.clone());
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some("./child_exec".to_string()),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let result = run_agent_step_with_provider_context(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "invoke_child",
            1,
            &provider,
            None,
            5,
            configured_agent_action_runtime_budget(Some(600)),
        )
        .await;
        assert!(result.is_ok(), "accepted child should run: {result:?}");
        let recorded = fs::read_to_string(context.package_data_root.join("child-cwd.txt"))
            .expect("child should record its working directory");
        assert_eq!(
            PathBuf::from(recorded.trim()),
            fs::canonicalize(&context.package_data_root)
                .expect("package data root should canonicalize")
        );

        let _ = fs::remove_dir_all(install_root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_step_invokes_child_with_forwarded_inputs() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(".tmp-cai2032-agent-child-{}.sh", std::process::id());
        let script_path = current_dir.join(&script_name);
        let usage_log_dir_name = format!(".tmp-cai2032-usage-{}", std::process::id());
        let usage_log = format!("{usage_log_dir_name}/child.jsonl");
        let usage_log_dir = current_dir.join(&usage_log_dir_name);
        let output_path = std::env::temp_dir().join(format!(
            "cai2032-agent-child-args-{}.txt",
            std::process::id()
        ));

        let script_body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
            output_path.display()
        );

        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", script_name)),
            usage_log: Some(usage_log.clone()),
            run_vars: None,
            input_overrides: None,
            inputs: Some(vec![
                crate::ActionInput::Text {
                    text: vec![
                        crate::RunArg::Literal("hello ".to_string()),
                        crate::RunArg::Variable("customer".to_string()),
                    ],
                },
                crate::ActionInput::Url {
                    url: vec![crate::RunArg::Literal("https://example.com".to_string())],
                },
                crate::ActionInput::Image {
                    path: vec![crate::RunArg::Literal("./diagram.png".to_string())],
                },
                crate::ActionInput::File {
                    path: vec![
                        crate::RunArg::Literal("./reports/".to_string()),
                        crate::RunArg::Variable("report_filename".to_string()),
                    ],
                },
            ]),
            reference_images: None,
            input_mode: Some(crate::ActionInputMode::Append),
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_agent_step(
            &step,
            &json!({
                "customer": "world",
                "report_filename": "report.pdf"
            }),
            &no_named_inputs(),
            0,
            "invoke_child",
            None,
            5,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&script_path);
        let _ = fs::remove_dir_all(&usage_log_dir);

        assert!(
            result.is_ok(),
            "child agent invocation should succeed: {result:?}"
        );

        let args = fs::read_to_string(&output_path).expect("child output should be captured");
        let _ = fs::remove_file(&output_path);

        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "--usage-log",
                usage_log.as_str(),
                "--input-mode",
                "append",
                "--input-text",
                "hello world",
                "--input-url",
                "https://example.com",
                "--input-image",
                "./diagram.png",
                "--input-file",
                "./reports/report.pdf",
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_step_summarizes_child_output_without_inlining_child_transcript() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let unique = uuid::Uuid::new_v4();
        let script_name = format!(".tmp-cai2067-child-summary-{unique}.sh");
        let script_path = current_dir.join(&script_name);
        let marker_path =
            std::env::temp_dir().join(format!("cai2067-child-summary-marker-{unique}.txt"));

        let script_body = format!(
            "#!/bin/sh\nprintf 'using: profile=child_profile auth=api_key server=openai model=gpt-5.2\\n'\nprintf 'child detail\\n'\nprintf 'ran' > \"{}\"\n",
            marker_path.display()
        );
        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", script_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let output = ActionOutput::new_for_mode(
            crate::ActionExecutionMode::Parallel,
            ActionOutputMode::Live,
        );
        output.action_started(0, "child_summary");
        output.action_step_started(0, "child_summary", "agent", 1, 1);

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = ACTION_OUTPUT
            .scope(output.clone(), async {
                run_agent_step(
                    &step,
                    &json!({}),
                    &no_named_inputs(),
                    0,
                    "child_summary",
                    None,
                    5,
                    runtime_budget,
                )
                .await
            })
            .await;

        if let Err(error) = result {
            let _ = fs::remove_file(&script_path);
            let _ = fs::remove_file(&marker_path);
            panic!("child summary step should succeed without passthrough: {error}");
        }

        let _ = fs::remove_file(&script_path);
        let marker =
            fs::read_to_string(&marker_path).expect("child script should still execute normally");
        let _ = fs::remove_file(&marker_path);
        assert_eq!(marker, "ran");

        output.action_success(0, "child_summary", "completed");

        let snapshot = output.snapshot_lines_for_test();
        assert!(snapshot
            .iter()
            .any(|line| line.starts_with("[Action 1: child_summary] completed · ")));
        assert!(snapshot.iter().any(|line| line == "  step: ✓ done"));
        assert!(!snapshot.iter().any(|line| line == "  last: completed"));
        assert!(!snapshot.iter().any(|line| line == "  last: completed."));
        assert!(!snapshot.iter().any(|line| line == "  output:"));
        assert!(!snapshot
            .iter()
            .any(|line| line.contains("using: profile=child_profile")));
        assert!(!snapshot.iter().any(|line| line.contains("child: started")));
        assert!(!snapshot
            .iter()
            .any(|line| line.contains("child: completed successfully")));
        assert!(!snapshot.iter().any(|line| line.contains("child detail")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_step_forwards_action_execution_override_to_child() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(
            ".tmp-cai2067-action-execution-child-{}.sh",
            std::process::id()
        );
        let script_path = current_dir.join(&script_name);
        let output_path = std::env::temp_dir().join(format!(
            "cai2067-action-execution-child-args-{}.txt",
            std::process::id()
        ));

        let script_body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
            output_path.display()
        );

        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", script_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_agent_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "invoke_child",
            Some(crate::ActionExecutionMode::Sequential),
            5,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&script_path);

        assert!(
            result.is_ok(),
            "child agent invocation should succeed: {result:?}"
        );

        let args = fs::read_to_string(&output_path).expect("child output should be captured");
        let _ = fs::remove_file(&output_path);

        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec!["--action-execution", "sequential"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_step_forwards_step_profile_to_child() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let config = profile_config(
            "child_profile",
            "https://api.openai.com/v1/chat/completions",
            "gpt-5.2",
        );
        let _test_env = TestCargoHome::new(&config);

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(".tmp-cai2067-child-profile-{}.sh", std::process::id());
        let script_path = current_dir.join(&script_name);
        let output_path = std::env::temp_dir().join(format!(
            "cai2067-child-profile-args-{}.txt",
            std::process::id()
        ));

        let script_body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
            output_path.display()
        );

        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: Some(crate::RunArg::Variable("runtime.child_profile".to_string())),
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", script_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_agent_step(
            &step,
            &json!({
                "runtime": {
                    "child_profile": "child_profile"
                }
            }),
            &no_named_inputs(),
            0,
            "invoke_child",
            Some(crate::ActionExecutionMode::Sequential),
            5,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&script_path);

        assert!(
            result.is_ok(),
            "child agent invocation should succeed: {result:?}"
        );

        let args = fs::read_to_string(&output_path).expect("child output should be captured");
        let _ = fs::remove_file(&output_path);

        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "--action-execution",
                "sequential",
                "--profile",
                "child_profile",
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn captured_exec_output_can_flow_into_later_agent_step() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(".tmp-cai2036-phase5-child-{}.sh", std::process::id());
        let script_path = current_dir.join(&script_name);
        let output_path = std::env::temp_dir().join(format!(
            "cai2036-phase5-child-args-{}.txt",
            std::process::id()
        ));

        let script_body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
            output_path.display()
        );

        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let exec_step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "exec".to_string(),
            program: Some("/bin/sh".to_string()),
            model: None,
            profile: None,
            output_variable: Some("report_listing".to_string()),
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: vec![
                crate::RunArg::Literal("-lc".to_string()),
                crate::RunArg::Literal("printf 'q1.pdf | q2.pdf\\n'".to_string()),
            ],
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let agent_step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", script_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: Some(vec![crate::ActionInput::Text {
                text: vec![
                    crate::RunArg::Literal("Files:\n".to_string()),
                    crate::RunArg::Variable("report_listing".to_string()),
                ],
            }]),
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let mut action_data = json!({});
        let captured_output = run_exec_step(
            &exec_step,
            &action_data,
            0,
            "capture_then_agent",
            None,
            runtime_budget,
        )
        .await
        .expect("exec capture should succeed");
        let (name, value) = captured_output.expect("captured output should be present");
        insert_action_output_variable(&mut action_data, name.as_str(), value, "capture_then_agent")
            .expect("captured output should be inserted");

        let result = run_agent_step(
            &agent_step,
            &action_data,
            &no_named_inputs(),
            0,
            "capture_then_agent",
            None,
            5,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&script_path);

        assert!(
            result.is_ok(),
            "child agent invocation should succeed: {result:?}"
        );

        let args = fs::read_to_string(&output_path).expect("child output should be captured");
        let _ = fs::remove_file(&output_path);

        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec!["--input-text", "Files:", "q1.pdf | q2.pdf"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_step_inherits_max_depth_and_runtime_budget_for_child_processes() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(".tmp-cai2032-agent-depth-child-{}.sh", std::process::id());
        let script_path = current_dir.join(&script_name);
        let output_path =
            std::env::temp_dir().join(format!("cai2032-agent-depth-{}.txt", std::process::id()));

        let script_body = format!(
            "#!/bin/sh\nprintf '%s\\n%s' \"$CARGO_AI_AGENT_ACTION_MAX_DEPTH\" \"$CARGO_AI_AGENT_MAX_RUNTIME_SECS\" > \"{}\"\n",
            output_path.display()
        );

        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", script_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_agent_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "invoke_child",
            None,
            7,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&script_path);

        assert!(
            result.is_ok(),
            "child agent invocation should succeed: {result:?}"
        );

        let inherited_values =
            fs::read_to_string(&output_path).expect("child output should be captured");
        let _ = fs::remove_file(&output_path);

        assert_eq!(
            inherited_values.lines().collect::<Vec<_>>(),
            vec!["7", "600"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tool_step_includes_child_agent_bridge_runtime_context() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = super::TEST_ENV_LOCK
            .lock()
            .expect("environment lock should not be poisoned");
        let original_depth = std::env::var_os(super::AGENT_ACTION_DEPTH_ENV);
        std::env::set_var(super::AGENT_ACTION_DEPTH_ENV, "1");

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be valid")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("cargo-ai-tool-bridge-{unique}"));
        let tool_root = root.join(".cargo-ai").join("tools").join("bridge_tool");
        fs::create_dir_all(&tool_root).expect("tool metadata dir should be created");
        fs::write(
            root.join(".cargo-ai").join("project.toml"),
            "format_version = 1\n",
        )
        .expect("project metadata should be written");

        let capture_path = root.join("bridge-request.json");
        let script_path = root.join("bridge_tool.sh");
        let script_body = format!(
            "#!/bin/sh\nif [ \"$1\" = \"describe\" ]; then\ncat <<'EOF'\n{{\"protocol_version\":1,\"name\":\"bridge_tool\",\"description\":\"bridge test\",\"params\":{{}},\"result\":{{\"type\":\"string\",\"nullable\":true,\"description\":\"bridge result\"}},\"resource_profile\":{{\"network\":\"none\",\"filesystem_read\":\"none\",\"filesystem_write\":\"none\",\"subprocess\":\"none\",\"env_read\":\"none\",\"credential_access\":\"none\"}},\"self_test\":{{\"supported\":false,\"safe\":false,\"description\":\"not implemented\"}},\"examples\":{{\"minimal_invoke\":{{\"protocol_version\":1,\"params\":{{}}}},\"full_invoke\":{{\"protocol_version\":1,\"params\":{{}}}}}}}}\nEOF\nelif [ \"$1\" = \"invoke\" ]; then\ncat > \"{}\"\nprintf '{{\"protocol_version\":1,\"result\":null}}\\n'\nelse\nexit 1\nfi\n",
            capture_path.display()
        );
        fs::write(&script_path, script_body).expect("tool script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let manifest = serde_json::json!({
            "schema_version": 1,
            "tool_id": "bridge_tool",
            "binary": {
                "default_name": "bridge_tool"
            },
            "artifacts": {
                "test-target": {
                    "path": script_path.display().to_string()
                }
            }
        });
        fs::write(
            tool_root.join("tool.json"),
            serde_json::to_string(&manifest).expect("manifest should serialize"),
        )
        .expect("tool manifest should be written");

        let step = crate::RunStep {
            tool_name: Some("bridge_tool".to_string()),
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "tool".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let mut provider_context = provider_context();
        provider_context.tool_resolver = Some(Arc::new(crate::commands::tools::ToolResolver::new(
            Some(root.clone()),
            "test-target",
        )));

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_tool_step(
            &step,
            &json!({}),
            0,
            "bridge_action",
            1,
            &provider_context,
            Some(crate::ActionExecutionMode::Sequential),
            4,
            runtime_budget,
        )
        .await;

        match &original_depth {
            Some(value) => std::env::set_var(super::AGENT_ACTION_DEPTH_ENV, value),
            None => std::env::remove_var(super::AGENT_ACTION_DEPTH_ENV),
        }

        assert!(result.is_ok(), "tool step should succeed: {result:?}");

        let captured_request: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&capture_path).expect("request should exist"))
                .expect("request json should parse");
        let bridge = captured_request
            .get("runtime_context")
            .and_then(|value| value.get("agent_bridge"))
            .expect("agent bridge should be present");
        assert_eq!(bridge.get("current_depth"), Some(&json!(1)));
        assert_eq!(bridge.get("max_depth"), Some(&json!(4)));
        assert_eq!(bridge.get("action_execution"), Some(&json!("sequential")));
        assert_eq!(
            bridge
                .get("runtime_budget")
                .and_then(|value| value.get("max_runtime_secs")),
            Some(&json!(600))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn json_artifact_prefers_cargo_ai_subcommand_when_available() {
        use std::fs;

        let test_path = TestPathCommands::new();
        let output_path = std::env::temp_dir().join(format!(
            "cai2078-json-artifact-cargo-args-{}.txt",
            std::process::id()
        ));
        let cargo_script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
            output_path.display()
        );
        test_path.write_command("cargo", &cargo_script);
        test_path.write_command("cargo-ai", "#!/bin/sh\nexit 99\n");

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let artifact_name = format!(".tmp-cai2078-json-child-{}.json", std::process::id());
        let artifact_path = current_dir.join(&artifact_name);
        fs::write(&artifact_path, "{}").expect("json child artifact should be written");

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", artifact_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: Some(vec![crate::ActionInput::Text {
                text: vec![crate::RunArg::Literal("hello".to_string())],
            }]),
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_agent_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "invoke_child",
            Some(crate::ActionExecutionMode::Sequential),
            5,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&artifact_path);
        assert!(
            result.is_ok(),
            "json child artifact should succeed via cargo ai: {result:?}"
        );

        let args = fs::read_to_string(&output_path).expect("cargo args should be captured");
        let _ = fs::remove_file(&output_path);
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "ai",
                "run",
                format!("./{}", artifact_name).as_str(),
                "--action-execution",
                "sequential",
                "--input-text",
                "hello",
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn json_artifact_falls_back_to_standalone_cargo_ai() {
        use std::fs;

        let test_path = TestPathCommands::new();
        let output_path = std::env::temp_dir().join(format!(
            "cai2078-json-artifact-standalone-args-{}.txt",
            std::process::id()
        ));
        let cargo_ai_script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
            output_path.display()
        );
        test_path.write_command("cargo-ai", &cargo_ai_script);

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let artifact_name = format!(".tmp-cai2078-json-standalone-{}.json", std::process::id());
        let artifact_path = current_dir.join(&artifact_name);
        fs::write(&artifact_path, "{}").expect("json child artifact should be written");

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", artifact_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: Some(vec![crate::ActionInput::Text {
                text: vec![crate::RunArg::Literal("hello".to_string())],
            }]),
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = run_agent_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "invoke_child",
            None,
            5,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&artifact_path);
        assert!(
            result.is_ok(),
            "json child artifact should succeed via standalone cargo-ai: {result:?}"
        );

        let args = fs::read_to_string(&output_path).expect("cargo-ai args should be captured");
        let _ = fs::remove_file(&output_path);
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "run",
                format!("./{}", artifact_name).as_str(),
                "--input-text",
                "hello"
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn json_artifact_fails_when_cargo_ai_is_unavailable() {
        use std::fs;

        let _test_path = TestPathCommands::new();
        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let artifact_name = format!(".tmp-cai2078-json-missing-{}.json", std::process::id());
        let artifact_path = current_dir.join(&artifact_name);
        fs::write(&artifact_path, "{}").expect("json child artifact should be written");

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", artifact_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let error = run_agent_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "invoke_child",
            None,
            5,
            runtime_budget,
        )
        .await
        .expect_err("json child artifact should fail when Cargo AI is unavailable");

        let _ = fs::remove_file(&artifact_path);
        assert!(error.contains("requires Cargo AI"));
        assert!(error.contains("cargo ai"));
        assert!(error.contains("cargo-ai"));
    }

    #[tokio::test]
    async fn agent_step_rejects_bare_child_name() {
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some("child_agent".to_string()),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let error = run_agent_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "invoke_child",
            None,
            5,
            runtime_budget,
        )
        .await
        .expect_err("bare child agent names should be rejected");

        assert!(error.contains("bare child-agent names are not allowed"));
    }

    #[tokio::test]
    async fn agent_step_rejects_parent_traversal_path() {
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some("./../child_agent".to_string()),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let error = run_agent_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "invoke_child",
            None,
            5,
            runtime_budget,
        )
        .await
        .expect_err("parent traversal should be rejected");

        assert!(error.contains("parent traversal"));
    }

    #[tokio::test]
    async fn agent_step_rejects_nested_child_path() {
        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some("./agents/child_agent".to_string()),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let error = run_agent_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "invoke_child",
            None,
            5,
            runtime_budget,
        )
        .await
        .expect_err("nested child agent paths should be rejected");

        assert!(error.contains("nested child-agent paths"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_step_times_out_against_invocation_budget() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(".tmp-cai2032-agent-timeout-child-{}.sh", std::process::id());
        let script_path = current_dir.join(&script_name);

        let script_body = "#!/bin/sh\nsleep 2\n";

        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", script_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(1));
        let error = run_agent_step(
            &step,
            &json!({}),
            &no_named_inputs(),
            0,
            "invoke_child",
            None,
            5,
            runtime_budget,
        )
        .await
        .expect_err("runtime budget should time out the child");

        let _ = fs::remove_file(&script_path);

        assert!(error.contains("max-runtime-in-sec 1"));
        assert!(error.contains("while waiting for child agent"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_actions_stops_on_failed_exec_by_default() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(".tmp-cai2040-stop-child-{}.sh", std::process::id());
        let script_path = current_dir.join(&script_name);
        let output_path =
            std::env::temp_dir().join(format!("cai2040-stop-output-{}.txt", std::process::id()));

        let script_body = format!("#!/bin/sh\nprintf 'ran' > \"{}\"\n", output_path.display());
        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let failing_step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "exec".to_string(),
            program: Some("/bin/sh".to_string()),
            model: None,
            profile: None,
            output_variable: None,
            status_variable: Some("step_status".to_string()),
            error_variable: Some("step_error".to_string()),
            failure_mode: None,
            when: None,
            args: vec![
                crate::RunArg::Literal("-lc".to_string()),
                crate::RunArg::Literal("exit 7".to_string()),
            ],
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let second_step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", script_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = apply_actions(
            &crate::Output { answer: 4 },
            &[action(vec![failing_step, second_step])],
            &serde_json::Map::new(),
            &[],
            crate::ActionExecutionMode::Sequential,
            None,
            RequestedActionOutputMode::Auto,
            &provider_context(),
            5,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&script_path);
        let _ = fs::remove_file(&output_path);

        let error = result.expect_err("failed exec should stop by default");
        assert!(error.contains("exited with status"));
        assert!(
            !output_path.exists(),
            "later step should not run after default stop"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_actions_continues_to_later_top_level_actions_after_hard_failure() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(".tmp-cai2067-later-action-{}.sh", std::process::id());
        let script_path = current_dir.join(&script_name);
        let output_path =
            std::env::temp_dir().join(format!("cai2067-later-action-{}.txt", std::process::id()));

        let script_body = format!("#!/bin/sh\nprintf 'ran' > \"{}\"\n", output_path.display());
        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let first_action = crate::Action {
            name: "first_action".to_string(),
            logic: json!({ "==": [{ "var": "answer" }, 4] }),
            run: vec![crate::RunStep {
                tool_name: None,
                tool_params: std::collections::BTreeMap::new(),
                ignore_tools: false,
                kind: "exec".to_string(),
                program: Some("/bin/sh".to_string()),
                model: None,
                profile: None,
                output_variable: None,
                status_variable: None,
                error_variable: None,
                failure_mode: None,
                when: None,
                args: vec![
                    crate::RunArg::Literal("-lc".to_string()),
                    crate::RunArg::Literal("exit 11".to_string()),
                ],
                prompt: None,
                path: None,
                subject: None,
                text: None,
                agent: None,
                usage_log: None,
                run_vars: None,
                input_overrides: None,
                inputs: None,
                reference_images: None,
                input_mode: None,
                platforms: None,
            }],
        };
        let second_action = crate::Action {
            name: "second_action".to_string(),
            logic: json!({ "==": [{ "var": "answer" }, 4] }),
            run: vec![crate::RunStep {
                tool_name: None,
                tool_params: std::collections::BTreeMap::new(),
                ignore_tools: false,
                kind: "agent".to_string(),
                program: None,
                model: None,
                profile: None,
                output_variable: None,
                status_variable: None,
                error_variable: None,
                failure_mode: None,
                when: None,
                args: Vec::new(),
                prompt: None,
                path: None,
                subject: None,
                text: None,
                agent: Some(format!("./{}", script_name)),
                usage_log: None,
                run_vars: None,
                input_overrides: None,
                inputs: None,
                reference_images: None,
                input_mode: None,
                platforms: None,
            }],
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = apply_actions(
            &crate::Output { answer: 4 },
            &[first_action, second_action],
            &serde_json::Map::new(),
            &[],
            crate::ActionExecutionMode::Sequential,
            None,
            RequestedActionOutputMode::Auto,
            &provider_context(),
            5,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&script_path);

        let error = result.expect_err("one failed action should still fail overall");
        assert!(error.contains("first_action"));
        assert!(error.contains("exited with status"));

        let file_contents =
            fs::read_to_string(&output_path).expect("later top-level action should have executed");
        let _ = fs::remove_file(&output_path);
        assert_eq!(file_contents, "ran");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_actions_parallel_starts_matching_actions_without_waiting() {
        use std::fs;

        let first_output_path =
            std::env::temp_dir().join(format!("cai2067-parallel-first-{}.txt", std::process::id()));
        let second_output_path = std::env::temp_dir().join(format!(
            "cai2067-parallel-second-{}.txt",
            std::process::id()
        ));

        let first_action = crate::Action {
            name: "first_action".to_string(),
            logic: json!({ "==": [{ "var": "answer" }, 4] }),
            run: vec![crate::RunStep {
                tool_name: None,
                tool_params: std::collections::BTreeMap::new(),
                ignore_tools: false,
                kind: "exec".to_string(),
                program: Some("/bin/sh".to_string()),
                model: None,
                profile: None,
                output_variable: None,
                status_variable: None,
                error_variable: None,
                failure_mode: None,
                when: None,
                args: vec![
                    crate::RunArg::Literal("-lc".to_string()),
                    crate::RunArg::Literal(format!(
                        "sleep 1; printf 'first' > \"{}\"",
                        first_output_path.display()
                    )),
                ],
                prompt: None,
                path: None,
                subject: None,
                text: None,
                agent: None,
                usage_log: None,
                run_vars: None,
                input_overrides: None,
                inputs: None,
                reference_images: None,
                input_mode: None,
                platforms: None,
            }],
        };
        let second_action = crate::Action {
            name: "second_action".to_string(),
            logic: json!({ "==": [{ "var": "answer" }, 4] }),
            run: vec![crate::RunStep {
                tool_name: None,
                tool_params: std::collections::BTreeMap::new(),
                ignore_tools: false,
                kind: "exec".to_string(),
                program: Some("/bin/sh".to_string()),
                model: None,
                profile: None,
                output_variable: None,
                status_variable: None,
                error_variable: None,
                failure_mode: None,
                when: None,
                args: vec![
                    crate::RunArg::Literal("-lc".to_string()),
                    crate::RunArg::Literal(format!(
                        "printf 'second' > \"{}\"",
                        second_output_path.display()
                    )),
                ],
                prompt: None,
                path: None,
                subject: None,
                text: None,
                agent: None,
                usage_log: None,
                run_vars: None,
                input_overrides: None,
                inputs: None,
                reference_images: None,
                input_mode: None,
                platforms: None,
            }],
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let output = crate::Output { answer: 4 };
        let actions = vec![first_action, second_action];
        let runtime_vars = serde_json::Map::new();
        let provider_context = provider_context();
        let mut future = std::pin::pin!(apply_actions(
            &output,
            &actions,
            &runtime_vars,
            &[],
            crate::ActionExecutionMode::Parallel,
            None,
            RequestedActionOutputMode::Auto,
            &provider_context,
            5,
            runtime_budget,
        ));

        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(300)) => {}
            result = &mut future => panic!("parallel actions finished too early: {result:?}"),
        }

        assert!(
            second_output_path.exists(),
            "second action should run before the delayed first action completes"
        );
        assert!(
            !first_output_path.exists(),
            "first action should still be sleeping when the second action has already completed"
        );

        let result = future.await;

        let first_contents =
            fs::read_to_string(&first_output_path).expect("first action should eventually finish");
        let second_contents = fs::read_to_string(&second_output_path)
            .expect("second action should have already completed");
        let _ = fs::remove_file(&first_output_path);
        let _ = fs::remove_file(&second_output_path);

        assert!(
            result.is_ok(),
            "parallel actions should succeed: {result:?}"
        );
        assert_eq!(first_contents, "first");
        assert_eq!(second_contents, "second");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_actions_parallel_reports_failures_in_lane_order() {
        let first_action = crate::Action {
            name: "first_action".to_string(),
            logic: json!({ "==": [{ "var": "answer" }, 4] }),
            run: vec![crate::RunStep {
                tool_name: None,
                tool_params: std::collections::BTreeMap::new(),
                ignore_tools: false,
                kind: "exec".to_string(),
                program: Some("/bin/sh".to_string()),
                model: None,
                profile: None,
                output_variable: None,
                status_variable: None,
                error_variable: None,
                failure_mode: None,
                when: None,
                args: vec![
                    crate::RunArg::Literal("-lc".to_string()),
                    crate::RunArg::Literal("sleep 1; exit 11".to_string()),
                ],
                prompt: None,
                path: None,
                subject: None,
                text: None,
                agent: None,
                usage_log: None,
                run_vars: None,
                input_overrides: None,
                inputs: None,
                reference_images: None,
                input_mode: None,
                platforms: None,
            }],
        };
        let second_action = crate::Action {
            name: "second_action".to_string(),
            logic: json!({ "==": [{ "var": "answer" }, 4] }),
            run: vec![crate::RunStep {
                tool_name: None,
                tool_params: std::collections::BTreeMap::new(),
                ignore_tools: false,
                kind: "exec".to_string(),
                program: Some("/bin/sh".to_string()),
                model: None,
                profile: None,
                output_variable: None,
                status_variable: None,
                error_variable: None,
                failure_mode: None,
                when: None,
                args: vec![
                    crate::RunArg::Literal("-lc".to_string()),
                    crate::RunArg::Literal("exit 12".to_string()),
                ],
                prompt: None,
                path: None,
                subject: None,
                text: None,
                agent: None,
                usage_log: None,
                run_vars: None,
                input_overrides: None,
                inputs: None,
                reference_images: None,
                input_mode: None,
                platforms: None,
            }],
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let actions = vec![first_action, second_action];
        let error = apply_actions(
            &crate::Output { answer: 4 },
            &actions,
            &serde_json::Map::new(),
            &[],
            crate::ActionExecutionMode::Parallel,
            None,
            RequestedActionOutputMode::Auto,
            &provider_context(),
            5,
            runtime_budget,
        )
        .await
        .expect_err("parallel hard failures should fail overall");

        let first_idx = error
            .find("first_action")
            .expect("aggregated error should mention first lane");
        let second_idx = error
            .find("second_action")
            .expect("aggregated error should mention second lane");

        assert!(
            first_idx < second_idx,
            "parallel failure reporting should remain in JSON/lane order: {error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_actions_abort_stops_current_and_later_top_level_work() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(".tmp-cai2067-abort-child-{}.sh", std::process::id());
        let script_path = current_dir.join(&script_name);
        let output_path =
            std::env::temp_dir().join(format!("cai2067-abort-output-{}.txt", std::process::id()));

        let script_body = format!("#!/bin/sh\nprintf 'ran' > \"{}\"\n", output_path.display());
        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let first_action = crate::Action {
            name: "first_action".to_string(),
            logic: json!({ "==": [{ "var": "answer" }, 4] }),
            run: vec![
                crate::RunStep {
                    tool_name: None,
                    tool_params: std::collections::BTreeMap::new(),
                    ignore_tools: false,
                    kind: "exec".to_string(),
                    program: Some("/bin/sh".to_string()),
                    model: None,
                    profile: None,
                    output_variable: None,
                    status_variable: Some("abort_status".to_string()),
                    error_variable: Some("abort_error".to_string()),
                    failure_mode: Some(crate::FailureMode::Abort),
                    when: None,
                    args: vec![
                        crate::RunArg::Literal("-lc".to_string()),
                        crate::RunArg::Literal("exit 17".to_string()),
                    ],
                    prompt: None,
                    path: None,
                    subject: None,
                    text: None,
                    agent: None,
                    usage_log: None,
                    run_vars: None,
                    input_overrides: None,
                    inputs: None,
                    reference_images: None,
                    input_mode: None,
                    platforms: None,
                },
                crate::RunStep {
                    tool_name: None,
                    tool_params: std::collections::BTreeMap::new(),
                    ignore_tools: false,
                    kind: "agent".to_string(),
                    program: None,
                    model: None,
                    profile: None,
                    output_variable: None,
                    status_variable: None,
                    error_variable: None,
                    failure_mode: None,
                    when: None,
                    args: Vec::new(),
                    prompt: None,
                    path: None,
                    subject: None,
                    text: None,
                    agent: Some(format!("./{}", script_name)),
                    usage_log: None,
                    run_vars: None,
                    input_overrides: None,
                    inputs: None,
                    reference_images: None,
                    input_mode: None,
                    platforms: None,
                },
            ],
        };
        let second_action = crate::Action {
            name: "second_action".to_string(),
            logic: json!({ "==": [{ "var": "answer" }, 4] }),
            run: vec![crate::RunStep {
                tool_name: None,
                tool_params: std::collections::BTreeMap::new(),
                ignore_tools: false,
                kind: "agent".to_string(),
                program: None,
                model: None,
                profile: None,
                output_variable: None,
                status_variable: None,
                error_variable: None,
                failure_mode: None,
                when: None,
                args: Vec::new(),
                prompt: None,
                path: None,
                subject: None,
                text: None,
                agent: Some(format!("./{}", script_name)),
                usage_log: None,
                run_vars: None,
                input_overrides: None,
                inputs: None,
                reference_images: None,
                input_mode: None,
                platforms: None,
            }],
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let error = apply_actions(
            &crate::Output { answer: 4 },
            &[first_action, second_action],
            &serde_json::Map::new(),
            &[],
            crate::ActionExecutionMode::Sequential,
            None,
            RequestedActionOutputMode::Auto,
            &provider_context(),
            5,
            runtime_budget,
        )
        .await
        .expect_err("abort should fail the invocation");

        let output_exists = output_path.exists();
        let _ = fs::remove_file(&script_path);
        let _ = fs::remove_file(&output_path);

        assert!(error.contains("Run aborted by [Action 1: first_action]"));
        assert!(error.contains("exit status: 17"));
        assert!(
            !output_exists,
            "abort should stop later steps in the lane and later top-level actions"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_actions_parallel_abort_stops_other_lanes_before_next_step() {
        use std::fs;

        let later_output_path = std::env::temp_dir().join(format!(
            "cai2067-parallel-abort-output-{}.txt",
            std::process::id()
        ));

        let first_action = crate::Action {
            name: "first_action".to_string(),
            logic: json!({ "==": [{ "var": "answer" }, 4] }),
            run: vec![
                crate::RunStep {
                    tool_name: None,
                    tool_params: std::collections::BTreeMap::new(),
                    ignore_tools: false,
                    kind: "exec".to_string(),
                    program: Some("/bin/sh".to_string()),
                    model: None,
                    profile: None,
                    output_variable: None,
                    status_variable: None,
                    error_variable: None,
                    failure_mode: None,
                    when: None,
                    args: vec![
                        crate::RunArg::Literal("-lc".to_string()),
                        crate::RunArg::Literal("sleep 0.1".to_string()),
                    ],
                    prompt: None,
                    path: None,
                    subject: None,
                    text: None,
                    agent: None,
                    usage_log: None,
                    run_vars: None,
                    input_overrides: None,
                    inputs: None,
                    reference_images: None,
                    input_mode: None,
                    platforms: None,
                },
                crate::RunStep {
                    tool_name: None,
                    tool_params: std::collections::BTreeMap::new(),
                    ignore_tools: false,
                    kind: "exec".to_string(),
                    program: Some("/bin/sh".to_string()),
                    model: None,
                    profile: None,
                    output_variable: None,
                    status_variable: None,
                    error_variable: None,
                    failure_mode: Some(crate::FailureMode::Abort),
                    when: None,
                    args: vec![
                        crate::RunArg::Literal("-lc".to_string()),
                        crate::RunArg::Literal("exit 19".to_string()),
                    ],
                    prompt: None,
                    path: None,
                    subject: None,
                    text: None,
                    agent: None,
                    usage_log: None,
                    run_vars: None,
                    input_overrides: None,
                    inputs: None,
                    reference_images: None,
                    input_mode: None,
                    platforms: None,
                },
            ],
        };
        let second_action = crate::Action {
            name: "second_action".to_string(),
            logic: json!({ "==": [{ "var": "answer" }, 4] }),
            run: vec![
                crate::RunStep {
                    tool_name: None,
                    tool_params: std::collections::BTreeMap::new(),
                    ignore_tools: false,
                    kind: "exec".to_string(),
                    program: Some("/bin/sh".to_string()),
                    model: None,
                    profile: None,
                    output_variable: None,
                    status_variable: None,
                    error_variable: None,
                    failure_mode: None,
                    when: None,
                    args: vec![
                        crate::RunArg::Literal("-lc".to_string()),
                        crate::RunArg::Literal("sleep 1".to_string()),
                    ],
                    prompt: None,
                    path: None,
                    subject: None,
                    text: None,
                    agent: None,
                    usage_log: None,
                    run_vars: None,
                    input_overrides: None,
                    inputs: None,
                    reference_images: None,
                    input_mode: None,
                    platforms: None,
                },
                crate::RunStep {
                    tool_name: None,
                    tool_params: std::collections::BTreeMap::new(),
                    ignore_tools: false,
                    kind: "exec".to_string(),
                    program: Some("/bin/sh".to_string()),
                    model: None,
                    profile: None,
                    output_variable: None,
                    status_variable: None,
                    error_variable: None,
                    failure_mode: None,
                    when: None,
                    args: vec![
                        crate::RunArg::Literal("-lc".to_string()),
                        crate::RunArg::Literal(format!(
                            "printf 'late' > \"{}\"",
                            later_output_path.display()
                        )),
                    ],
                    prompt: None,
                    path: None,
                    subject: None,
                    text: None,
                    agent: None,
                    usage_log: None,
                    run_vars: None,
                    input_overrides: None,
                    inputs: None,
                    reference_images: None,
                    input_mode: None,
                    platforms: None,
                },
            ],
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let error = apply_actions(
            &crate::Output { answer: 4 },
            &[first_action, second_action],
            &serde_json::Map::new(),
            &[],
            crate::ActionExecutionMode::Parallel,
            None,
            RequestedActionOutputMode::Auto,
            &provider_context(),
            5,
            runtime_budget,
        )
        .await
        .expect_err("abort should fail the parallel invocation");

        let later_output_exists = later_output_path.exists();
        let _ = fs::remove_file(&later_output_path);

        assert!(error.contains("Run aborted by [Action 1: first_action]"));
        assert!(error.contains("exit status: 19"));
        assert!(
            !later_output_exists,
            "other lanes should not start later steps after a peer lane aborts"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_actions_continue_mode_exposes_failed_status_to_later_when() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(".tmp-cai2040-continue-child-{}.sh", std::process::id());
        let script_path = current_dir.join(&script_name);
        let output_path = std::env::temp_dir().join(format!(
            "cai2040-continue-output-{}.txt",
            std::process::id()
        ));

        let script_body = format!("#!/bin/sh\nprintf 'ran' > \"{}\"\n", output_path.display());
        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let failing_step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "exec".to_string(),
            program: Some("/bin/sh".to_string()),
            model: None,
            profile: None,
            output_variable: None,
            status_variable: Some("step_status".to_string()),
            error_variable: Some("step_error".to_string()),
            failure_mode: Some(crate::FailureMode::Continue),
            when: None,
            args: vec![
                crate::RunArg::Literal("-lc".to_string()),
                crate::RunArg::Literal("exit 9".to_string()),
            ],
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: None,
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let second_step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: Some(json!({ "==": [{ "var": "step_status" }, "failed"] })),
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", script_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = apply_actions(
            &crate::Output { answer: 4 },
            &[action(vec![failing_step, second_step])],
            &serde_json::Map::new(),
            &[],
            crate::ActionExecutionMode::Sequential,
            None,
            RequestedActionOutputMode::Auto,
            &provider_context(),
            5,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&script_path);

        assert!(
            result.is_ok(),
            "continue-mode failure should not stop the action"
        );

        let file_contents =
            fs::read_to_string(&output_path).expect("later step should have executed");
        let _ = fs::remove_file(&output_path);

        assert_eq!(file_contents, "ran");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_actions_allows_runtime_vars_in_action_logic() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(".tmp-cai2055-runtime-logic-{}.sh", std::process::id());
        let script_path = current_dir.join(&script_name);
        let output_path =
            std::env::temp_dir().join(format!("cai2055-runtime-logic-{}.txt", std::process::id()));

        let script_body = format!("#!/bin/sh\nprintf 'ran' > \"{}\"\n", output_path.display());
        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: None,
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", script_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };
        let action = crate::Action {
            name: "runtime_gate".to_string(),
            logic: json!({ "==": [{ "var": "runtime.generate_images" }, true] }),
            run: vec![step],
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = apply_actions(
            &crate::Output { answer: 4 },
            &[action],
            &runtime_vars(&[("generate_images", json!(true))]),
            &[],
            crate::ActionExecutionMode::Sequential,
            None,
            RequestedActionOutputMode::Auto,
            &provider_context(),
            5,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&script_path);

        assert!(
            result.is_ok(),
            "runtime-gated action should succeed: {result:?}"
        );

        let file_contents = fs::read_to_string(&output_path).expect("action should have run");
        let _ = fs::remove_file(&output_path);
        assert_eq!(file_contents, "ran");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_actions_allows_runtime_vars_in_step_when() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let current_dir = std::env::current_dir().expect("current dir should resolve");
        let script_name = format!(".tmp-cai2055-runtime-when-{}.sh", std::process::id());
        let script_path = current_dir.join(&script_name);
        let output_path =
            std::env::temp_dir().join(format!("cai2055-runtime-when-{}.txt", std::process::id()));

        let script_body = format!("#!/bin/sh\nprintf 'ran' > \"{}\"\n", output_path.display());
        fs::write(&script_path, script_body).expect("script should be written");
        let mut permissions = fs::metadata(&script_path)
            .expect("script metadata should load")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("script should be executable");

        let step = crate::RunStep {
            tool_name: None,
            tool_params: std::collections::BTreeMap::new(),
            ignore_tools: false,
            kind: "agent".to_string(),
            program: None,
            model: None,
            profile: None,
            output_variable: None,
            status_variable: None,
            error_variable: None,
            failure_mode: None,
            when: Some(json!({ "==": [{ "var": "runtime.generate_images" }, true] })),
            args: Vec::new(),
            prompt: None,
            path: None,
            subject: None,
            text: None,
            agent: Some(format!("./{}", script_name)),
            usage_log: None,
            run_vars: None,
            input_overrides: None,
            inputs: None,
            reference_images: None,
            input_mode: None,
            platforms: None,
        };

        let runtime_budget = configured_agent_action_runtime_budget(Some(600));
        let result = apply_actions(
            &crate::Output { answer: 4 },
            &[action(vec![step])],
            &runtime_vars(&[("generate_images", json!(true))]),
            &[],
            crate::ActionExecutionMode::Sequential,
            None,
            RequestedActionOutputMode::Auto,
            &provider_context(),
            5,
            runtime_budget,
        )
        .await;

        let _ = fs::remove_file(&script_path);

        assert!(
            result.is_ok(),
            "runtime-gated step should succeed: {result:?}"
        );

        let file_contents = fs::read_to_string(&output_path).expect("step should have run");
        let _ = fs::remove_file(&output_path);
        assert_eq!(file_contents, "ran");
    }

    #[test]
    fn validate_agent_action_depth_allows_nested_calls_below_limit() {
        let result = validate_agent_action_depth(2, 5, "invoke_child");

        assert!(result.is_ok(), "depth below limit should be allowed");
    }

    #[test]
    fn validate_agent_action_depth_rejects_when_limit_is_reached() {
        let error = validate_agent_action_depth(5, 5, "invoke_child")
            .expect_err("depth at limit should be rejected");

        assert!(error.contains("current depth 5"));
        assert!(error.contains("max-agent-depth 5"));
    }

    #[test]
    fn validate_agent_action_depth_rejects_zero_depth_limit() {
        let error = validate_agent_action_depth(0, 0, "invoke_child")
            .expect_err("zero max depth should disable child invocation");

        assert!(error.contains("current depth 0"));
        assert!(error.contains("max-agent-depth 0"));
    }

    #[test]
    fn formats_backend_ui_success_with_kind_prefix() {
        let response = json!({
            "ui": {
                "schema": "1.0",
                "kind": "success",
                "title": "Email sent",
                "summary": "Test email sent to sales@analyzer1.com.",
                "next_steps": ["Check your inbox and spam folder for the message."]
            }
        });

        let rendered =
            format_backend_ui_message(&response, true).expect("success ui should format");

        assert!(rendered.contains("✅ Email sent"));
        assert!(rendered.contains("Test email sent to sales@analyzer1.com."));
        assert!(rendered.contains("Next steps:"));
    }

    #[test]
    fn formats_backend_ui_failure_without_kind_prefix() {
        let response = json!({
            "ui": {
                "schema": "1.0",
                "kind": "failure",
                "title": "Request failed",
                "summary": "Email sending is disabled for this account.",
                "next_steps": ["Enable mail and retry."]
            }
        });

        let rendered = format_backend_error_message(&response).expect("failure ui should format");

        assert!(rendered.starts_with("Request failed"));
        assert!(!rendered.contains("❌ Request failed"));
        assert!(rendered.contains("Email sending is disabled for this account."));
        assert!(rendered.contains("Next steps:"));
    }

    #[test]
    fn auto_render_mode_prefers_live_when_supported() {
        assert_eq!(
            resolve_action_output_mode_for_capability(RequestedActionOutputMode::Auto, true),
            (ActionOutputMode::Live, None)
        );
    }

    #[test]
    fn auto_render_mode_uses_append_only_when_live_is_unsupported() {
        assert_eq!(
            resolve_action_output_mode_for_capability(RequestedActionOutputMode::Auto, false),
            (ActionOutputMode::AppendOnly, None)
        );
    }

    #[test]
    fn explicit_live_render_mode_falls_back_with_notice_when_unsupported() {
        assert_eq!(
            resolve_action_output_mode_for_capability(RequestedActionOutputMode::Live, false),
            (
                ActionOutputMode::AppendOnly,
                Some(
                    "! Requested --render-mode live, but live output is unavailable here; using append-only output.",
                ),
            )
        );
    }

    #[test]
    fn explicit_append_only_render_mode_forces_append_only() {
        assert_eq!(
            resolve_action_output_mode_for_capability(RequestedActionOutputMode::AppendOnly, true,),
            (ActionOutputMode::AppendOnly, None)
        );
    }
    #[test]
    fn project_outputs_and_dynamic_inputs_use_data_without_changing_legacy_paths() {
        let root = std::env::temp_dir().join(format!("runtime-paths-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join(".cargo-ai")).unwrap();
        fs::write(
            root.join(".cargo-ai/project.toml"),
            "[runtime]\ndata_root = '.cargo-ai/data'",
        )
        .unwrap();
        let data = crate::commands::runtime_data::project_data_root(Some(&root))
            .unwrap()
            .unwrap();
        let expected = fs::canonicalize(&root).unwrap().join(".cargo-ai/data");
        assert_eq!(
            super::resolve_generated_image_output_path_with_data(
                "image.png",
                "test",
                None,
                Some(&data)
            )
            .unwrap(),
            expected.join("image.png")
        );
        assert_eq!(
            super::resolve_generated_image_output_path("image.png", "test", None).unwrap(),
            PathBuf::from("image.png")
        );
        assert_eq!(
            super::resolve_child_usage_log_path_with_data("child.jsonl", "test", None, Some(&data))
                .unwrap(),
            expected.join("child.jsonl")
        );
        assert_eq!(
            super::resolve_installed_child_input_path_with_data(
                "image.png",
                false,
                "test",
                1,
                "image",
                None,
                Some(&data)
            )
            .unwrap(),
            "image.png"
        );
        assert_eq!(
            super::resolve_installed_child_input_path_with_data(
                "image.png",
                true,
                "test",
                1,
                "image",
                None,
                Some(&data)
            )
            .unwrap(),
            expected.join("image.png").to_string_lossy()
        );
        assert!(super::resolve_generated_image_output_path_with_data(
            "../image.png",
            "test",
            None,
            Some(&data)
        )
        .is_err());
        assert!(!expected.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
