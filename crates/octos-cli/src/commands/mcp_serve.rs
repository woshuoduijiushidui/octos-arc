//! M7.2 — `octos mcp-serve` subcommand.
//!
//! Exposes octos as an MCP server so outer orchestrators can invoke it
//! as a sub-agent. See [`octos_agent::mcp_server`] for the transport and
//! session-level tool semantics.
//!
//! # Transports
//!
//! - `stdio` (default): JSON-RPC over stdin/stdout. Parent-trust auth.
//! - `http`: MCP Streamable HTTP served by the rmcp SDK. Requires a bearer
//!   token supplied via the `OCTOS_MCP_SERVER_TOKEN` environment variable.
//!
//! # Session dispatch (M7.2a)
//!
//! The [`RealSessionDispatch`] implementation wires outer MCP calls into
//! the existing [`Agent`](octos_agent::Agent) loop. Every `run_octos_session`
//! MCP invocation:
//!
//! 1. Loads [`ProfileConfig`](crate::profiles::ProfileConfig)-style config
//!    from disk (when present) and builds the LLM provider via the same
//!    factory chat/gateway use.
//! 2. Marks the session `Running` on the supplied
//!    [`SessionLifecycleObserver`](octos_agent::mcp_server::SessionLifecycleObserver).
//! 3. Constructs a single-shot [`Agent`](octos_agent::Agent) and runs the
//!    supplied prompt as a [`Task`](octos_core::Task) — the same code path
//!    the local chat command uses, including workspace-contract enforcement.
//! 4. Marks the session `Verifying`, resolves the contract artifact (either
//!    the caller-supplied `expected_artifact` or the workspace contract's
//!    primary artifact), then transitions to `Ready`/`Failed` with the
//!    aggregate outcome.
//!
//! Failures carry a typed prefix (`config_error:`, `llm_error:`,
//! `contract_failed:`, `artifact_missing:`, `session_failed:`) so outer
//! orchestrators can branch on category without scraping English text.
//! Native ARC input additionally uses `arc_task_invalid:` and
//! `artifact_schema_invalid:`; see `docs/ARC_AGENT_TASK_MCP.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use clap::{Args, ValueEnum};
use eyre::{Result, WrapErr};
use fs2::FileExt;
use octos_agent::arc_task::{
    ARC_AGENT_TASK_SCHEMA_V1, parse_arc_agent_task_input, validate_arc_artifact_location,
    validate_arc_response,
};
use octos_agent::completion_gate::{
    ArtifactCheckKind, ArtifactCheckOutcome, ArtifactReasonCode, ArtifactState, CheckOutcome,
    CompletionCandidate, CompletionDecision, CompletionGate, CompletionReceipt,
    ProgressObservation, TerminalReason, classify, failure_signature, observe_progress,
};
use octos_agent::mcp_server::{
    McpServer, McpServerError, McpSessionCost, McpSessionDispatch, McpSessionOutcome,
    OCTOS_MCP_SERVER_TOKEN_ENV, SessionLifecycleObserver,
};
use octos_agent::snapshot::{SnapshotId, SnapshotManager};
use octos_agent::task_supervisor::{TaskLifecycleState, TaskSupervisor};
use octos_agent::validators::{
    ValidatorInvocation, ValidatorOutcome, ValidatorPhase, ValidatorRunner, ValidatorStatus,
    run_workspace_validators,
};
use octos_agent::{
    Agent, AgentConfig, ApprovalPolicy, EffectivePermissions, HarnessEvent, SandboxConfig,
    SandboxMode, TaskFileState, ToolPolicy, ToolRegistry, create_sandbox,
};
use octos_core::{AgentId, Task, TaskContext, TaskKind};
use octos_llm::LlmProvider;
use octos_memory::EpisodeStore;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::Executable;
use crate::config::Config;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpTransport {
    Stdio,
    Http,
}

/// Run octos as an MCP server for outer orchestrators.
#[derive(Debug, Args)]
pub struct McpServeCommand {
    /// Transport to bind. `stdio` (default) uses parent-trust auth; `http`
    /// requires a bearer token via `OCTOS_MCP_SERVER_TOKEN`.
    #[arg(long, value_enum, default_value_t = McpTransport::Stdio)]
    pub transport: McpTransport,

    /// Bind address for the HTTP transport. Defaults to 127.0.0.1:0 (ephemeral).
    #[arg(long, default_value = "127.0.0.1:4033")]
    pub bind: SocketAddr,

    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Path to config file.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Allow up to two in-task repairs in an isolated disposable workspace (H06).
    #[arg(long)]
    pub h06_completion_repair: bool,
}

impl Executable for McpServeCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024)
            .build()
            .wrap_err("failed to create tokio runtime")?
            .block_on(self.run_async())
    }
}

impl McpServeCommand {
    async fn run_async(self) -> Result<()> {
        let cwd = match self.cwd.clone() {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };
        let ctx = super::resolve_command_context(self.data_dir.clone())?;
        let data_dir = ctx.data_dir.clone();

        // Load config — same precedence as chat/gateway: project-local
        // `.octos/config.json` wins over the resolved config_home. Missing
        // config is fine for stdio mode; the LLM factory will fail later
        // if no provider is configured.
        let config = if let Some(ref config_path) = self.config {
            Config::from_file(config_path)?
        } else {
            Config::load_with_context(&cwd, &ctx)?
        };

        // Capture the sandbox + tool-policy config before `config` is moved into
        // the LLM factory. The per-session tool registry uses the sandbox to
        // confine shell/exec/file tools to the workspace and the policies so an
        // external caller gets no looser a tool surface than local chat (see the
        // security note on `RealSessionDispatch`).
        let sandbox = config.sandbox.clone();
        let tool_policy = config.tool_policy.clone();
        let tool_policy_by_provider = config.tool_policy_by_provider.clone();
        // Resolved for the per-provider policy fallback (model id wins at
        // session time). Matches chat: explicit `provider`, else inferred from
        // the configured model.
        let provider_name = config
            .provider
            .clone()
            .or_else(|| {
                config
                    .model
                    .as_deref()
                    .and_then(crate::config::detect_provider)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let factory = AgentLlmFactory::from_config(config)
            .wrap_err("failed to build LLM factory from config")?;
        let dispatch_config = SessionDispatchConfig {
            cwd: cwd.clone(),
            data_dir: data_dir.clone(),
            max_iterations: 20,
            sandbox,
            tool_policy,
            tool_policy_by_provider,
            provider_name,
            output_recovery: octos_agent::output_recovery::OutputPolicy::from_env(),
            completion_repair: CompletionRepairPolicy::new(if self.h06_completion_repair {
                2
            } else {
                0
            }),
        };
        let dispatch: Arc<dyn McpSessionDispatch> =
            Arc::new(RealSessionDispatch::new(dispatch_config, factory));
        let supervisor = Arc::new(TaskSupervisor::new());
        let server = McpServer::new(dispatch, supervisor);

        // Install a lightweight event sink that forwards typed harness events
        // to the tracing subsystem. Operators can pipe logs into the same
        // harness audit tooling the rest of the runtime uses.
        server
            .set_event_sink(|event: HarnessEvent| {
                tracing::info!(target: "mcp_serve_audit", ?event, "mcp-serve audit");
            })
            .await;

        match self.transport {
            McpTransport::Stdio => {
                tracing::info!("octos mcp-serve: stdio transport");
                server.serve_stdio().await
            }
            McpTransport::Http => {
                let token = std::env::var(OCTOS_MCP_SERVER_TOKEN_ENV).map_err(|_| {
                    eyre::eyre!("{OCTOS_MCP_SERVER_TOKEN_ENV} must be set for the http transport")
                })?;
                if token.trim().is_empty() {
                    eyre::bail!("{OCTOS_MCP_SERVER_TOKEN_ENV} must not be empty");
                }
                tracing::info!(
                    bind = %self.bind,
                    "octos mcp-serve: http transport (bearer token required)",
                );
                serve_http(server, self.bind, token).await
            }
        }
    }
}

/// Maximum accepted HTTP request body for the MCP transport (1 MiB), restoring
/// the explicit cap the hand-rolled endpoint enforced. The auth middleware
/// bounds every body before rmcp reads it, so a bearer-holding client cannot
/// exhaust memory with a large or chunked POST.
#[cfg(feature = "api")]
const MAX_HTTP_BODY_BYTES: usize = 1_048_576;

/// rmcp tower service (from octos-agent) behind a bearer-token gate. Extracted
/// from [`serve_http`] so the gate can be exercised by an integration test
/// without binding a well-known port.
///
/// Returns the [`CancellationToken`](tokio_util::sync::CancellationToken) that
/// tears down live SSE sessions; [`serve_http`] cancels it on shutdown.
#[cfg(feature = "api")]
fn mcp_http_router(
    server: McpServer,
    token: String,
    allow_non_loopback: bool,
) -> (axum::Router, tokio_util::sync::CancellationToken) {
    use std::sync::Arc;

    use axum::extract::{Request, State};
    use axum::http::StatusCode;
    use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH};
    use axum::middleware::{self, Next};
    use axum::response::{IntoResponse, Response};
    use axum::{Router, body::Body};

    /// Single gate in front of the rmcp service: reject a missing/wrong bearer
    /// token (401) before touching the body, then bound the body. A *declared*
    /// oversized `Content-Length` is rejected (413) before any bytes are read;
    /// otherwise the body is buffered with a cap — which also makes an
    /// over-limit *chunked* body (no `Content-Length`) surface as 413 instead of
    /// the 500 rmcp would map a mid-read length-limit error to.
    async fn gate(State(token): State<Arc<String>>, request: Request, next: Next) -> Response {
        let provided = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        let authorized = octos_agent::mcp_server::parse_bearer_token(provided)
            .is_some_and(|candidate| octos_agent::mcp_server::constant_time_eq(&candidate, &token));
        if !authorized {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        }
        // Fast-reject a *declared* oversized body before reading any of it, so a
        // bearer holder cannot pin a connection open by announcing a huge
        // `Content-Length` and then trickling (or never sending) the body.
        let declared_over_limit = request
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|len| len > MAX_HTTP_BODY_BYTES);
        if declared_over_limit {
            return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
        }
        // Chunked / unknown-length bodies are still bounded during the read.
        let (parts, body) = request.into_parts();
        match axum::body::to_bytes(body, MAX_HTTP_BODY_BYTES).await {
            Ok(bytes) => {
                next.run(Request::from_parts(parts, Body::from(bytes)))
                    .await
            }
            Err(_) => (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
        }
    }

    let (service, cancel) = server.streamable_http_service(allow_non_loopback);
    let router = Router::new()
        .fallback_service(service)
        .layer(middleware::from_fn_with_state(Arc::new(token), gate));
    (router, cancel)
}

/// Serve the MCP Streamable HTTP transport on `bind`, gating every request
/// behind the bearer token. The rmcp tower service (built in octos-agent) is
/// mounted in axum here; bearer auth is an axum middleware in front of it.
/// Requires the `api` feature (axum); the canonical install includes it.
#[cfg(feature = "api")]
async fn serve_http(server: McpServer, bind: SocketAddr, token: String) -> Result<()> {
    // A loopback bind keeps rmcp's DNS-rebinding Host guard. An explicit
    // non-loopback bind opts into cross-host exposure, so disable the guard —
    // the bearer token is then the sole authenticator.
    let allow_non_loopback = !bind.ip().is_loopback();
    if allow_non_loopback {
        tracing::warn!(
            %bind,
            "octos mcp-serve http bound to a non-loopback address; the DNS-rebinding Host \
             guard is disabled and the bearer token is the only authenticator"
        );
    }

    let (app, cancel) = mcp_http_router(server, token, allow_non_loopback);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .wrap_err_with(|| format!("failed to bind MCP server on {bind}"))?;
    let addr = listener.local_addr().unwrap_or(bind);
    tracing::info!(%addr, "octos mcp-serve http bound (streamable HTTP, bearer required)");

    // Serve until Ctrl+C / SIGTERM. On shutdown, cancel the rmcp service so live
    // SSE sessions terminate immediately — otherwise axum's graceful drain would
    // block on those long-lived streams.
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        })
        .await
        .wrap_err("mcp-serve http server error")?;
    Ok(())
}

/// Builds without the `api` feature lack axum, so the HTTP transport is
/// unavailable; stdio still works.
#[cfg(not(feature = "api"))]
async fn serve_http(_server: McpServer, _bind: SocketAddr, _token: String) -> Result<()> {
    eyre::bail!(
        "the http transport for `octos mcp-serve` requires building octos with the `api` \
         feature (the canonical install includes it); use `--transport stdio` otherwise"
    )
}

// ---- M7.2a: real session dispatch ----

/// Runtime configuration for the session-level dispatch.
#[derive(Debug, Clone)]
pub struct SessionDispatchConfig {
    /// Working directory passed to every spawned [`Agent`]. This is the
    /// workspace root enforced by the workspace-contract checks.
    pub cwd: PathBuf,
    /// Data directory used for episode/memory persistence.
    pub data_dir: PathBuf,
    /// Maximum number of tool-call iterations per MCP session.
    pub max_iterations: u32,
    /// Sandbox policy applied to the per-session tool registry. Each MCP call
    /// runs agent tool calls (shell/exec/file) on behalf of an outer
    /// orchestrator that is only parent-trusted (stdio) or bearer-token
    /// authenticated (http) — never fully trusted. Confining shell/exec to the
    /// workspace via the OS sandbox is what stops `run_octos_session` from
    /// reading, writing, or executing outside `cwd`. Defaults to
    /// [`SandboxMode::Auto`](octos_agent::SandboxMode) via
    /// [`SandboxConfig::default`].
    pub sandbox: SandboxConfig,
    /// Operator-configured global tool deny/allow policy. Applied to the
    /// MCP-served registry so a server that denies e.g. `shell`/`bash` locally
    /// doesn't re-expose those tools to an external caller (parity with chat).
    pub tool_policy: Option<ToolPolicy>,
    /// Full per-provider tool-policy map (config `tool_policy_by_provider`),
    /// resolved per session against the built provider's `model_id()` — with a
    /// fallback to [`Self::provider_name`] — so a model-scoped deny is honoured
    /// even when the config relies on a provider default model.
    pub tool_policy_by_provider: HashMap<String, ToolPolicy>,
    /// Configured provider name, the fallback key when resolving the
    /// per-provider tool policy (model id wins over provider name).
    pub provider_name: String,
    /// Invocation-local output recovery policy. Each MCP call gets a distinct
    /// owner and may recover only outputs created during that call.
    pub output_recovery: octos_agent::output_recovery::OutputPolicy,
    /// Opt-in MCP completion repair policy, resolved once at dispatch setup.
    pub completion_repair: CompletionRepairPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionRepairPolicy {
    pub max_repair_rounds: u8,
}

impl CompletionRepairPolicy {
    pub const fn new(max_repair_rounds: u8) -> Self {
        Self {
            max_repair_rounds: if max_repair_rounds > 2 {
                2
            } else {
                max_repair_rounds
            },
        }
    }

    pub const fn enabled(self) -> bool {
        self.max_repair_rounds > 0
    }
}

impl SessionDispatchConfig {
    /// Build the sandbox backend for this session's tool registry, applying the
    /// MCP approval posture first. Returns the effective [`SandboxConfig`] and
    /// the constructed backend so the caller can fail closed when a sandbox was
    /// requested but no backend is available on this host.
    ///
    /// The MCP-served agent runs under [`ApprovalPolicy::Never`]: there is no
    /// interactive approver in server mode, so any tool call that would prompt
    /// fails at the tool boundary rather than silently proceeding. Auto mode
    /// resolves to `sandbox-exec` on macOS / `bwrap` on Linux.
    fn sandbox_backend(&self) -> (SandboxConfig, Box<dyn octos_agent::Sandbox>) {
        let permissions = self.permissions();
        let effective = permissions.apply_to_sandbox(&self.sandbox);
        let backend = create_sandbox(&effective);
        (effective, backend)
    }

    /// The MCP-served agent's effective permissions: workspace-write, but with
    /// a fail-closed approval policy (no interactive approver exists server-side).
    fn permissions(&self) -> EffectivePermissions {
        EffectivePermissions::workspace_write().with_approval_policy(ApprovalPolicy::Never)
    }
}

/// Factory that yields a ready-to-use LLM provider for each session.
///
/// Production builds build a provider from a [`Config`] file (matching the
/// chat/gateway code path). Tests inject a scripted provider so they never
/// need network access.
pub struct AgentLlmFactory {
    inner: AgentLlmFactoryKind,
}

enum AgentLlmFactoryKind {
    /// A provider that can be cloned (behind an Arc) for each session.
    Shared(Arc<dyn LlmProvider>),
    /// A lazy factory that reads config on demand. Used by the production
    /// `McpServeCommand::run_async` path.
    Config(Box<Config>),
}

impl AgentLlmFactory {
    /// Build from a loaded config — used by the production path.
    pub fn from_config(config: Config) -> Result<Self> {
        Ok(Self {
            inner: AgentLlmFactoryKind::Config(Box::new(config)),
        })
    }

    /// Build from a preconstructed provider — used by integration tests.
    pub fn scripted(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            inner: AgentLlmFactoryKind::Shared(provider),
        }
    }

    fn build_provider(&self) -> Result<Arc<dyn LlmProvider>, McpServerError> {
        match &self.inner {
            AgentLlmFactoryKind::Shared(p) => Ok(p.clone()),
            AgentLlmFactoryKind::Config(config) => {
                let provider_name = config
                    .provider
                    .clone()
                    .or_else(|| {
                        config
                            .model
                            .as_deref()
                            .and_then(crate::config::detect_provider)
                            .map(String::from)
                    })
                    .ok_or_else(|| {
                        McpServerError::SessionFailed(
                            "config_error: no LLM provider configured (set provider or model in config.json)".into(),
                        )
                    })?;
                let model = config.model.clone();
                let base_url = config.base_url.clone();
                super::chat::create_provider(&provider_name, config, model, base_url)
                    .map_err(|err| McpServerError::SessionFailed(format!("config_error: {err}")))
            }
        }
    }
}

/// Session dispatch that wires MCP calls into the real agent loop.
///
/// Each `run_session` call:
///
/// * Emits `Running` on the supplied observer.
/// * Builds a fresh [`Agent`] sharing the process-level LLM factory but with
///   per-call episode/memory state so sessions do not alias.
/// * Gives that invocation its own output-recovery owner. Saved output may be
///   recalled later in the same call, but is deliberately unavailable to a
///   later MCP invocation.
/// * Runs the supplied prompt as a [`Task`], which exercises the full
///   build-messages → call-llm → tool-use → end-turn loop.
/// * Emits `Verifying`, resolves the contract artifact (either the
///   `expected_artifact` field from the MCP input or the workspace contract's
///   primary artifact), and transitions to `Ready`/`Failed`.
///
/// # Sandboxing
///
/// The per-session [`ToolRegistry`] is built with
/// [`ToolRegistry::with_builtins_and_sandbox`] using the
/// [`SessionDispatchConfig::sandbox`] policy (default
/// [`SandboxMode::Auto`](octos_agent::SandboxMode)). This confines
/// `shell`/`exec_command`/`bash` and the file tools to the workspace `cwd`,
/// exactly as `octos chat`/`octos gateway` do. Without it the outer MCP caller
/// — which is only parent-trusted (stdio) or bearer-authenticated (http) — can
/// drive `run_octos_session` to read, write, and execute anywhere the octos
/// process can reach.
pub struct RealSessionDispatch {
    config: SessionDispatchConfig,
    factory: AgentLlmFactory,
}

impl RealSessionDispatch {
    /// Production constructor.
    pub fn new(config: SessionDispatchConfig, factory: AgentLlmFactory) -> Self {
        Self { config, factory }
    }

    /// Alias used by integration tests for visibility.
    pub fn new_for_test(config: SessionDispatchConfig, factory: AgentLlmFactory) -> Self {
        Self::new(config, factory)
    }

    /// Set the H06 repair switch for a locally constructed dispatch.
    pub fn with_completion_repair(mut self, enabled: bool) -> Self {
        self.config.completion_repair = CompletionRepairPolicy::new(if enabled { 2 } else { 0 });
        self
    }

    /// Inject an off, single-round, or two-round policy for a local dispatch.
    pub fn with_completion_repair_rounds(mut self, rounds: u8) -> Self {
        self.config.completion_repair = CompletionRepairPolicy::new(rounds);
        self
    }
}

#[async_trait]
impl McpSessionDispatch for RealSessionDispatch {
    async fn run_session(
        &self,
        contract: &str,
        input: &Value,
        observer: &dyn SessionLifecycleObserver,
    ) -> Result<McpSessionOutcome, McpServerError> {
        observer.mark_state(TaskLifecycleState::Running);

        // Native ARC input is parsed and workspace-validated before the LLM
        // provider or agent loop is constructed. If `arc_task` is absent, the
        // original free-form prompt path remains byte-for-byte compatible.
        let arc_request = match parse_arc_agent_task_input(input, &self.config.cwd) {
            Ok(request) => request,
            Err(error) => {
                observer.mark_state(TaskLifecycleState::Failed);
                return Err(McpServerError::InvalidParams(error.to_string()));
            }
        };
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Run the {contract} contract"));
        let expected_artifact = arc_request
            .as_ref()
            .map(|request| request.expected_artifact.clone())
            .or_else(|| {
                input
                    .get("expected_artifact")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
            });
        let artifact_name = arc_request
            .as_ref()
            .map(|request| request.artifact_name.as_str())
            .or_else(|| input.get("artifact_name").and_then(Value::as_str))
            .unwrap_or("primary");

        // Build the LLM provider and the per-session agent.
        let llm = self.factory.build_provider()?;
        let memory = Arc::new(
            EpisodeStore::open(&self.config.data_dir)
                .await
                .map_err(|err| {
                    McpServerError::SessionFailed(format!(
                        "config_error: open episode store: {err}"
                    ))
                })?,
        );
        // Confine shell/exec/file tools to the workspace. `with_builtins`
        // alone installs `NoSandbox`, which let an outer MCP caller drive
        // `shell`/`exec_command` to read, write, or execute anywhere the octos
        // process could (the M7.2 session-dispatch RCE). The OS sandbox
        // restricts writes to `cwd`.
        let permissions = self.config.permissions();
        let (effective_sandbox_config, sandbox) = self.config.sandbox_backend();
        // Fail closed: unlike local chat/gateway (where the human runs their own
        // commands), the mcp-serve caller is only parent-trusted (stdio) or
        // bearer-token authenticated (http). If the operator wanted a sandbox but
        // this host has no backend (Auto → NoSandbox), refuse the session rather
        // than silently running an external caller's tools unsandboxed. An
        // explicit `sandbox.mode = "none"` opt-out is respected.
        if let Some(refusal) = sandbox.refusal() {
            // The resolution already refused (an explicit mode unhonorable on
            // this host, or sandbox.fail_closed): refuse the session with the
            // typed remediation instead of building one whose every tool call
            // refuses one by one.
            return Err(McpServerError::SessionFailed(format!(
                "session_failed: {refusal}"
            )));
        }
        if effective_sandbox_config.enabled
            && effective_sandbox_config.mode != SandboxMode::None
            && sandbox.is_noop()
        {
            return Err(McpServerError::SessionFailed(
                "session_failed: no sandbox backend available on this host; refusing to run \
                 tools unsandboxed on the mcp-serve path (set sandbox.mode = \"none\" to opt out)"
                    .to_string(),
            ));
        }
        let mut registry =
            ToolRegistry::with_builtins_and_permissions(&self.config.cwd, sandbox, permissions);
        if self.config.output_recovery.enabled {
            registry.register(octos_agent::tools::RecallTool::for_output_recovery(
                self.config.output_recovery,
            ));
        }
        // Apply the operator's global tool deny/allow policy (parity with chat)
        // so a server that denies command tools doesn't re-expose them, then the
        // model-scoped policy resolved against the provider we actually built
        // (`llm.model_id()` — not the raw config model, which is empty when only
        // a provider is configured, so a policy keyed to the provider default
        // model would otherwise be skipped).
        if let Some(policy) = &self.config.tool_policy {
            registry.apply_policy(policy);
        }
        let provider_policy = self
            .config
            .tool_policy_by_provider
            .get(llm.model_id())
            .or_else(|| {
                self.config
                    .tool_policy_by_provider
                    .get(&self.config.provider_name)
            })
            .cloned();
        if let Some(policy) = provider_policy {
            registry.set_provider_policy(policy);
        }
        let output_recovery_visible = registry.is_tool_visible("recall");
        let tools = Arc::new(registry);
        let agent_config = AgentConfig {
            max_iterations: self.config.max_iterations,
            // Skip episode persistence — MCP sessions are short-lived and
            // the outer orchestrator owns durability. Writing episodes from
            // every MCP call would also bloat the memory store.
            save_episodes: false,
            ..Default::default()
        };
        let mut agent = Agent::new_shared(
            AgentId::new("mcp-serve"),
            llm.clone(),
            tools.clone(),
            memory,
        )
        .with_config(agent_config);
        let invocation_id = format!("mcp-{}", uuid::Uuid::now_v7());
        let file_state =
            match TaskFileState::for_local_workspace(&self.config.cwd).and_then(|task_state| {
                task_state
                    .for_branch(&invocation_id, &invocation_id, "root")
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "incomplete MCP file-state owner",
                        )
                    })
            }) {
                Ok(file_state) => Some(file_state),
                Err(error) => {
                    tracing::warn!(
                    invocation = %invocation_id,
                    workspace = %self.config.cwd.display(),
                    error = %error,
                    "MCP file state initialization failed; read deduplication remains disabled",
                    );
                    None
                }
            };
        let output_owner = file_state
            .as_ref()
            .and_then(|state| state.receipts().owner())
            .cloned();
        if let Some(file_state) = file_state {
            agent = agent.with_file_state(file_state);
        }
        if let Some(owner) = output_owner {
            let output_state = Arc::new(octos_agent::output_recovery::OutputState::new(
                self.config.output_recovery,
                owner,
            ));
            if output_recovery_visible
                && let Err(error) = output_state.enable_store(&self.config.data_dir)
            {
                tracing::warn!(
                    invocation = %invocation_id,
                    error = %error,
                    "MCP invocation output recovery unavailable",
                );
            }
            agent = agent.with_output_state(output_state);
        }
        if let Some(request) = &arc_request {
            agent = agent.with_system_prompt(request.task.system_prompt.clone());
        }

        // Review A F-004: propagate the workspace policy's declarative
        // compaction contract onto the MCP-served child Agent. Parity with
        // the local chat + session-actor wiring — the MCP-served child
        // session must honour the same preflight-token budget and preserved
        // artifacts declared in workspace_policy.toml.
        if let Ok(Some(workspace_policy)) = octos_agent::read_workspace_policy(&self.config.cwd) {
            if let Some(compaction_policy) = workspace_policy.compaction.clone() {
                use octos_agent::compaction::CompactionRunner;
                use octos_agent::workspace_policy::CompactionSummarizerKind;
                let runner = match compaction_policy.summarizer {
                    CompactionSummarizerKind::LlmIterative => {
                        CompactionRunner::with_provider(compaction_policy, llm.clone())
                    }
                    CompactionSummarizerKind::Extractive => {
                        CompactionRunner::new(compaction_policy)
                    }
                }
                .with_workspace_policy(&workspace_policy);
                agent = agent
                    .with_compaction_runner(Arc::new(runner))
                    .with_compaction_workspace(workspace_policy);
            }
        }

        // Run the task — this is the same code path the local chat command
        // uses. Any LLM/tool errors surface as `eyre::Report`.
        let task = if let Some(request) = &arc_request {
            Task::new(
                TaskKind::Custom {
                    name: ARC_AGENT_TASK_SCHEMA_V1.to_string(),
                    params: request.execution_params(),
                },
                TaskContext {
                    working_dir: self.config.cwd.clone(),
                    ..Default::default()
                },
            )
        } else {
            Task::new(
                TaskKind::Custom {
                    name: contract.to_string(),
                    params: input.clone(),
                },
                TaskContext {
                    working_dir: self.config.cwd.clone(),
                    working_memory: vec![octos_core::Message {
                        role: octos_core::MessageRole::User,
                        content: prompt,
                        media: vec![],
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                        client_message_id: None,
                        thread_id: None,
                        timestamp: chrono::Utc::now(),
                    }],
                    ..Default::default()
                },
            )
        };

        if self.config.completion_repair.enabled() {
            let protected = match ProtectedWorkspace::new(&self.config.cwd, &self.config.data_dir) {
                Ok(protected) => protected,
                Err(error) => {
                    tracing::warn!(
                        target: "h06_observation",
                        event = "repair_session_final",
                        invocation_id = %invocation_id,
                        task_id = %task.id,
                        decision = "terminal_failure",
                        stop_reason = "rollback_unavailable",
                        usage_status = "not_started",
                        "H06 repair workspace protection unavailable"
                    );
                    observer.mark_state(TaskLifecycleState::Failed);
                    return Ok(McpSessionOutcome {
                        final_state: TaskLifecycleState::Failed,
                        artifact_path: None,
                        artifact_content: None,
                        validator_results: Vec::new(),
                        cost: McpSessionCost::default(),
                        error: Some(format!("rollback_unavailable: {error}")),
                    });
                }
            };
            let gate = McpRepairGate {
                policy: self.config.completion_repair,
                contract,
                expected_artifact: expected_artifact.as_deref(),
                artifact_name,
                native_arc: arc_request.is_some(),
                response_schema: arc_request
                    .as_ref()
                    .and_then(|request| request.task.response_schema.as_ref()),
                tools: &tools,
                sandbox: &effective_sandbox_config,
                latest: Mutex::new(None),
                previous: Mutex::new(None),
                protected: &protected,
                best: Mutex::new(None),
            };
            let mut gated = match agent.run_task_with_completion_gate(&task, &gate).await {
                Ok(result) => result,
                Err(err) => {
                    let restore = gate.restore_best(None, None).await;
                    let candidate_revision = gate
                        .previous
                        .lock()
                        .expect("MCP progress mutex")
                        .as_ref()
                        .map(|previous| previous.receipt.candidate_revision)
                        .unwrap_or(0);
                    let partial = err.downcast_ref::<octos_agent::PartialTurnUsage>();
                    let cost = partial
                        .map(|usage| McpSessionCost::from(&usage.total))
                        .unwrap_or_default();
                    let usage_status = if partial.is_some() {
                        "partial"
                    } else {
                        "unknown; numeric_cost_is_compatibility_placeholder"
                    };
                    let restore_result = match &restore {
                        Some(Ok(_)) => "restored",
                        Some(Err(_)) => "failed",
                        None => "not_available",
                    };
                    tracing::warn!(
                        target: "h06_observation",
                        event = "repair_session_final",
                        invocation_id = %invocation_id,
                        task_id = %task.id,
                        candidate_revision,
                        decision = "provider_error",
                        stop_reason = "provider_error",
                        usage_status,
                        input_tokens = cost.input_tokens,
                        output_tokens = cost.output_tokens,
                        reasoning_tokens = cost.reasoning_tokens,
                        cache_read_tokens = cost.cache_read_tokens,
                        cache_write_tokens = cost.cache_write_tokens,
                        restore_result,
                        "H06 repair session stopped"
                    );
                    observer.mark_state(TaskLifecycleState::Failed);
                    return Ok(McpSessionOutcome {
                        final_state: TaskLifecycleState::Failed,
                        artifact_path: None,
                        artifact_content: None,
                        validator_results: Vec::new(),
                        cost,
                        error: Some(if let Some(Err(restore_error)) = restore {
                            format!(
                                "rollback_unavailable: {restore_error}; llm_error: {err}; usage_status={usage_status}"
                            )
                        } else {
                            format!("llm_error: {err}; usage_status={usage_status}")
                        }),
                    });
                }
            };
            let restored = if matches!(
                gated.decision,
                Some(CompletionDecision::TerminalFailure { .. })
            ) {
                let revision = gated
                    .decision
                    .as_ref()
                    .map(|decision| decision.receipt().candidate_revision.saturating_add(1));
                gate.restore_best(Some(&gated.task_result.token_usage), revision)
                    .await
            } else {
                None
            };
            let restored = match restored {
                Some(Ok((receipt, outcome))) => {
                    let reason = match gated.decision.as_ref() {
                        Some(CompletionDecision::TerminalFailure { reason, .. }) => *reason,
                        _ => unreachable!("restore follows a terminal completion decision"),
                    };
                    *gate.latest.lock().expect("MCP gate result mutex") =
                        Some((receipt.candidate_revision, outcome));
                    gated.decision = Some(CompletionDecision::TerminalFailure { reason, receipt });
                    true
                }
                Some(Err(error)) => {
                    let cost = McpSessionCost::from(&gated.task_result.token_usage);
                    tracing::warn!(
                        target: "h06_observation",
                        event = "repair_session_final",
                        invocation_id = %invocation_id,
                        task_id = %task.id,
                        candidate_revision = gated
                            .decision
                            .as_ref()
                            .map(|decision| decision.receipt().candidate_revision)
                            .unwrap_or(0),
                        decision = "terminal_failure",
                        stop_reason = "rollback_unavailable",
                        input_tokens = cost.input_tokens,
                        output_tokens = cost.output_tokens,
                        reasoning_tokens = cost.reasoning_tokens,
                        cache_read_tokens = cost.cache_read_tokens,
                        cache_write_tokens = cost.cache_write_tokens,
                        restore_result = "failed",
                        "H06 repair session stopped"
                    );
                    observer.mark_state(TaskLifecycleState::Failed);
                    return Ok(McpSessionOutcome {
                        final_state: TaskLifecycleState::Failed,
                        artifact_path: None,
                        artifact_content: None,
                        validator_results: Vec::new(),
                        cost,
                        error: Some(format!("rollback_unavailable: {error}")),
                    });
                }
                None => false,
            };
            if let Some(decision) = gated.decision.as_ref() {
                // A budget/cancel stop can carry the previous receipt, but it
                // has no verified final candidate. Never project that stale
                // artifact or its validator results as the final outcome.
                if restored
                    || !matches!(
                        decision,
                        CompletionDecision::TerminalFailure {
                            reason: TerminalReason::TaskBudgetExhausted | TerminalReason::Cancelled,
                            ..
                        }
                    )
                {
                    let revision = decision.receipt().candidate_revision;
                    let mut outcome = gate
                        .latest
                        .lock()
                        .expect("MCP gate result mutex")
                        .take()
                        .filter(|(checked_revision, _)| *checked_revision == revision)
                        .map(|(_, outcome)| outcome)
                        .expect("final gate decision must have its own projection");
                    let ready = matches!(decision, CompletionDecision::Pass(_))
                        && gated.task_result.success;
                    outcome.final_state = if ready {
                        TaskLifecycleState::Ready
                    } else {
                        TaskLifecycleState::Failed
                    };
                    if !ready {
                        outcome.artifact_path = None;
                        outcome.artifact_content = None;
                        if outcome.error.is_none() {
                            outcome.error = Some(
                                "contract_failed: completion gate rejected the final candidate"
                                    .into(),
                            );
                        }
                        if let CompletionDecision::TerminalFailure { reason, .. } = decision {
                            let label = format!("h06_terminal={}", terminal_reason_code(*reason));
                            outcome.error =
                                Some(format!("{}; {label}", outcome.error.unwrap_or_default()));
                        }
                    }
                    outcome.cost = McpSessionCost::from(&gated.task_result.token_usage);
                    if !matches!(
                        decision,
                        CompletionDecision::TerminalFailure {
                            reason: TerminalReason::TaskBudgetExhausted | TerminalReason::Cancelled,
                            ..
                        }
                    ) {
                        observer.mark_state(TaskLifecycleState::Verifying);
                    }
                    tracing::info!(
                        target: "h06_observation",
                        event = "repair_session_final",
                        invocation_id = %invocation_id,
                        task_id = %decision.receipt().task_id,
                        candidate_revision = decision.receipt().candidate_revision,
                        decision = decision_code(decision),
                        stop_reason = match decision {
                            CompletionDecision::Pass(_) => "ready",
                            CompletionDecision::Repairable { .. } => "invalid_repairable_final",
                            CompletionDecision::TerminalFailure { reason, .. } => {
                                terminal_reason_code(*reason)
                            }
                        },
                        input_tokens = outcome.cost.input_tokens,
                        output_tokens = outcome.cost.output_tokens,
                        reasoning_tokens = outcome.cost.reasoning_tokens,
                        cache_read_tokens = outcome.cost.cache_read_tokens,
                        cache_write_tokens = outcome.cost.cache_write_tokens,
                        restored,
                        "H06 repair session completed"
                    );
                    observer.mark_state(outcome.final_state);
                    return Ok(outcome);
                }
            }
            let detail = gated.task_result.output.trim();
            let error = if detail.is_empty() {
                "session_failed: agent task reported unsuccessful completion without an explanatory message".to_string()
            } else {
                format!("session_failed: agent task reported unsuccessful completion: {detail}")
            };
            let final_state = if matches!(
                gated.decision,
                Some(CompletionDecision::TerminalFailure {
                    reason: TerminalReason::Cancelled,
                    ..
                })
            ) {
                TaskLifecycleState::Cancelled
            } else {
                TaskLifecycleState::Failed
            };
            let outcome = McpSessionOutcome {
                final_state,
                artifact_path: None,
                artifact_content: None,
                validator_results: Vec::new(),
                cost: McpSessionCost::from(&gated.task_result.token_usage),
                error: Some(error),
            };
            tracing::info!(
                target: "h06_observation",
                event = "repair_session_final",
                invocation_id = %invocation_id,
                task_id = %task.id,
                decision = "task_failed",
                stop_reason = if final_state == TaskLifecycleState::Cancelled {
                    "cancelled"
                } else {
                    "task_failed"
                },
                input_tokens = outcome.cost.input_tokens,
                output_tokens = outcome.cost.output_tokens,
                reasoning_tokens = outcome.cost.reasoning_tokens,
                cache_read_tokens = outcome.cost.cache_read_tokens,
                cache_write_tokens = outcome.cost.cache_write_tokens,
                "H06 repair session completed"
            );
            observer.mark_state(final_state);
            return Ok(outcome);
        }

        let task_result = match agent.run_task(&task).await {
            Ok(result) => result,
            Err(err) => {
                observer.mark_state(TaskLifecycleState::Failed);
                return Ok(McpSessionOutcome {
                    final_state: TaskLifecycleState::Failed,
                    artifact_path: None,
                    artifact_content: None,
                    validator_results: Vec::new(),
                    cost: McpSessionCost::default(),
                    error: Some(format!("llm_error: {err}")),
                });
            }
        };

        // `run_task` reports budget exhaustion and other soft terminal failures
        // as `Ok(TaskResult { success: false, .. })`. Do not verify a pre-existing
        // artifact in that case: accepting stale output would turn an
        // unsuccessful ARC attempt into Ready.
        if !task_result.success {
            observer.mark_state(TaskLifecycleState::Failed);
            let detail = task_result.output.trim();
            let error = if detail.is_empty() {
                "session_failed: agent task reported unsuccessful completion without an explanatory message"
                    .to_string()
            } else {
                format!("session_failed: agent task reported unsuccessful completion: {detail}")
            };
            return Ok(McpSessionOutcome {
                final_state: TaskLifecycleState::Failed,
                artifact_path: None,
                artifact_content: None,
                validator_results: Vec::new(),
                cost: McpSessionCost::from(&task_result.token_usage),
                error: Some(error),
            });
        }

        observer.mark_state(TaskLifecycleState::Verifying);

        let gate = run_completion_gate(CompletionGateInput {
            candidate: CompletionCandidate {
                task_id: task.id.clone(),
                working_dir: self.config.cwd.clone(),
                proposed_output: task_result.output,
                files_modified: task_result.files_modified,
                files_to_send: task_result.files_to_send,
                iteration: 0,
                cumulative_usage: task_result.token_usage,
                revision: 1,
            },
            contract,
            expected_artifact: expected_artifact.as_deref(),
            artifact_name,
            native_arc: arc_request.is_some(),
            response_schema: arc_request
                .as_ref()
                .and_then(|request| request.task.response_schema.as_ref()),
            tools: &tools,
            sandbox: &effective_sandbox_config,
        })
        .await;
        let ready = matches!(gate.decision, CompletionDecision::Pass(_));
        debug_assert_eq!(ready, gate.outcome.final_state == TaskLifecycleState::Ready);
        observer.mark_state(gate.outcome.final_state);
        Ok(gate.outcome)
    }
}

/// The H06 gate lives only for one MCP invocation. Its last projection is
/// matched to the final candidate revision before it becomes an MCP outcome.
struct McpRepairGate<'a> {
    policy: CompletionRepairPolicy,
    contract: &'a str,
    expected_artifact: Option<&'a Path>,
    artifact_name: &'a str,
    native_arc: bool,
    response_schema: Option<&'a Value>,
    tools: &'a Arc<ToolRegistry>,
    sandbox: &'a SandboxConfig,
    latest: Mutex<Option<(u64, McpSessionOutcome)>>,
    previous: Mutex<Option<PreviousCandidate>>,
    protected: &'a ProtectedWorkspace,
    best: Mutex<Option<BestCandidate>>,
}

struct BestCandidate {
    candidate: CompletionCandidate,
    snapshot: SnapshotId,
    manifest: WorkspaceManifest,
}

fn terminal_reason_code(reason: TerminalReason) -> &'static str {
    match reason {
        TerminalReason::RepairRoundLimit => "repair_round_limit",
        TerminalReason::NoWorkspaceChange => "no_workspace_change",
        TerminalReason::UnchangedFailure => "unchanged_failure",
        TerminalReason::Regressed => "regressed",
        TerminalReason::TaskBudgetExhausted => "task_budget_exhausted",
        TerminalReason::GateTimeout => "gate_timeout",
        TerminalReason::GateError => "gate_error",
        TerminalReason::Cancelled => "cancelled",
        TerminalReason::RollbackUnavailable => "rollback_unavailable",
        TerminalReason::StaleCandidate => "stale_candidate",
    }
}

fn decision_code(decision: &CompletionDecision) -> &'static str {
    match decision {
        CompletionDecision::Pass(_) => "pass",
        CompletionDecision::Repairable { .. } => "repairable",
        CompletionDecision::TerminalFailure { .. } => "terminal_failure",
    }
}

fn progress_code(progress: Option<ProgressObservation>) -> &'static str {
    match progress {
        None => "initial",
        Some(ProgressObservation::Improved) => "improved",
        Some(ProgressObservation::StrategyChange) => "strategy_change",
        Some(ProgressObservation::EvidenceChanged) => "evidence_changed",
        Some(ProgressObservation::NoWorkspaceChange) => "no_workspace_change",
        Some(ProgressObservation::Regressed) => "regressed",
    }
}

fn observation_facts(receipt: &CompletionReceipt) -> (usize, usize, String, String) {
    let mut hard_passes = 0usize;
    let mut hard_failures = Vec::new();
    let mut categories = BTreeSet::new();
    for check in &receipt.checks {
        match check {
            CheckOutcome::Validator(outcome) if outcome.required => {
                if outcome.status == ValidatorStatus::Pass {
                    hard_passes += 1;
                } else {
                    hard_failures.push(check.clone());
                    categories.insert(match outcome.status {
                        ValidatorStatus::Fail => "validator_fail",
                        ValidatorStatus::Timeout => "validator_timeout",
                        ValidatorStatus::Error => "validator_error",
                        ValidatorStatus::Pass => unreachable!(),
                    });
                }
            }
            CheckOutcome::Artifact(outcome) => {
                if outcome.status == ValidatorStatus::Pass {
                    hard_passes += 1;
                } else {
                    hard_failures.push(check.clone());
                    categories.insert(match outcome.reason_code {
                        ArtifactReasonCode::Missing => "artifact_missing",
                        ArtifactReasonCode::UnsafeLocation => "artifact_unsafe_location",
                        ArtifactReasonCode::TooLarge => "artifact_too_large",
                        ArtifactReasonCode::InvalidUtf8 => "artifact_invalid_utf8",
                        ArtifactReasonCode::InvalidJson => "artifact_invalid_json",
                        ArtifactReasonCode::SchemaMismatch => "artifact_schema_mismatch",
                        ArtifactReasonCode::Other => "artifact_other",
                    });
                }
            }
            CheckOutcome::Validator(_) => {}
        }
    }
    (
        hard_passes,
        hard_failures.len(),
        categories.into_iter().collect::<Vec<_>>().join(","),
        failure_signature(&hard_failures),
    )
}

fn stop_code(decision: &CompletionDecision, repair_rounds_sent: u8) -> &'static str {
    match decision {
        CompletionDecision::Pass(_) if repair_rounds_sent == 0 => "initial_ready",
        CompletionDecision::Pass(_) if repair_rounds_sent == 1 => "first_round_ready",
        CompletionDecision::Pass(_) => "second_round_ready",
        CompletionDecision::Repairable { .. } => "repair_requested",
        CompletionDecision::TerminalFailure { reason, .. } => terminal_reason_code(*reason),
    }
}

fn emit_gate_observation(
    candidate: &CompletionCandidate,
    decision: &CompletionDecision,
    repair_rounds_sent: u8,
    progress: Option<ProgressObservation>,
    workspace_changed: bool,
    gate_duration_ms: u64,
) {
    let (hard_pass_count, hard_fail_count, failure_categories, signature) =
        observation_facts(decision.receipt());
    let (ticket_visible_bytes, evidence_reference_count) = match decision {
        CompletionDecision::Repairable { ticket, .. } => (
            ticket
                .render(octos_agent::completion_gate::TICKET_BYTES)
                .len(),
            ticket.validator_references.len(),
        ),
        _ => (0, 0),
    };
    tracing::info!(
        target: "h06_observation",
        event = "completion_gate",
        task_id = %candidate.task_id,
        candidate_revision = candidate.revision,
        round = repair_rounds_sent,
        decision = decision_code(decision),
        stop_reason = stop_code(decision, repair_rounds_sent),
        progress = progress_code(progress),
        failure_categories = %failure_categories,
        hard_pass_count,
        hard_fail_count,
        signature = %signature,
        gate_duration_ms,
        ticket_visible_bytes,
        evidence_reference_count,
        workspace_changed,
        converted_to_ready = repair_rounds_sent > 0
            && matches!(decision, CompletionDecision::Pass(_)),
        success_round = if matches!(decision, CompletionDecision::Pass(_)) {
            repair_rounds_sent
        } else {
            0
        },
        input_tokens = candidate.cumulative_usage.input_tokens,
        output_tokens = candidate.cumulative_usage.output_tokens,
        reasoning_tokens = candidate.cumulative_usage.reasoning_tokens,
        cache_read_tokens = candidate.cumulative_usage.cache_read_tokens,
        cache_write_tokens = candidate.cumulative_usage.cache_write_tokens,
        "H06 completion gate observation"
    );
}

impl McpRepairGate<'_> {
    /// Restore a saved failing candidate and build the final projection only
    /// from checks rerun against those restored bytes. A failed restore never
    /// produces an artifact or a Ready result.
    async fn restore_best(
        &self,
        usage: Option<&octos_core::TokenUsage>,
        revision: Option<u64>,
    ) -> Option<Result<(CompletionReceipt, McpSessionOutcome)>> {
        let best = self.best.lock().expect("MCP best candidate mutex").take()?;
        let restore_started = Instant::now();
        let task_id = best.candidate.task_id.clone();
        let saved_revision = best.candidate.revision;
        let restored_revision = revision.unwrap_or_else(|| saved_revision.saturating_add(1));
        let result = async {
            self.protected.restore(best.snapshot, best.manifest).await?;
            let mut candidate = best.candidate;
            candidate.revision = restored_revision;
            if let Some(usage) = usage {
                candidate.cumulative_usage = usage.clone();
            }
            let _ = octos_agent::workspace_contract::run_project_root_validators(
                self.tools.as_ref(),
                &candidate.working_dir,
                None,
                &candidate.files_to_send,
                self.tools.sandbox(),
            )
            .await;
            let core_failed =
                octos_agent::workspace_git::inspect_workspace_contracts(&candidate.working_dir)?
                    .iter()
                    .any(|status| status.policy_managed && !status.ready);
            let mut result = run_completion_gate(CompletionGateInput {
                candidate,
                contract: self.contract,
                expected_artifact: self.expected_artifact,
                artifact_name: self.artifact_name,
                native_arc: self.native_arc,
                response_schema: self.response_schema,
                tools: self.tools,
                sandbox: self.sandbox,
            })
            .await;
            if core_failed {
                result.outcome.error =
                    Some("contract_failed: restored workspace contract failed".into());
            }
            let mut receipt = result.decision.receipt().clone();
            if core_failed {
                let mut check = artifact_check(
                    Path::new(""),
                    ArtifactCheckKind::Location,
                    ValidatorStatus::Error,
                    ArtifactReasonCode::Other,
                    "project workspace contract did not pass",
                    false,
                );
                if let CheckOutcome::Artifact(ref mut artifact) = check {
                    artifact.gate_id = "core/workspace_contract".into();
                }
                receipt.checks.push(check);
            }
            sanitize_repair_receipt(&mut receipt, &self.protected.root);
            result.outcome.final_state = TaskLifecycleState::Failed;
            result.outcome.artifact_path = None;
            result.outcome.artifact_content = None;
            Ok((receipt, result.outcome))
        }
        .await;
        match &result {
            Ok(_) => tracing::info!(
                target: "h06_observation",
                event = "candidate_restore",
                task_id = %task_id,
                saved_revision,
                restored_revision,
                restore_result = "restored",
                restore_duration_ms = restore_started.elapsed().as_millis() as u64,
                "H06 candidate restore observation"
            ),
            Err(_) => tracing::warn!(
                target: "h06_observation",
                event = "candidate_restore",
                task_id = %task_id,
                saved_revision,
                restored_revision,
                restore_result = "failed",
                restore_duration_ms = restore_started.elapsed().as_millis() as u64,
                "H06 candidate restore observation"
            ),
        }
        Some(result)
    }
}

struct PreviousCandidate {
    receipt: CompletionReceipt,
    workspace_fingerprint: Option<[u8; 32]>,
}

#[async_trait]
impl CompletionGate for McpRepairGate<'_> {
    async fn verify(
        &self,
        candidate: &CompletionCandidate,
        core_contract_failure: Option<&str>,
        repair_rounds_sent: u8,
    ) -> CompletionDecision {
        let gate_started = Instant::now();
        let mut result = run_completion_gate(CompletionGateInput {
            candidate: candidate.clone(),
            contract: self.contract,
            expected_artifact: self.expected_artifact,
            artifact_name: self.artifact_name,
            native_arc: self.native_arc,
            response_schema: self.response_schema,
            tools: self.tools,
            sandbox: self.sandbox,
        })
        .await;
        let mut receipt = result.decision.receipt().clone();
        if core_contract_failure.is_some() {
            // The Agent's project-root contract is a hard gate for this same
            // candidate. Its raw diagnostics can contain workspace paths, so
            // the repair-facing receipt gets a fixed, nonrepairable check.
            let mut check = artifact_check(
                Path::new(""),
                ArtifactCheckKind::Location,
                ValidatorStatus::Error,
                ArtifactReasonCode::Other,
                "project workspace contract did not pass",
                false,
            );
            if let CheckOutcome::Artifact(ref mut artifact) = check {
                artifact.gate_id = "core/workspace_contract".into();
            }
            receipt.checks.push(check);
            result.outcome.error =
                Some("contract_failed: project workspace contract did not pass".into());
        }
        let progress_receipt = receipt.clone();
        sanitize_repair_receipt(&mut receipt, &candidate.working_dir);
        let workspace_scan = WorkspaceManifest::scan(&self.protected.root);
        let workspace_fingerprint = workspace_scan
            .as_ref()
            .ok()
            .and_then(|_| workspace_fingerprint(candidate));
        let (progress, workspace_changed) = {
            let previous = self.previous.lock().expect("MCP progress mutex");
            previous.as_ref().map_or((None, false), |old| {
                let changed = old
                    .workspace_fingerprint
                    .zip(workspace_fingerprint)
                    .is_some_and(|(before, after)| before != after);
                (
                    Some(observe_progress(&old.receipt, &progress_receipt, changed)),
                    changed,
                )
            })
        };
        let decision = if repair_allowed(&receipt) {
            classify(
                candidate,
                receipt,
                repair_rounds_sent.saturating_add(1),
                self.policy.max_repair_rounds,
            )
        } else {
            match classify(
                candidate,
                receipt,
                repair_rounds_sent.saturating_add(1),
                self.policy.max_repair_rounds,
            ) {
                CompletionDecision::Repairable { receipt, .. } => {
                    CompletionDecision::TerminalFailure {
                        reason: TerminalReason::GateError,
                        receipt,
                    }
                }
                other => other,
            }
        };
        let mut decision = match (decision, progress) {
            (
                CompletionDecision::Repairable { receipt, .. },
                Some(ProgressObservation::Regressed),
            )
            | (
                CompletionDecision::TerminalFailure {
                    reason: TerminalReason::RepairRoundLimit,
                    receipt,
                },
                Some(ProgressObservation::Regressed),
            ) => CompletionDecision::TerminalFailure {
                reason: TerminalReason::Regressed,
                receipt,
            },
            (
                CompletionDecision::Repairable { receipt, .. },
                Some(ProgressObservation::NoWorkspaceChange),
            )
            | (
                CompletionDecision::TerminalFailure {
                    reason: TerminalReason::RepairRoundLimit,
                    receipt,
                },
                Some(ProgressObservation::NoWorkspaceChange),
            ) => CompletionDecision::TerminalFailure {
                reason: TerminalReason::NoWorkspaceChange,
                receipt,
            },
            (
                CompletionDecision::Repairable { receipt, .. },
                Some(ProgressObservation::EvidenceChanged),
            ) if !workspace_changed => CompletionDecision::TerminalFailure {
                reason: TerminalReason::UnchangedFailure,
                receipt,
            },
            (
                CompletionDecision::TerminalFailure {
                    reason: TerminalReason::RepairRoundLimit,
                    receipt,
                },
                Some(ProgressObservation::StrategyChange),
            ) => CompletionDecision::TerminalFailure {
                reason: TerminalReason::UnchangedFailure,
                receipt,
            },
            (other, _) => other,
        };
        {
            let mut previous = self.previous.lock().expect("MCP progress mutex");
            *previous = Some(PreviousCandidate {
                receipt: progress_receipt,
                workspace_fingerprint,
            });
        }
        if workspace_fingerprint.is_none() {
            result.outcome.error = Some(match workspace_scan {
                Err(error) => format!("rollback_unavailable: {error}"),
                Ok(_) => "rollback_unavailable: workspace progress cannot be verified".into(),
            });
            decision = CompletionDecision::TerminalFailure {
                reason: TerminalReason::RollbackUnavailable,
                receipt: decision.receipt().clone(),
            };
        } else if (repair_rounds_sent == 0
            && matches!(decision, CompletionDecision::Repairable { .. }))
            || (matches!(progress, Some(ProgressObservation::Improved))
                && !matches!(decision, CompletionDecision::Pass(_)))
        {
            match self.protected.save("h06-best-candidate").await {
                Ok((snapshot, manifest)) => {
                    *self.best.lock().expect("MCP best candidate mutex") = Some(BestCandidate {
                        candidate: candidate.clone(),
                        snapshot,
                        manifest,
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        target: "h06_observation",
                        event = "candidate_snapshot",
                        task_id = %candidate.task_id,
                        candidate_revision = candidate.revision,
                        snapshot_result = "failed",
                        "H06 candidate snapshot unavailable"
                    );
                    result.outcome.error = Some(format!("rollback_unavailable: {error}"));
                    decision = CompletionDecision::TerminalFailure {
                        reason: TerminalReason::RollbackUnavailable,
                        receipt: decision.receipt().clone(),
                    };
                }
            }
        }
        *self.latest.lock().expect("MCP gate result mutex") =
            Some((candidate.revision, result.outcome));
        emit_gate_observation(
            candidate,
            &decision,
            repair_rounds_sent,
            progress,
            workspace_changed,
            gate_started.elapsed().as_millis() as u64,
        );
        decision
    }
}

/// Only the first H06 repair classes may enter a second provider request.
fn repair_allowed(receipt: &CompletionReceipt) -> bool {
    receipt
        .checks
        .iter()
        .filter(|check| match check {
            CheckOutcome::Validator(v) => !v.required_gate_passed(),
            CheckOutcome::Artifact(a) => a.status != ValidatorStatus::Pass,
        })
        .all(|check| match check {
            CheckOutcome::Validator(v) => v.required && v.status == ValidatorStatus::Fail,
            CheckOutcome::Artifact(a) => {
                a.status == ValidatorStatus::Fail
                    && match a.reason_code {
                        ArtifactReasonCode::Missing
                        | ArtifactReasonCode::InvalidJson
                        | ArtifactReasonCode::SchemaMismatch => true,
                        ArtifactReasonCode::UnsafeLocation => a.safe_target,
                        _ => false,
                    }
            }
        })
}

/// Hash the actual bytes of files reported by H05, including delivered files.
/// Unknown or unsafe paths cannot establish progress for a second ticket.
fn workspace_fingerprint(candidate: &CompletionCandidate) -> Option<[u8; 32]> {
    const MAX_HASH_BYTES: u64 = 64 * 1024 * 1024;
    let mut paths = BTreeSet::new();
    for path in candidate
        .files_modified
        .iter()
        .chain(&candidate.files_to_send)
    {
        let relative = if path.is_absolute() {
            path.strip_prefix(&candidate.working_dir).ok()?
        } else {
            path.as_path()
        };
        if !relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            return None;
        }
        paths.insert(relative.to_path_buf());
    }
    let mut digest = Sha256::new();
    for relative in paths {
        let name = relative.to_string_lossy();
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        let path = candidate.working_dir.join(&relative);
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => digest.update(b"missing"),
            Err(_) => return None,
            Ok(metadata) if metadata.file_type().is_file() && metadata.len() <= MAX_HASH_BYTES => {
                digest.update(b"file");
                let mut file = std::fs::File::open(&path).ok()?;
                let mut buf = [0u8; 8192];
                loop {
                    let count = file.read(&mut buf).ok()?;
                    if count == 0 {
                        break;
                    }
                    digest.update(&buf[..count]);
                }
            }
            Ok(_) => return None,
        }
    }
    Some(digest.finalize().into())
}

/// H06's opt-in scope is a disposable directory below the OS temp root. The
/// operator owns that directory for this invocation. We reject filesystem
/// features Git snapshots cannot round-trip and compare the recorded tree
/// with an independent byte-level manifest before trusting a snapshot.
struct ProtectedWorkspace {
    root: PathBuf,
    snapshots: Arc<SnapshotManager>,
    _lock: File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceManifest {
    files: BTreeMap<PathBuf, ([u8; 32], bool)>,
    dirs: BTreeSet<PathBuf>,
}

impl WorkspaceManifest {
    fn scan(root: &Path) -> Result<Self> {
        #[cfg(not(unix))]
        eyre::bail!("H06 protected repair requires Unix file-link checks");
        let mut manifest = Self {
            files: BTreeMap::new(),
            dirs: BTreeSet::new(),
        };
        let mut pending = vec![(root.to_path_buf(), PathBuf::new())];
        let mut total_bytes = 0u64;
        while let Some((directory, relative_dir)) = pending.pop() {
            for item in std::fs::read_dir(&directory)? {
                let item = item?;
                let name = item.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| eyre::eyre!("H06 workspace contains a non-UTF-8 name"))?;
                if matches!(
                    name,
                    ".git" | ".gitignore" | ".gitattributes" | ".gitmodules"
                ) {
                    eyre::bail!("H06 workspace contains Git metadata or ignore rules");
                }
                let relative = relative_dir.join(name);
                let metadata = std::fs::symlink_metadata(item.path())?;
                if metadata.file_type().is_symlink() {
                    eyre::bail!("H06 workspace contains a symlink");
                }
                if metadata.is_dir() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if metadata.permissions().mode() & 0o777 != 0o755 {
                            eyre::bail!("H06 workspace has a directory mode Git cannot round-trip");
                        }
                    }
                    manifest.dirs.insert(relative.clone());
                    pending.push((item.path(), relative));
                } else if metadata.is_file() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::{MetadataExt, PermissionsExt};
                        if metadata.nlink() != 1 {
                            eyre::bail!("H06 workspace contains a hard link");
                        }
                        let mode = metadata.permissions().mode() & 0o777;
                        if !matches!(mode, 0o644 | 0o755) {
                            eyre::bail!("H06 workspace has a file mode Git cannot round-trip");
                        }
                    }
                    if metadata.len() > 64 * 1024 * 1024 {
                        eyre::bail!("H06 workspace file exceeds snapshot limit");
                    }
                    total_bytes = total_bytes.saturating_add(metadata.len());
                    if total_bytes > 256 * 1024 * 1024 || manifest.files.len() >= 10_000 {
                        eyre::bail!("H06 workspace exceeds snapshot limit");
                    }
                    let mut hash = Sha256::new();
                    let mut file = File::open(item.path())?;
                    let mut chunk = [0u8; 8192];
                    loop {
                        let count = file.read(&mut chunk)?;
                        if count == 0 {
                            break;
                        }
                        hash.update(&chunk[..count]);
                    }
                    #[cfg(unix)]
                    let executable = {
                        use std::os::unix::fs::PermissionsExt;
                        metadata.permissions().mode() & 0o111 != 0
                    };
                    #[cfg(not(unix))]
                    let executable = false;
                    manifest
                        .files
                        .insert(relative, (hash.finalize().into(), executable));
                } else {
                    eyre::bail!("H06 workspace contains a special file");
                }
            }
        }
        Ok(manifest)
    }
}

impl ProtectedWorkspace {
    fn new(workspace: &Path, data_dir: &Path) -> Result<Self> {
        let root = workspace.canonicalize()?;
        let temp_root = std::env::temp_dir().canonicalize()?;
        if root == temp_root || !root.starts_with(&temp_root) {
            eyre::bail!("H06 repair requires a disposable workspace below the OS temp directory");
        }
        std::fs::create_dir_all(data_dir)?;
        let data = data_dir.canonicalize()?;
        if data.starts_with(&root) || root.starts_with(&data) {
            eyre::bail!("H06 snapshot data must be outside the disposable workspace");
        }
        let lock_root = temp_root.join("octos-h06-workspace-locks");
        std::fs::create_dir_all(&lock_root)?;
        if root.starts_with(&lock_root) {
            eyre::bail!("H06 workspace overlaps its lock directory");
        }
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_root.join(format!(
                "{}.lock",
                octos_agent::snapshot::workspace_hash(&root)
            )))?;
        lock.try_lock_exclusive()
            .wrap_err("H06 workspace is already in use")?;
        WorkspaceManifest::scan(&root)?;
        if let Some(policy) = octos_agent::read_workspace_policy(&root)? {
            use octos_agent::workspace_policy::ValidatorSpec;
            if policy
                .validation
                .validators
                .iter()
                .any(|validator| !matches!(&validator.spec, ValidatorSpec::FileExists { .. }))
            {
                eyre::bail!("H06 protected repair supports only file_exists validators");
            }
        }
        let snapshots = SnapshotManager::new(data.join("h06-snapshots"), &root, 4)
            .ok_or_else(|| eyre::eyre!("Git is required for H06 protected repair"))?;
        Ok(Self {
            root,
            snapshots: Arc::new(snapshots),
            _lock: lock,
        })
    }

    async fn save(&self, label: &str) -> Result<(SnapshotId, WorkspaceManifest)> {
        let before = WorkspaceManifest::scan(&self.root)?;
        let id = self.snapshots.take_snapshot_async(label).await?;
        let manager = Arc::clone(&self.snapshots);
        let id_for_list = id.clone();
        let recorded =
            tokio::task::spawn_blocking(move || manager.snapshot_paths(&id_for_list)).await??;
        let expected: BTreeSet<_> = before.files.keys().cloned().collect();
        if recorded.into_iter().collect::<BTreeSet<_>>() != expected
            || WorkspaceManifest::scan(&self.root)? != before
        {
            eyre::bail!("H06 snapshot did not cover a stable workspace");
        }
        Ok((id, before))
    }

    async fn restore(&self, id: SnapshotId, manifest: WorkspaceManifest) -> Result<()> {
        let manager = Arc::clone(&self.snapshots);
        tokio::task::spawn_blocking(move || manager.restore(&id)).await??;
        // Git stores files but omits empty directories. Reconcile directory
        // names only after confirming every file was restored, then compare
        // the complete manifest. Any nonempty extra directory fails closed.
        let observed = WorkspaceManifest::scan(&self.root)?;
        if observed.files != manifest.files {
            eyre::bail!("H06 restored files differ from saved candidate");
        }
        let mut extra: Vec<_> = observed.dirs.difference(&manifest.dirs).cloned().collect();
        extra.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in extra {
            std::fs::remove_dir(self.root.join(path))?;
        }
        let mut missing: Vec<_> = manifest.dirs.difference(&observed.dirs).cloned().collect();
        missing.sort_by_key(|path| path.components().count());
        for path in missing {
            std::fs::create_dir(self.root.join(path))?;
        }
        if WorkspaceManifest::scan(&self.root)? != manifest {
            eyre::bail!("H06 restored workspace differs from saved candidate");
        }
        Ok(())
    }
}

/// Tickets have a small, controlled diagnostic vocabulary. Full validator
/// output remains in the typed MCP result, never in the model-facing ticket.
fn sanitize_repair_receipt(receipt: &mut CompletionReceipt, workspace: &Path) {
    for check in &mut receipt.checks {
        match check {
            CheckOutcome::Validator(v) => {
                v.validator_id = ticket_label(&v.validator_id);
                v.kind = ticket_label(&v.kind);
                if v.status != ValidatorStatus::Pass {
                    v.reason = format!(
                        "required validator {}",
                        match v.status {
                            ValidatorStatus::Fail => "failed",
                            ValidatorStatus::Timeout => "timed out",
                            ValidatorStatus::Error => "could not run",
                            ValidatorStatus::Pass => unreachable!(),
                        }
                    );
                    v.stderr = None;
                }
            }
            CheckOutcome::Artifact(a) => {
                a.expected_artifact = a
                    .expected_artifact
                    .as_deref()
                    .and_then(|path| path.strip_prefix(workspace).ok())
                    .filter(|path| {
                        path.components()
                            .all(|part| matches!(part, std::path::Component::Normal(_)))
                    })
                    .map(Path::to_path_buf);
                a.observed_artifact = a
                    .observed_artifact
                    .as_deref()
                    .and_then(|path| path.strip_prefix(workspace).ok())
                    .filter(|path| {
                        path.components()
                            .all(|part| matches!(part, std::path::Component::Normal(_)))
                    })
                    .map(Path::to_path_buf);
                if a.status != ValidatorStatus::Pass {
                    if a.reason_code == ArtifactReasonCode::SchemaMismatch {
                        a.schema_pointer = a
                            .reason
                            .split_whitespace()
                            .find(|part| part.starts_with('$'))
                            .map(|part| part.trim_end_matches([':', ',', ';']))
                            .filter(|part| {
                                part.len() <= 96
                                    && part.bytes().all(|byte| {
                                        byte.is_ascii_alphanumeric() || b"$._[]".contains(&byte)
                                    })
                            })
                            .map(str::to_string);
                    }
                    a.reason = match a.reason_code {
                        ArtifactReasonCode::Missing => "expected artifact is missing",
                        ArtifactReasonCode::InvalidJson => "artifact is not valid JSON",
                        ArtifactReasonCode::SchemaMismatch => {
                            "artifact does not match the response schema"
                        }
                        ArtifactReasonCode::UnsafeLocation => "artifact location is unsafe",
                        ArtifactReasonCode::TooLarge => "artifact exceeds inline size limit",
                        ArtifactReasonCode::InvalidUtf8 => "artifact is not UTF-8 text",
                        ArtifactReasonCode::Other => "artifact check could not pass",
                    }
                    .into();
                    a.stderr = None;
                }
            }
        }
    }
}

fn ticket_label(value: &str) -> String {
    if !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        value.to_string()
    } else {
        "<redacted>".into()
    }
}

struct CompletionGateInput<'a> {
    candidate: CompletionCandidate,
    contract: &'a str,
    expected_artifact: Option<&'a Path>,
    artifact_name: &'a str,
    native_arc: bool,
    response_schema: Option<&'a Value>,
    tools: &'a Arc<ToolRegistry>,
    sandbox: &'a SandboxConfig,
}

struct CompletionGateResult {
    decision: CompletionDecision,
    outcome: McpSessionOutcome,
}

struct GateArtifact {
    state: ArtifactState,
    content: Option<String>,
    checks: Vec<CheckOutcome>,
    error: Option<String>,
}

async fn run_completion_gate(input: CompletionGateInput<'_>) -> CompletionGateResult {
    let candidate = &input.candidate;
    let validators = run_completion_validators(
        &candidate.working_dir,
        input.contract,
        input.tools,
        input.sandbox,
    )
    .await;
    let path = resolve_artifact_path(
        &candidate.working_dir,
        input.expected_artifact,
        input.artifact_name,
        &candidate.files_to_send,
    );
    complete_candidate_verification(
        candidate,
        validators,
        path,
        input.native_arc,
        input.response_schema,
    )
}

fn complete_candidate_verification(
    candidate: &CompletionCandidate,
    validators: Vec<ValidatorOutcome>,
    path: Option<PathBuf>,
    native_arc: bool,
    response_schema: Option<&Value>,
) -> CompletionGateResult {
    let required_failure = validators
        .iter()
        .any(|outcome| !outcome.required_gate_passed());
    let artifact = if required_failure {
        GateArtifact {
            state: ArtifactState::Unchecked,
            content: None,
            checks: Vec::new(),
            error: Some("contract_failed: required completion-phase validator failed; hint: inspect the validator_results entries with status != pass before delivering the artifact".into()),
        }
    } else {
        inspect_artifact(
            &candidate.working_dir,
            path.as_deref(),
            native_arc,
            response_schema,
        )
    };
    let checks = validators
        .iter()
        .cloned()
        .map(CheckOutcome::Validator)
        .chain(artifact.checks)
        .collect();
    let receipt = CompletionReceipt {
        task_id: candidate.task_id.clone(),
        candidate_revision: candidate.revision,
        gate_policy_version: octos_agent::WORKSPACE_POLICY_SCHEMA_VERSION,
        checks,
        artifact_state: artifact.state,
        artifact_path: path.clone(),
        artifact_content: artifact.content.clone(),
        validator_references: BTreeMap::new(),
    };
    let decision = classify(candidate, receipt, 1, 2);
    let ready = artifact.error.is_none();
    let outcome = McpSessionOutcome {
        final_state: if ready {
            TaskLifecycleState::Ready
        } else {
            TaskLifecycleState::Failed
        },
        artifact_path: if ready {
            path.map(|p| p.display().to_string())
        } else {
            None
        },
        artifact_content: if ready { artifact.content } else { None },
        validator_results: validators,
        cost: McpSessionCost::from(&candidate.cumulative_usage),
        error: artifact.error,
    };
    CompletionGateResult { decision, outcome }
}

fn inspect_artifact(
    workspace: &Path,
    path: Option<&Path>,
    native_arc: bool,
    response_schema: Option<&Value>,
) -> GateArtifact {
    let Some(path) = path else {
        let error = "contract_failed: agent finished without declaring an artifact_path; hint: pass `expected_artifact` in the MCP input or declare the contract artifact in the workspace policy".to_string();
        return rejected_artifact(
            None,
            ArtifactState::Missing,
            ArtifactCheckKind::Exists,
            ArtifactReasonCode::Other,
            ValidatorStatus::Error,
            false,
            error,
        );
    };
    if !path.exists() {
        let error = format!(
            "artifact_missing: expected artifact at '{}' but the file is not present; hint: ensure the agent writes the contract artifact before returning",
            path.display()
        );
        return rejected_artifact(
            Some(path),
            ArtifactState::Missing,
            ArtifactCheckKind::Exists,
            ArtifactReasonCode::Missing,
            ValidatorStatus::Fail,
            true,
            error,
        );
    }
    let mut checks = vec![artifact_check(
        path,
        ArtifactCheckKind::Exists,
        ValidatorStatus::Pass,
        ArtifactReasonCode::Other,
        "",
        true,
    )];
    if native_arc {
        if let Err(error) = validate_arc_artifact_location(workspace, path) {
            let error = error.to_string();
            checks.push(artifact_check(
                path,
                ArtifactCheckKind::Location,
                ValidatorStatus::Fail,
                ArtifactReasonCode::UnsafeLocation,
                &error,
                false,
            ));
            return GateArtifact {
                state: ArtifactState::Rejected,
                content: None,
                checks,
                error: Some(error),
            };
        }
        checks.push(artifact_check(
            path,
            ArtifactCheckKind::Location,
            ValidatorStatus::Pass,
            ArtifactReasonCode::Other,
            "",
            true,
        ));
    }
    let content = read_small_text_artifact(path);
    if let Err(reason) = &content {
        if response_schema.is_some() {
            let error =
                "artifact_schema_invalid: expected a UTF-8 JSON artifact no larger than 64 KiB"
                    .to_string();
            checks.push(artifact_check(
                path,
                ArtifactCheckKind::Text,
                ValidatorStatus::Fail,
                *reason,
                &error,
                true,
            ));
            return GateArtifact {
                state: ArtifactState::Rejected,
                content: None,
                checks,
                error: Some(error),
            };
        }
    }
    let content = content.ok();
    if content.is_some() {
        checks.push(artifact_check(
            path,
            ArtifactCheckKind::Text,
            ValidatorStatus::Pass,
            ArtifactReasonCode::Other,
            "",
            true,
        ));
    }
    if let Some(schema) = response_schema {
        let parsed = match serde_json::from_str::<Value>(
            content.as_deref().expect("schema requires text"),
        ) {
            Ok(value) => value,
            Err(error) => {
                let error = format!("artifact_schema_invalid: artifact is not valid JSON: {error}");
                checks.push(artifact_check(
                    path,
                    ArtifactCheckKind::Json,
                    ValidatorStatus::Fail,
                    ArtifactReasonCode::InvalidJson,
                    &error,
                    true,
                ));
                return GateArtifact {
                    state: ArtifactState::Rejected,
                    content: None,
                    checks,
                    error: Some(error),
                };
            }
        };
        checks.push(artifact_check(
            path,
            ArtifactCheckKind::Json,
            ValidatorStatus::Pass,
            ArtifactReasonCode::Other,
            "",
            true,
        ));
        if let Err(error) = validate_arc_response(schema, &parsed) {
            let error = error.to_string();
            checks.push(artifact_check(
                path,
                ArtifactCheckKind::Schema,
                ValidatorStatus::Fail,
                ArtifactReasonCode::SchemaMismatch,
                &error,
                true,
            ));
            return GateArtifact {
                state: ArtifactState::Rejected,
                content: None,
                checks,
                error: Some(error),
            };
        }
        checks.push(artifact_check(
            path,
            ArtifactCheckKind::Schema,
            ValidatorStatus::Pass,
            ArtifactReasonCode::Other,
            "",
            true,
        ));
    }
    GateArtifact {
        state: if content.is_some() {
            ArtifactState::Ready
        } else {
            ArtifactState::ReadyWithoutInlineContent
        },
        content,
        checks,
        error: None,
    }
}

fn rejected_artifact(
    path: Option<&Path>,
    state: ArtifactState,
    kind: ArtifactCheckKind,
    code: ArtifactReasonCode,
    status: ValidatorStatus,
    safe_target: bool,
    error: String,
) -> GateArtifact {
    GateArtifact {
        state,
        content: None,
        checks: vec![artifact_check(
            path.unwrap_or(Path::new("")),
            kind,
            status,
            code,
            &error,
            safe_target,
        )],
        error: Some(error),
    }
}

fn artifact_check(
    path: &Path,
    kind: ArtifactCheckKind,
    status: ValidatorStatus,
    reason_code: ArtifactReasonCode,
    reason: &str,
    safe_target: bool,
) -> CheckOutcome {
    CheckOutcome::Artifact(ArtifactCheckOutcome {
        gate_id: format!("artifact/{kind:?}").to_lowercase(),
        kind,
        status,
        reason_code,
        reason: reason.to_string(),
        stderr: None,
        expected_artifact: (!path.as_os_str().is_empty()).then(|| path.to_path_buf()),
        observed_artifact: (!path.as_os_str().is_empty()
            && (status == ValidatorStatus::Pass || kind != ArtifactCheckKind::Exists))
            .then(|| path.to_path_buf()),
        schema_pointer: None,
        safe_target,
        evidence_ref: None,
    })
}

/// Run completion-phase workspace validators and return their typed outcomes.
/// No policy or no completion validators returns an empty vector. Failures
/// during individual validators show up as `status != pass` entries in the
/// returned array — the dispatch blocks terminal success on required misses.
async fn run_completion_validators(
    workspace_root: &std::path::Path,
    contract: &str,
    tools: &Arc<ToolRegistry>,
    sandbox: &SandboxConfig,
) -> Vec<ValidatorOutcome> {
    let Ok(Some(policy)) = octos_agent::read_workspace_policy(workspace_root) else {
        return Vec::new();
    };
    if policy.validation.validators.is_empty() {
        return Vec::new();
    }
    // Run workspace command validators *through* the same sandbox the agent's
    // tools use — otherwise a workspace-declared completion command would be an
    // unsandboxed host-exec escape on the mcp-serve path (the validator runner
    // routes no-op sandboxes to a direct, injection-safe argv exec).
    let runner = ValidatorRunner::new(tools.clone(), workspace_root)
        .with_sandbox(Arc::from(create_sandbox(sandbox)));
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: workspace_root.to_path_buf(),
        repo_label: format!("mcp-serve/{contract}"),
        input_args: None,
        tool_output: None,
        spawn_only_files: Vec::new(),
    };
    run_workspace_validators(
        &runner,
        &invocation,
        &policy.validation.validators,
        Some(ValidatorPhase::Completion),
    )
    .await
}

fn resolve_artifact_path(
    cwd: &std::path::Path,
    expected: Option<&std::path::Path>,
    artifact_name: &str,
    files_to_send: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(path) = expected {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        return Some(absolute);
    }
    if let Ok(Some(policy)) = octos_agent::read_workspace_policy(cwd) {
        if let Some(pattern) = policy.artifacts.entries.get(artifact_name) {
            let candidate = if std::path::Path::new(pattern).is_absolute() {
                PathBuf::from(pattern)
            } else {
                cwd.join(pattern)
            };
            // Only return a direct file match — globs are deliberately left
            // to the workspace-contract enforcement path. This keeps the
            // MCP dispatch cheap and dependency-free.
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    files_to_send.iter().find(|p| p.is_file()).cloned()
}

fn read_small_text_artifact(path: &std::path::Path) -> Result<String, ArtifactReasonCode> {
    const MAX_INLINE_BYTES: u64 = 64 * 1024;
    let meta = std::fs::metadata(path).map_err(|_| ArtifactReasonCode::Other)?;
    if meta.len() > MAX_INLINE_BYTES {
        return Err(ArtifactReasonCode::TooLarge);
    }
    std::fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidData {
            ArtifactReasonCode::InvalidUtf8
        } else {
            ArtifactReasonCode::Other
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h06_m5_workspace_progress_uses_file_bytes() {
        let workspace = tempfile::tempdir().unwrap();
        let mut candidate = gate_candidate(workspace.path());
        candidate.files_modified.push(PathBuf::from("result.json"));
        std::fs::write(workspace.path().join("result.json"), "first").unwrap();
        let first = workspace_fingerprint(&candidate).unwrap();
        std::fs::write(workspace.path().join("result.json"), "first").unwrap();
        assert_eq!(workspace_fingerprint(&candidate), Some(first));
        std::fs::write(workspace.path().join("result.json"), "second").unwrap();
        assert_ne!(workspace_fingerprint(&candidate), Some(first));
        candidate.files_modified.push(PathBuf::from("../outside"));
        assert!(workspace_fingerprint(&candidate).is_none());
    }

    #[test]
    fn h06_m6_observation_uses_bounded_facts_without_changing_decision() {
        let workspace = tempfile::tempdir().unwrap();
        let candidate = gate_candidate(workspace.path());
        let secret = "ARCBENCH_API_KEY=secret schema={very_large} stderr=private";
        let receipt = CompletionReceipt {
            task_id: candidate.task_id.clone(),
            candidate_revision: candidate.revision,
            gate_policy_version: 1,
            checks: vec![artifact_check(
                Path::new("result.json"),
                ArtifactCheckKind::Exists,
                ValidatorStatus::Fail,
                ArtifactReasonCode::Missing,
                secret,
                true,
            )],
            artifact_state: ArtifactState::Missing,
            artifact_path: None,
            artifact_content: None,
            validator_references: BTreeMap::new(),
        };
        let decision = CompletionDecision::TerminalFailure {
            reason: TerminalReason::GateError,
            receipt,
        };

        let (hard_passes, hard_failures, categories, signature) =
            observation_facts(decision.receipt());
        assert_eq!(hard_passes, 0);
        assert_eq!(hard_failures, 1);
        assert_eq!(categories, "artifact_missing");
        assert_eq!(signature.len(), 64);
        assert!(!categories.contains(secret));
        assert!(!signature.contains(secret));

        emit_gate_observation(&candidate, &decision, 1, None, false, 7);
        assert!(matches!(
            decision,
            CompletionDecision::TerminalFailure {
                reason: TerminalReason::GateError,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn h06_m5_disposable_workspace_lock_is_exclusive() {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let first = ProtectedWorkspace::new(workspace.path(), data.path()).unwrap();
        let error = ProtectedWorkspace::new(workspace.path(), data.path())
            .err()
            .expect("second owner must be rejected");
        assert!(error.to_string().contains("already in use"));
        drop(first);
        assert!(ProtectedWorkspace::new(workspace.path(), data.path()).is_ok());
    }

    #[test]
    fn h06_m4_repair_classes_are_explicitly_bounded() {
        let workspace = tempfile::tempdir().unwrap();
        let candidate = gate_candidate(workspace.path());
        let allowed = [
            (ArtifactReasonCode::Missing, ArtifactCheckKind::Exists, true),
            (
                ArtifactReasonCode::InvalidJson,
                ArtifactCheckKind::Json,
                true,
            ),
            (
                ArtifactReasonCode::SchemaMismatch,
                ArtifactCheckKind::Schema,
                true,
            ),
            (
                ArtifactReasonCode::UnsafeLocation,
                ArtifactCheckKind::Location,
                true,
            ),
        ];
        let terminal = [
            (
                ArtifactReasonCode::UnsafeLocation,
                ArtifactCheckKind::Location,
                false,
            ),
            (ArtifactReasonCode::TooLarge, ArtifactCheckKind::Text, true),
            (
                ArtifactReasonCode::InvalidUtf8,
                ArtifactCheckKind::Text,
                true,
            ),
            (ArtifactReasonCode::Other, ArtifactCheckKind::Exists, true),
        ];
        for (code, kind, safe_target) in allowed {
            let mut check = artifact_check(
                Path::new("result.json"),
                kind,
                ValidatorStatus::Fail,
                code,
                "failure",
                safe_target,
            );
            if let CheckOutcome::Artifact(ref mut artifact) = check {
                artifact.safe_target = safe_target;
            }
            let receipt = CompletionReceipt {
                task_id: candidate.task_id.clone(),
                candidate_revision: candidate.revision,
                gate_policy_version: 1,
                checks: vec![check],
                artifact_state: ArtifactState::Rejected,
                artifact_path: None,
                artifact_content: None,
                validator_references: BTreeMap::new(),
            };
            assert!(repair_allowed(&receipt), "{code:?}");
        }
        for (code, kind, safe_target) in terminal {
            let check = artifact_check(
                Path::new("result.json"),
                kind,
                ValidatorStatus::Fail,
                code,
                "failure",
                safe_target,
            );
            let receipt = CompletionReceipt {
                task_id: candidate.task_id.clone(),
                candidate_revision: candidate.revision,
                gate_policy_version: 1,
                checks: vec![check],
                artifact_state: ArtifactState::Rejected,
                artifact_path: None,
                artifact_content: None,
                validator_references: BTreeMap::new(),
            };
            assert!(!repair_allowed(&receipt), "{code:?}");
        }
        for status in [ValidatorStatus::Timeout, ValidatorStatus::Error] {
            let check = artifact_check(
                Path::new("result.json"),
                ArtifactCheckKind::Exists,
                status,
                ArtifactReasonCode::Other,
                "failure",
                true,
            );
            let receipt = CompletionReceipt {
                task_id: candidate.task_id.clone(),
                candidate_revision: candidate.revision,
                gate_policy_version: 1,
                checks: vec![check],
                artifact_state: ArtifactState::Rejected,
                artifact_path: None,
                artifact_content: None,
                validator_references: BTreeMap::new(),
            };
            assert!(!repair_allowed(&receipt), "{status:?}");
        }
    }

    fn gate_candidate(workspace: &Path) -> CompletionCandidate {
        CompletionCandidate {
            task_id: octos_core::TaskId::new(),
            working_dir: workspace.to_path_buf(),
            proposed_output: "done".into(),
            files_modified: Vec::new(),
            files_to_send: Vec::new(),
            iteration: 0,
            cumulative_usage: octos_core::TokenUsage::default(),
            revision: 1,
        }
    }

    fn gate_receipt(decision: &CompletionDecision) -> &CompletionReceipt {
        match decision {
            CompletionDecision::Pass(receipt)
            | CompletionDecision::Repairable { receipt, .. }
            | CompletionDecision::TerminalFailure { receipt, .. } => receipt,
        }
    }

    fn validator(id: &str, required: bool) -> ValidatorOutcome {
        ValidatorOutcome {
            schema_version: 1,
            validator_id: id.into(),
            phase: ValidatorPhase::Completion,
            kind: "file_exists".into(),
            repo_label: "mcp-serve/test".into(),
            required,
            required_tier: if required { "hard" } else { "soft" }.into(),
            status: ValidatorStatus::Fail,
            reason: "missing file".into(),
            duration_ms: 0,
            evidence_path: None,
            stderr: None,
            started_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn gate_preserves_all_validator_outcomes_and_skips_artifact_after_required_failure() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("existing.json");
        std::fs::write(&path, "{}").unwrap();
        let candidate = gate_candidate(workspace.path());
        let gate = complete_candidate_verification(
            &candidate,
            vec![validator("required", true), validator("optional", false)],
            Some(path.clone()),
            true,
            Some(&serde_json::json!({"type": "object"})),
        );
        assert_eq!(gate.outcome.final_state, TaskLifecycleState::Failed);
        assert_eq!(gate.outcome.validator_results.len(), 2);
        assert!(
            gate.outcome
                .error
                .as_deref()
                .unwrap()
                .starts_with("contract_failed:")
        );
        let receipt = gate_receipt(&gate.decision);
        assert_eq!(receipt.task_id, candidate.task_id);
        assert_eq!(receipt.candidate_revision, candidate.revision);
        assert_eq!(receipt.artifact_state, ArtifactState::Unchecked);
        assert_eq!(receipt.artifact_path.as_deref(), Some(path.as_path()));
        assert_eq!(receipt.checks.len(), 2);
    }

    #[test]
    fn optional_validator_failure_still_checks_artifact_and_passes() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("result.json");
        std::fs::write(&path, "{}").unwrap();
        let candidate = gate_candidate(workspace.path());
        let gate = complete_candidate_verification(
            &candidate,
            vec![validator("optional", false)],
            Some(path.clone()),
            true,
            Some(&serde_json::json!({"type": "object"})),
        );
        assert!(matches!(gate.decision, CompletionDecision::Pass(_)));
        assert_eq!(gate.outcome.final_state, TaskLifecycleState::Ready);
        assert_eq!(
            gate.outcome.validator_results[0].status,
            ValidatorStatus::Fail
        );
        let receipt = gate_receipt(&gate.decision);
        assert_eq!(receipt.artifact_state, ArtifactState::Ready);
        assert_eq!(receipt.checks.len(), 6);
    }

    #[test]
    fn gate_artifact_failures_keep_external_errors_and_typed_reasons() {
        let workspace = tempfile::tempdir().unwrap();
        let candidate = gate_candidate(workspace.path());
        let schema = serde_json::json!({"type": "object", "required": ["count"]});
        type ArtifactFailureCase<'a> = (
            &'a str,
            Option<&'a [u8]>,
            ArtifactCheckKind,
            ArtifactReasonCode,
            &'a str,
        );
        let cases: &[ArtifactFailureCase<'_>] = &[
            (
                "missing.json",
                None,
                ArtifactCheckKind::Exists,
                ArtifactReasonCode::Missing,
                "artifact_missing:",
            ),
            (
                "large.json",
                Some(&[b'X'; 65_537]),
                ArtifactCheckKind::Text,
                ArtifactReasonCode::TooLarge,
                "artifact_schema_invalid:",
            ),
            (
                "utf8.json",
                Some(&[0xff]),
                ArtifactCheckKind::Text,
                ArtifactReasonCode::InvalidUtf8,
                "artifact_schema_invalid:",
            ),
            (
                "json.json",
                Some(b"{broken"),
                ArtifactCheckKind::Json,
                ArtifactReasonCode::InvalidJson,
                "artifact_schema_invalid:",
            ),
            (
                "schema.json",
                Some(b"{}"),
                ArtifactCheckKind::Schema,
                ArtifactReasonCode::SchemaMismatch,
                "artifact_schema_invalid:",
            ),
        ];
        for (name, bytes, kind, reason, prefix) in cases {
            let path = workspace.path().join(name);
            if let Some(bytes) = bytes {
                std::fs::write(&path, bytes).unwrap();
            }
            let gate = complete_candidate_verification(
                &candidate,
                Vec::new(),
                Some(path),
                true,
                Some(&schema),
            );
            assert_eq!(
                gate.outcome.final_state,
                TaskLifecycleState::Failed,
                "{name}"
            );
            assert!(
                gate.outcome.error.as_deref().unwrap().starts_with(prefix),
                "{name}"
            );
            let receipt = gate_receipt(&gate.decision);
            assert!(matches!(
                receipt.artifact_state,
                ArtifactState::Missing | ArtifactState::Rejected
            ));
            assert!(receipt.checks.iter().any(|check| matches!(check, CheckOutcome::Artifact(a) if a.kind == *kind && a.reason_code == *reason && a.status == ValidatorStatus::Fail)), "{name}");
        }
        let gate = complete_candidate_verification(&candidate, Vec::new(), None, false, None);
        assert!(
            gate.outcome
                .error
                .as_deref()
                .unwrap()
                .starts_with("contract_failed:")
        );
        assert_eq!(
            gate_receipt(&gate.decision).artifact_state,
            ArtifactState::Missing
        );
        assert!(matches!(
            gate.decision,
            CompletionDecision::TerminalFailure { .. }
        ));
    }

    #[test]
    fn legacy_artifact_without_schema_keeps_non_json_and_binary_ready() {
        let workspace = tempfile::tempdir().unwrap();
        let candidate = gate_candidate(workspace.path());
        let text = workspace.path().join("plain.txt");
        std::fs::write(&text, "not JSON").unwrap();
        let gate = complete_candidate_verification(&candidate, Vec::new(), Some(text), false, None);
        assert!(matches!(gate.decision, CompletionDecision::Pass(_)));
        assert_eq!(gate.outcome.artifact_content.as_deref(), Some("not JSON"));
        let binary = workspace.path().join("binary.bin");
        std::fs::write(&binary, [0xff]).unwrap();
        let gate =
            complete_candidate_verification(&candidate, Vec::new(), Some(binary), false, None);
        assert!(matches!(gate.decision, CompletionDecision::Pass(_)));
        assert_eq!(gate.outcome.artifact_content, None);
        assert_eq!(
            gate_receipt(&gate.decision).artifact_state,
            ArtifactState::ReadyWithoutInlineContent
        );
        let native_text = workspace.path().join("native.txt");
        std::fs::write(&native_text, "still not JSON").unwrap();
        let gate =
            complete_candidate_verification(&candidate, Vec::new(), Some(native_text), true, None);
        assert!(matches!(gate.decision, CompletionDecision::Pass(_)));
        assert_eq!(
            gate.outcome.artifact_content.as_deref(),
            Some("still not JSON")
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_artifact_symlink_outside_workspace_is_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("result.json");
        std::fs::write(&target, "{}").unwrap();
        let link = workspace.path().join("result.json");
        std::os::unix::fs::symlink(target, &link).unwrap();
        let candidate = gate_candidate(workspace.path());
        let gate = complete_candidate_verification(&candidate, Vec::new(), Some(link), true, None);
        assert!(
            gate.outcome
                .error
                .as_deref()
                .unwrap()
                .starts_with("artifact_schema_invalid:")
        );
        assert!(matches!(
            gate.decision,
            CompletionDecision::TerminalFailure { .. }
        ));
        assert!(gate_receipt(&gate.decision).checks.iter().any(|check| matches!(check, CheckOutcome::Artifact(a) if a.kind == ArtifactCheckKind::Location && a.reason_code == ArtifactReasonCode::UnsafeLocation && !a.safe_target)));
    }

    #[test]
    fn http_transport_parses() {
        let cmd: McpServeCommand = McpServeCommand {
            transport: McpTransport::Http,
            bind: "127.0.0.1:4033".parse().unwrap(),
            cwd: None,
            data_dir: None,
            config: None,
            h06_completion_repair: false,
        };
        assert!(matches!(cmd.transport, McpTransport::Http));
    }

    #[test]
    fn sandbox_backend_honors_config_and_defaults_to_auto() {
        let mk = |sandbox: SandboxConfig| SessionDispatchConfig {
            cwd: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            max_iterations: 4,
            sandbox,
            tool_policy: None,
            tool_policy_by_provider: HashMap::new(),
            provider_name: String::new(),
            output_recovery: octos_agent::output_recovery::OutputPolicy::default(),
            completion_repair: CompletionRepairPolicy::new(0),
        };

        // A disabled policy must produce a pass-through backend, proving the
        // dispatch reads the configured policy rather than hardcoding one.
        let disabled = mk(SandboxConfig {
            enabled: false,
            ..Default::default()
        });
        let program = disabled
            .sandbox_backend()
            .1
            .wrap_command("true", std::path::Path::new("."))
            .as_std()
            .get_program()
            .to_string_lossy()
            .into_owned();
        assert!(
            program == "sh" || program == "cmd",
            "disabled sandbox must pass through, got {program:?}"
        );

        // The default policy is enabled + Auto — never NoSandbox by omission,
        // which was the M7.2 dispatch RCE (`with_builtins` hardcoded NoSandbox).
        let default = mk(SandboxConfig::default());
        assert!(default.sandbox.enabled);
        assert_eq!(default.sandbox.mode, octos_agent::SandboxMode::Auto);
    }

    #[test]
    fn resolve_artifact_prefers_expected_path_when_supplied() {
        let dir = tempfile::tempdir().unwrap();
        let expected = dir.path().join("out.bin");
        std::fs::write(&expected, b"x").unwrap();
        let resolved = resolve_artifact_path(dir.path(), Some(&expected), "primary", &[]).unwrap();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn resolve_artifact_falls_back_to_files_to_send_when_no_expected() {
        let dir = tempfile::tempdir().unwrap();
        let delivered = dir.path().join("delivered.txt");
        std::fs::write(&delivered, b"hello").unwrap();
        let resolved = resolve_artifact_path(
            dir.path(),
            None,
            "primary",
            std::slice::from_ref(&delivered),
        );
        assert_eq!(resolved.as_ref(), Some(&delivered));
    }

    #[test]
    fn mcp_session_cost_includes_all_counters() {
        let usage = octos_core::TokenUsage {
            input_tokens: 12,
            output_tokens: 7,
            reasoning_tokens: 3,
            cache_read_tokens: 2,
            cache_write_tokens: 1,
        };
        let cost = McpSessionCost::from(&usage);
        assert_eq!(cost.input_tokens, 12);
        assert_eq!(cost.output_tokens, 7);
        assert_eq!(cost.reasoning_tokens, 3);
        assert_eq!(cost.cache_read_tokens, 2);
        assert_eq!(cost.cache_write_tokens, 1);
    }

    #[test]
    fn read_small_text_artifact_returns_none_for_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        // Write 128 KiB — above the 64 KiB inline ceiling.
        let payload = vec![b'A'; 128 * 1024];
        std::fs::write(&path, payload).unwrap();
        assert!(matches!(
            read_small_text_artifact(&path),
            Err(ArtifactReasonCode::TooLarge)
        ));
    }

    #[test]
    fn read_small_text_artifact_inlines_small_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, b"hello").unwrap();
        assert_eq!(read_small_text_artifact(&path).as_deref(), Ok("hello"));
    }
}

#[cfg(all(test, feature = "api"))]
mod http_transport_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use octos_agent::mcp_server::{
        McpServer, McpServerError, McpSessionCost, McpSessionDispatch, McpSessionOutcome,
        SessionLifecycleObserver,
    };
    use octos_agent::task_supervisor::{TaskLifecycleState, TaskSupervisor};
    use serde_json::Value;

    use super::mcp_http_router;

    struct ReadyDispatch;

    #[async_trait]
    impl McpSessionDispatch for ReadyDispatch {
        async fn run_session(
            &self,
            _contract: &str,
            _input: &Value,
            observer: &dyn SessionLifecycleObserver,
        ) -> Result<McpSessionOutcome, McpServerError> {
            observer.mark_state(TaskLifecycleState::Ready);
            Ok(McpSessionOutcome {
                final_state: TaskLifecycleState::Ready,
                artifact_path: None,
                artifact_content: None,
                validator_results: vec![],
                cost: McpSessionCost::default(),
                error: None,
            })
        }
    }

    /// Bind the Streamable HTTP router on an ephemeral loopback port and return
    /// its address. The server task is detached; it stops when the test's
    /// runtime is dropped.
    async fn spawn_http_server(token: &str) -> std::net::SocketAddr {
        let server = McpServer::new(Arc::new(ReadyDispatch), Arc::new(TaskSupervisor::new()));
        let (app, _cancel) = mcp_http_router(server, token.to_string(), false);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        addr
    }

    /// The bearer gate must reject missing/wrong tokens with 401 before the
    /// request reaches the rmcp service, and let a correct token through.
    #[tokio::test]
    async fn http_transport_gates_on_bearer_token() {
        let addr = spawn_http_server("super-secret").await;
        let url = format!("http://{addr}/mcp");
        let client = reqwest::Client::new();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

        // No Authorization header -> 401.
        let resp = client.post(&url).body(body).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Wrong token -> 401.
        let resp = client
            .post(&url)
            .header("Authorization", "Bearer nope")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Correct token passes the gate and reaches rmcp. rmcp may reject the
        // bare body with a 4xx for missing MCP headers/session, but never 401.
        let resp = client
            .post(&url)
            .header("Authorization", "Bearer super-secret")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "a correct bearer token must pass the gate"
        );
    }
}
