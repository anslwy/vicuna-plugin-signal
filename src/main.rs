//! Vicuna Signal bridge plugin (powered by `signal-cli jsonRpc`).
//!
//! Speaks Vicuna's newline-delimited JSON-RPC bridge protocol over stdin/stdout,
//! and manages a single `signal-cli --config <dir> jsonRpc` child process to
//! link a Signal account via QR code and route chats (Note to Self, solo groups,
//! or DMs) to individual Vicuna Work sessions.

mod runtime;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use qrcode::render::svg;
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};

const MAX_MSG_CHARS: usize = 3800;
const MAX_PROGRESS_EDITS: u8 = 8; // Signal caps edits at 10 per message; leave headroom for final edit
const MAX_SENT_TRACKED: usize = 512;

// ---------- Persistent config ----------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum SignalTarget {
    NoteToSelf,
    Group { group_id: String },
    Recipient { address: String },
}

impl SignalTarget {
    fn id_string(&self) -> String {
        match self {
            SignalTarget::NoteToSelf => "note-to-self".into(),
            SignalTarget::Group { group_id } => format!("group:{group_id}"),
            SignalTarget::Recipient { address } => format!("dm:{address}"),
        }
    }

    fn display_name(&self, hint: Option<&str>) -> String {
        if let Some(h) = hint.map(str::trim).filter(|s| !s.is_empty()) {
            return h.to_string();
        }
        match self {
            SignalTarget::NoteToSelf => "Note to Self".into(),
            SignalTarget::Group { group_id } => {
                let short: String = group_id.chars().take(10).collect();
                format!("Signal Group ({short}…)")
            }
            SignalTarget::Recipient { address } => address.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairedTarget {
    target: SignalTarget,
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionBridge {
    enabled: bool,
    pair_code: String,
    #[serde(default)]
    paired: Option<PairedTarget>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Store {
    #[serde(default)]
    account: Option<String>,
    /// True when `account` was registered by the plugin as its own number
    /// (a dedicated "Vicuna" contact) rather than linked to the user's account.
    #[serde(default)]
    registered: bool,
    #[serde(default)]
    sessions: HashMap<String, SessionBridge>,
}

fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("VICUNA_BRIDGE_CONFIG_PATH") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join("vicuna")
        .join("config")
        .join("plugins")
        .join("signal.json")
}

fn signal_cli_data_dir(cfg_path: &Path) -> PathBuf {
    if let Ok(d) = std::env::var("VICUNA_SIGNAL_DATA_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    cfg_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("signal-cli-data")
}

fn load_store(path: &Path) -> Store {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_store(path: &Path, store: &Store) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(store).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(())
}

// ---------- Register mode (dedicated bot number) ----------

fn input_prompt(reg: &RegState) -> Value {
    match reg {
        RegState::Idle => json!({
            "label": "Phone number for Vicuna's own Signal account, with country code. (To use your own account with a QR code instead, type link.)",
            "placeholder": "+15551234567",
            "submitLabel": "Continue",
        }),
        RegState::NeedCaptcha { number } => json!({
            "label": format!("Signal needs a quick CAPTCHA before it texts a code to {number}."),
            "placeholder": "signalcaptcha://…",
            "submitLabel": "Continue",
            "capture": { "url": CAPTCHA_URL, "scheme": "signalcaptcha", "label": "Solve CAPTCHA" },
        }),
        RegState::AwaitingCode { number, .. } => json!({
            "label": format!("Enter the 6-digit code Signal sent by SMS to {number}. Add the registration-lock PIN after it if the number has one, or type voice to get a phone call instead."),
            "placeholder": "123456",
            "submitLabel": "Verify",
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RegState {
    Idle,
    NeedCaptcha { number: String },
    AwaitingCode { number: String, captcha: String },
}

#[derive(Debug, PartialEq, Eq)]
enum RegInput {
    Link,
    Register { number: String, captcha: Option<String> },
}

#[derive(Debug, PartialEq, Eq)]
enum CodeInput {
    Voice,
    Code { code: String, pin: Option<String> },
}

const CAPTCHA_URL: &str = "https://signalcaptchas.org/registration/generate.html";

/// `+15551234567 signalcaptcha://…` (either order, number may contain spaces/dashes) or `link`.
fn parse_register_input(s: &str) -> Result<RegInput, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("link") || s.eq_ignore_ascii_case("qr") {
        return Ok(RegInput::Link);
    }
    let mut captcha = None;
    let mut rest = String::new();
    for tok in s.split_whitespace() {
        if tok.starts_with("signalcaptcha://") {
            captcha = Some(tok.to_string());
        } else {
            rest.push_str(tok);
        }
    }
    let number: String = rest.chars().filter(|c| !matches!(c, '-' | '(' | ')' | '.')).collect();
    let digits = number.strip_prefix('+').unwrap_or("");
    if digits.len() < 8 || digits.len() > 15 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err("Enter the number with its country code, like +15551234567.".into());
    }
    Ok(RegInput::Register { number, captcha })
}

/// `123456`, `123-456`, optionally followed by the registration-lock PIN, or `voice`.
fn parse_code_input(s: &str) -> Result<CodeInput, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("voice") || s.eq_ignore_ascii_case("call") {
        return Ok(CodeInput::Voice);
    }
    let mut toks = s.split_whitespace();
    let code: String = toks.next().unwrap_or("").chars().filter(|c| *c != '-').collect();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err("Enter the 6-digit code from the SMS (or type voice to get a call).".into());
    }
    let pin = toks.next().map(str::to_string);
    Ok(CodeInput::Code { code, pin })
}

fn random_pair_code() -> String {
    let mut buf = [0u8; 4];
    let _ = getrandom::fill(&mut buf);
    let n = u32::from_be_bytes(buf) % 900_000 + 100_000;
    format!("{n:06}")
}

// ---------- QR code helper ----------

fn qr_svg_data_url(uri: &str) -> Result<String, String> {
    let code = QrCode::new(uri.as_bytes()).map_err(|e| format!("qr encode failed: {e}"))?;
    let svg_xml = code
        .render::<svg::Color>()
        .min_dimensions(200, 200)
        .quiet_zone(true)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();
    let b64 = base64::engine::general_purpose::STANDARD.encode(svg_xml.as_bytes());
    Ok(format!("data:image/svg+xml;base64,{b64}"))
}

// ---------- Host JSON-RPC client (plugin -> Vicuna) ----------

struct HostRpc {
    tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    next_id: AtomicU64,
}

impl HostRpc {
    fn new(tx: mpsc::UnboundedSender<String>) -> Self {
        Self {
            tx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let line = json!({ "id": id, "method": method, "params": params }).to_string();
        if self.tx.send(line).is_err() {
            self.pending.lock().await.remove(&id);
            return Err("host stdout closed".into());
        }
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err("host disconnected".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("host rpc timed out on {method}"))
            }
        }
    }

    async fn resolve(&self, id: u64, res: Result<Value, String>) {
        if let Some(tx) = self.pending.lock().await.remove(&id) {
            let _ = tx.send(res);
        }
    }
}

// ---------- signal-cli JSON-RPC client ----------

fn find_signal_cli() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("SIGNAL_CLI_BIN") {
        let p = PathBuf::from(explicit.trim());
        if p.is_file() {
            return Some(p);
        }
    }
    let candidates: &[&str] = if cfg!(windows) {
        &["signal-cli.bat", "signal-cli.exe", "signal-cli"]
    } else {
        &[
            "signal-cli",
            "/opt/homebrew/bin/signal-cli",
            "/usr/local/bin/signal-cli",
            "/usr/bin/signal-cli",
        ]
    };
    for c in candidates {
        let p = Path::new(c);
        if p.is_absolute() {
            if p.is_file() {
                return Some(p.to_path_buf());
            }
        } else if let Some(found) = which_in_path(c) {
            return Some(found);
        }
    }
    None
}

fn which_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let full = dir.join(name);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

struct SignalProc {
    tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    next_id: AtomicU64,
    task: tokio::task::JoinHandle<()>,
}

impl SignalProc {
    async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });
        if !params.is_null() {
            req["params"] = params;
        }
        if self.tx.send(req.to_string()).is_err() {
            self.pending.lock().await.remove(&id);
            return Err("signal-cli daemon is not running".into());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err("signal-cli daemon closed connection".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("signal-cli timed out on {method}"))
            }
        }
    }

    fn is_alive(&self) -> bool {
        !self.task.is_finished()
    }
}

impl Drop for SignalProc {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ---------- Runtime state ----------

#[derive(Debug, Clone)]
struct ActiveTurn {
    turn_id: String,
    target: SignalTarget,
    msg_timestamp: Option<i64>,
    edit_count: u8,
    pending_ask: Option<PendingAsk>,
}

#[derive(Debug, Clone)]
struct PendingAsk {
    options: Vec<String>,
    freeform: bool,
}

#[derive(Default)]
struct SentTracker {
    order: VecDeque<i64>,
    set: HashSet<i64>,
}

impl SentTracker {
    fn record(&mut self, ts: i64) {
        if self.set.insert(ts) {
            self.order.push_back(ts);
            while self.order.len() > MAX_SENT_TRACKED {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }

    fn contains(&self, ts: i64) -> bool {
        self.set.contains(&ts)
    }
}

struct AppState {
    cfg_path: PathBuf,
    data_dir: PathBuf,
    store: Mutex<Store>,
    host: Arc<HostRpc>,
    proc: Mutex<Option<Arc<SignalProc>>>,
    qr_data_url: Mutex<Option<String>>,
    reg: Mutex<RegState>,
    rt_status: Arc<std::sync::Mutex<runtime::Status>>,
    rt_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    last_error: Mutex<String>,
    linking_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    turns: Mutex<HashMap<String, ActiveTurn>>,
    sent: Mutex<SentTracker>,
    incoming_tx: mpsc::UnboundedSender<Value>,
}

impl AppState {
    /// SIGNAL_CLI_BIN, then the managed runtime, then a system signal-cli.
    fn resolve_runtime(&self) -> Option<runtime::Runtime> {
        let explicit = std::env::var("SIGNAL_CLI_BIN").map(|v| !v.trim().is_empty()).unwrap_or(false);
        if !explicit {
            if let Some(rt) = runtime::installed(&runtime::root(&self.data_dir)) {
                return Some(rt);
            }
        }
        find_signal_cli().map(|bin| runtime::Runtime { bin, java_home: None })
    }

    fn runtime_note(&self) -> Option<String> {
        self.rt_status.lock().ok().and_then(|s| s.describe())
    }

    /// Start the one-time signal-cli download unless it's running or done.
    async fn start_runtime_download(self: &Arc<Self>) {
        let mut task = self.rt_task.lock().await;
        if task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        if let Ok(mut s) = self.rt_status.lock() {
            *s = runtime::Status::Downloading { label: "signal-cli", done: 0, total: 0 };
        }
        let root = runtime::root(&self.data_dir);
        let status = self.rt_status.clone();
        *task = Some(tokio::spawn(async move {
            let report = |s: runtime::Status| {
                if let Ok(mut g) = status.lock() {
                    *g = s;
                }
            };
            let res = runtime::install(&root, &report).await;
            report(match res {
                Ok(_) => runtime::Status::Ready,
                Err(e) => runtime::Status::Failed(e),
            });
        }));
    }

    async fn ensure_daemon(self: &Arc<Self>) -> Result<Arc<SignalProc>, String> {
        {
            let guard = self.proc.lock().await;
            if let Some(p) = guard.as_ref() {
                if p.is_alive() {
                    return Ok(p.clone());
                }
            }
        }

        let Some(rt) = self.resolve_runtime() else {
            self.start_runtime_download().await;
            return Err(self
                .runtime_note()
                .unwrap_or_else(|| "Signal is still being set up. Try again when the download finishes.".into()));
        };
        let bin = rt.bin.clone();

        std::fs::create_dir_all(&self.data_dir)
            .map_err(|e| format!("mkdir {}: {e}", self.data_dir.display()))?;

        let mut cmd = Command::new(&bin);
        cmd.arg("--config")
            .arg(&self.data_dir)
            .arg("jsonRpc")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(home) = &rt.java_home {
            cmd.env("JAVA_HOME", home);
            let mut paths = vec![home.join("bin")];
            if let Some(p) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&p));
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                cmd.env("PATH", joined);
            }
        }
        #[cfg(windows)]
        {
            cmd.creation_flags(0x0800_0000);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn {}: {e}", bin.display()))?;
        let mut stdin = child.stdin.take().ok_or("signal-cli missing stdin")?;
        let stdout = child.stdout.take().ok_or("signal-cli missing stdout")?;
        let stderr = child.stderr.take().ok_or("signal-cli missing stderr")?;

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_read = pending.clone();
        let state_err = self.clone();
        let incoming_tx = self.incoming_tx.clone();

        let task = tokio::spawn(async move {
            let writer = tokio::spawn(async move {
                while let Some(mut line) = rx.recv().await {
                    line.push('\n');
                    if stdin.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
            });

            let state_stderr = state_err.clone();
            let err_reader = tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let trimmed = l.trim();
                    if !trimmed.is_empty() && (trimmed.contains("ERROR") || trimmed.contains("Exception") || trimmed.contains("Failed")) {
                        *state_stderr.last_error.lock().await = trimmed.chars().take(300).collect();
                    }
                }
            });

            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    let res = if let Some(err) = v.get("error") {
                        let msg = err
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| err.to_string());
                        Err(msg)
                    } else {
                        Ok(v.get("result").cloned().unwrap_or(Value::Null))
                    };
                    if let Some(tx) = pending_read.lock().await.remove(&id) {
                        let _ = tx.send(res);
                    }
                } else if v.get("method").and_then(Value::as_str) == Some("receive") {
                    if let Some(params) = v.get("params") {
                        let _ = incoming_tx.send(params.clone());
                    }
                }
            }

            writer.abort();
            err_reader.abort();
            let _ = child.kill().await;
            let mut p = pending_read.lock().await;
            for (_, tx) in p.drain() {
                let _ = tx.send(Err("signal-cli daemon exited".into()));
            }
        });

        let proc = Arc::new(SignalProc {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            task,
        });
        *self.proc.lock().await = Some(proc.clone());
        *self.last_error.lock().await = String::new();
        Ok(proc)
    }

    async fn detect_existing_account(self: &Arc<Self>) {
        if self.store.lock().await.account.is_some() {
            return;
        }
        let Ok(proc) = self.ensure_daemon().await else {
            return;
        };
        if let Ok(v) = proc.call("listAccounts", Value::Null, Duration::from_secs(10)).await {
            if let Some(acc) = v
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|item| item.get("number").and_then(Value::as_str).or_else(|| item.as_str()))
                .map(str::to_string)
            {
                let mut store = self.store.lock().await;
                store.account = Some(acc);
                let _ = save_store(&self.cfg_path, &store);
            }
        }
    }

    async fn ensure_session(self: &Arc<Self>, session_id: &str) -> Result<(), String> {
        let mut store = self.store.lock().await;
        store.sessions.entry(session_id.to_string()).or_insert_with(|| SessionBridge {
            enabled: true,
            pair_code: random_pair_code(),
            paired: None,
        });
        save_store(&self.cfg_path, &store)
    }

    /// `bridge_save` entry point: empty input = QR link (old behavior / Relink);
    /// otherwise drives the register-a-number flow step by step.
    async fn save_input(self: &Arc<Self>, session_id: &str, input: &str) -> Result<Value, String> {
        self.ensure_session(session_id).await?;
        if input.trim().is_empty() {
            *self.reg.lock().await = RegState::Idle;
            return self.start_qr_link(session_id).await;
        }
        let reg = self.reg.lock().await.clone();
        let res = match reg {
            RegState::AwaitingCode { number, captcha } => self.verify_step(&number, &captcha, input).await,
            RegState::NeedCaptcha { number } => {
                let captcha = input.trim();
                if captcha.starts_with("signalcaptcha://") {
                    self.register_step(&number, captcha, false).await
                } else {
                    Err("That isn't a CAPTCHA link. Press Solve CAPTCHA, or paste the signalcaptcha:// link.".into())
                }
            }
            RegState::Idle => match parse_register_input(input) {
                Ok(RegInput::Link) => return self.start_qr_link(session_id).await,
                Ok(RegInput::Register { number, captcha: Some(captcha) }) => self.register_step(&number, &captcha, false).await,
                Ok(RegInput::Register { number, captcha: None }) => {
                    *self.reg.lock().await = RegState::NeedCaptcha { number };
                    Ok(())
                }
                Err(e) => Err(e),
            },
        };
        // Step errors stay visible in the dialog; the prompt keeps asking.
        *self.last_error.lock().await = res.err().unwrap_or_default();
        Ok(self.status_for(session_id).await)
    }

    async fn register_step(self: &Arc<Self>, number: &str, captcha: &str, voice: bool) -> Result<(), String> {
        let proc = self.ensure_daemon().await?;
        proc.call(
            "register",
            json!({ "account": number, "captcha": captcha, "voice": voice }),
            Duration::from_secs(60),
        )
        .await
        .map_err(|e| format!("Signal refused the registration: {e}. CAPTCHAs expire within minutes: press Solve CAPTCHA again."))?;
        *self.reg.lock().await = RegState::AwaitingCode { number: number.into(), captcha: captcha.into() };
        Ok(())
    }

    async fn verify_step(self: &Arc<Self>, number: &str, captcha: &str, input: &str) -> Result<(), String> {
        let (code, pin) = match parse_code_input(input)? {
            CodeInput::Voice => return self.register_step(number, captcha, true).await,
            CodeInput::Code { code, pin } => (code, pin),
        };
        let proc = self.ensure_daemon().await?;
        let mut params = json!({ "account": number, "verificationCode": code });
        if let Some(p) = pin {
            params["pin"] = Value::String(p);
        }
        proc.call("verify", params, Duration::from_secs(60))
            .await
            .map_err(|e| format!("Verification failed: {e}. If the number has a registration lock, add its PIN after the code."))?;
        let _ = proc
            .call("updateProfile", json!({ "account": number, "givenName": "Vicuna" }), Duration::from_secs(30))
            .await;
        {
            let mut store = self.store.lock().await;
            store.account = Some(number.into());
            store.registered = true;
            save_store(&self.cfg_path, &store)?;
        }
        *self.reg.lock().await = RegState::Idle;
        Ok(())
    }

    async fn start_qr_link(self: &Arc<Self>, session_id: &str) -> Result<Value, String> {
        self.ensure_session(session_id).await?;

        // If already linked and not explicitly relinking, still allow refreshing QR when requested
        let proc = self.ensure_daemon().await?;
        let res = proc
            .call("startLink", Value::Null, Duration::from_secs(15))
            .await?;
        let uri = res
            .get("deviceLinkUri")
            .and_then(Value::as_str)
            .ok_or("signal-cli startLink returned no deviceLinkUri")?
            .to_string();
        let data_url = qr_svg_data_url(&uri)?;

        *self.qr_data_url.lock().await = Some(data_url);
        *self.last_error.lock().await = String::new();

        // Cancel any previous finishLink waiter
        if let Some(prev) = self.linking_task.lock().await.take() {
            prev.abort();
        }

        let state = self.clone();
        let proc_bg = proc.clone();
        let handle = tokio::spawn(async move {
            let finish = proc_bg
                .call(
                    "finishLink",
                    json!({
                        "deviceLinkUri": uri,
                        "deviceName": "Vicuna",
                    }),
                    Duration::from_secs(180),
                )
                .await;
            match finish {
                Ok(v) => {
                    let number = v
                        .get("number")
                        .and_then(Value::as_str)
                        .or_else(|| v.get("username").and_then(Value::as_str))
                        .map(str::to_string);
                    if let Some(acc) = number {
                        let mut store = state.store.lock().await;
                        store.account = Some(acc);
                        store.registered = false;
                        let _ = save_store(&state.cfg_path, &store);
                    }
                    *state.qr_data_url.lock().await = None;
                    *state.last_error.lock().await = String::new();
                }
                Err(e) => {
                    *state.qr_data_url.lock().await = None;
                    *state.last_error.lock().await = format!("Linking failed: {e}");
                }
            }
        });
        *self.linking_task.lock().await = Some(handle);

        Ok(self.status_for(session_id).await)
    }

    async fn status_for(self: &Arc<Self>, session_id: &str) -> Value {
        let store = self.store.lock().await.clone();
        let qr = self.qr_data_url.lock().await.clone();
        let last_error = self.last_error.lock().await.clone();
        let alive = self
            .proc
            .lock()
            .await
            .as_ref()
            .map(|p| p.is_alive())
            .unwrap_or(false);

        let sess = store.sessions.get(session_id);
        let reg = self.reg.lock().await.clone();
        let configured = sess.is_some() && (store.account.is_some() || qr.is_some());
        let hint = if store.registered {
            format!(
                "Send this to {} in a direct message, or in a group with just you and that number:",
                store.account.clone().unwrap_or_default()
            )
        } else {
            "Send this in Note to Self or a solo Signal group:".to_string()
        };
        let enabled = sess.map(|s| s.enabled).unwrap_or(false);
        let paired = sess.and_then(|s| s.paired.as_ref()).map(|p| {
            json!({
                "id": p.target.id_string(),
                "name": p.name,
            })
        });
        let pair_code = sess.map(|s| s.pair_code.clone()).unwrap_or_default();

        let mut obj = json!({
            "sessionId": session_id,
            "configured": configured,
            "enabled": enabled,
            "connected": alive && store.account.is_some(),
            "paired": paired,
            "botUsername": store.account.clone().unwrap_or_default(),
            "pairCode": pair_code,
            "pairingHint": hint,
            "lastError": last_error,
        });
        if let Some(q) = qr {
            obj["qrDataUrl"] = Value::String(q);
        } else if store.account.is_none() {
            let mut prompt = input_prompt(&reg);
            if let Some(note) = self.runtime_note() {
                prompt["note"] = Value::String(note);
            }
            obj["inputPrompt"] = prompt;
        }
        obj
    }

    async fn send_signal(
        self: &Arc<Self>,
        target: &SignalTarget,
        text: &str,
        edit_ts: Option<i64>,
    ) -> Result<Option<i64>, String> {
        let account = self
            .store
            .lock()
            .await
            .account
            .clone()
            .ok_or("Signal account is not linked")?;
        let proc = self.ensure_daemon().await?;

        let mut params = json!({
            "account": account,
            "message": text,
        });
        match target {
            SignalTarget::NoteToSelf => {
                params["noteToSelf"] = Value::Bool(true);
            }
            SignalTarget::Group { group_id } => {
                params["groupId"] = json!([group_id]);
            }
            SignalTarget::Recipient { address } => {
                params["recipient"] = json!([address]);
            }
        }
        if let Some(ts) = edit_ts {
            params["editTimestamp"] = json!(ts);
        }

        let res = proc.call("send", params, Duration::from_secs(20)).await?;
        let ts = res.get("timestamp").and_then(Value::as_i64);
        if let Some(t) = ts {
            self.sent.lock().await.record(t);
        }
        Ok(ts)
    }

    async fn send_chunked(self: &Arc<Self>, target: &SignalTarget, text: &str) -> Result<Option<i64>, String> {
        let mut last_ts = None;
        for chunk in chunk_text(text, MAX_MSG_CHARS) {
            last_ts = self.send_signal(target, &chunk, None).await?;
        }
        Ok(last_ts)
    }
}

// ---------- Incoming Signal message parsing ----------

#[derive(Debug, Clone, PartialEq, Eq)]
struct IncomingMsg {
    target: SignalTarget,
    chat_name_hint: Option<String>,
    text: String,
    timestamp: i64,
    attachments: Vec<IncomingAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IncomingAttachment {
    id: String,
    filename: Option<String>,
    content_type: String,
}

fn parse_attachments(v: Option<&Value>) -> Vec<IncomingAttachment> {
    let Some(arr) = v.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|a| {
            let id = a.get("id").and_then(Value::as_str)?.to_string();
            let filename = a.get("filename").and_then(Value::as_str).map(str::to_string);
            let content_type = a
                .get("contentType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_string();
            Some(IncomingAttachment {
                id,
                filename,
                content_type,
            })
        })
        .collect()
}

fn parse_receive_params(params: &Value, own_account: Option<&str>) -> Option<IncomingMsg> {
    let env = params.get("envelope")?;
    let account = params
        .get("account")
        .and_then(Value::as_str)
        .or(own_account);

    // 1. Messages sent by the user on another device (Note to Self or solo group)
    if let Some(sent) = env.get("syncMessage").and_then(|s| s.get("sentMessage")) {
        let ts = sent
            .get("timestamp")
            .and_then(Value::as_i64)
            .or_else(|| env.get("timestamp").and_then(Value::as_i64))
            .unwrap_or(0);
        let text = sent
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let attachments = parse_attachments(sent.get("attachments"));
        if text.trim().is_empty() && attachments.is_empty() {
            return None;
        }
        if let Some(gid) = sent
            .get("groupInfo")
            .and_then(|g| g.get("groupId"))
            .and_then(Value::as_str)
        {
            let name = sent
                .get("groupInfo")
                .and_then(|g| g.get("groupName"))
                .and_then(Value::as_str)
                .map(str::to_string);
            return Some(IncomingMsg {
                target: SignalTarget::Group {
                    group_id: gid.to_string(),
                },
                chat_name_hint: name,
                text,
                timestamp: ts,
                attachments,
            });
        }
        let dest = sent
            .get("destinationNumber")
            .and_then(Value::as_str)
            .or_else(|| sent.get("destination").and_then(Value::as_str));
        let is_self = match (dest, account) {
            (Some(d), Some(a)) => d == a,
            (None, _) => true,
            _ => false,
        };
        if is_self {
            return Some(IncomingMsg {
                target: SignalTarget::NoteToSelf,
                chat_name_hint: Some("Note to Self".into()),
                text,
                timestamp: ts,
                attachments,
            });
        }
        // Ignore outgoing messages the user sent to other people
        return None;
    }

    // 2. Incoming messages from another Signal user (dedicated bot number or group)
    if let Some(dm) = env.get("dataMessage") {
        let ts = dm
            .get("timestamp")
            .and_then(Value::as_i64)
            .or_else(|| env.get("timestamp").and_then(Value::as_i64))
            .unwrap_or(0);
        let text = dm
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let attachments = parse_attachments(dm.get("attachments"));
        if text.trim().is_empty() && attachments.is_empty() {
            return None;
        }
        if let Some(gid) = dm
            .get("groupInfo")
            .and_then(|g| g.get("groupId"))
            .and_then(Value::as_str)
        {
            let name = dm
                .get("groupInfo")
                .and_then(|g| g.get("groupName"))
                .and_then(Value::as_str)
                .map(str::to_string);
            return Some(IncomingMsg {
                target: SignalTarget::Group {
                    group_id: gid.to_string(),
                },
                chat_name_hint: name,
                text,
                timestamp: ts,
                attachments,
            });
        }
        let src = env
            .get("sourceNumber")
            .and_then(Value::as_str)
            .or_else(|| env.get("sourceUuid").and_then(Value::as_str))
            .or_else(|| env.get("source").and_then(Value::as_str))?;
        let name = env
            .get("sourceName")
            .and_then(Value::as_str)
            .map(str::to_string);
        let target = if account == Some(src) {
            SignalTarget::NoteToSelf
        } else {
            SignalTarget::Recipient {
                address: src.to_string(),
            }
        };
        return Some(IncomingMsg {
            target,
            chat_name_hint: name,
            text,
            timestamp: ts,
            attachments,
        });
    }

    None
}

// ---------- Incoming message handler ----------

async fn handle_incoming(state: Arc<AppState>, params: Value) {
    let own = state.store.lock().await.account.clone();
    let Some(msg) = parse_receive_params(&params, own.as_deref()) else {
        return;
    };

    // Ignore echoes of messages we sent ourselves
    if msg.timestamp > 0 && state.sent.lock().await.contains(msg.timestamp) {
        return;
    }

    let trimmed = msg.text.trim();

    // Handle !pair <code>
    if let Some(rest) = trimmed.strip_prefix("!pair") {
        let code = rest.trim();
        if !code.is_empty() {
            let matched = {
                let mut store = state.store.lock().await;
                let sid = store
                    .sessions
                    .iter()
                    .find(|(_, s)| s.pair_code == code)
                    .map(|(id, _)| id.clone());
                if let Some(id) = sid {
                    let display = msg.target.display_name(msg.chat_name_hint.as_deref());
                    if let Some(s) = store.sessions.get_mut(&id) {
                        s.paired = Some(PairedTarget {
                            target: msg.target.clone(),
                            name: display.clone(),
                        });
                    }
                    let _ = save_store(&state.cfg_path, &store);
                    Some((id, display))
                } else {
                    None
                }
            };
            if let Some((sid, _)) = matched {
                let title = state
                    .host
                    .call("session_info", json!({ "sessionId": sid }))
                    .await
                    .ok()
                    .and_then(|v| v.get("title").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| "this Work session".into());
                let _ = state
                    .send_signal(
                        &msg.target,
                        &format!("✅ Paired with Vicuna session \"{title}\". Send any message to start, or !help for commands."),
                        None,
                    )
                    .await;
            } else {
                let _ = state
                    .send_signal(&msg.target, "❌ Invalid pair code. Check Vicuna's Signal bridge modal.", None)
                    .await;
            }
            return;
        }
    }

    // Find which enabled session is paired with this target
    let session_id = {
        let store = state.store.lock().await;
        store
            .sessions
            .iter()
            .find(|(_, s)| {
                s.enabled
                    && s.paired
                        .as_ref()
                        .map(|p| p.target == msg.target)
                        .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
    };
    let Some(sid) = session_id else {
        return;
    };

    // Built-in commands
    if trimmed == "!help" {
        let _ = state
            .send_signal(
                &msg.target,
                "Commands:\n!status — session info\n!stop — stop current turn\n!send — send queued messages now\n!model — show or switch model (!model <engine> <model>)\n!help — show this message",
                None,
            )
            .await;
        return;
    }

    if trimmed == "!stop" {
        match state.host.call("stop_turn", json!({ "sessionId": sid })).await {
            Ok(v) if v.get("stopped").and_then(Value::as_bool) == Some(true) => {
                let _ = state.send_signal(&msg.target, "⏹ Stopped.", None).await;
            }
            Ok(_) => {
                let _ = state.send_signal(&msg.target, "Nothing is running right now.", None).await;
            }
            Err(e) => {
                let _ = state.send_signal(&msg.target, &format!("Error: {e}"), None).await;
            }
        }
        return;
    }

    if trimmed == "!send" {
        match state.host.call("send_now", json!({ "sessionId": sid })).await {
            Ok(v) => {
                let n = v.get("sent").and_then(Value::as_u64).unwrap_or(0);
                let reply = if n > 0 {
                    format!("Sent {n} queued message(s).")
                } else {
                    "No queued messages.".into()
                };
                let _ = state.send_signal(&msg.target, &reply, None).await;
            }
            Err(e) => {
                let _ = state.send_signal(&msg.target, &format!("Error: {e}"), None).await;
            }
        }
        return;
    }

    if trimmed == "!status" {
        match state.host.call("session_info", json!({ "sessionId": sid })).await {
            Ok(v) => {
                let title = v.get("title").and_then(Value::as_str).unwrap_or("Session");
                let engine = v.get("engine").and_then(Value::as_str).unwrap_or("?");
                let model = v.get("model").and_then(Value::as_str).unwrap_or("default");
                let working = v.get("working").and_then(Value::as_bool).unwrap_or(false);
                let st = if working { "working" } else { "idle" };
                let _ = state
                    .send_signal(&msg.target, &format!("{title}\nEngine: {engine}/{model}\nState: {st}"), None)
                    .await;
            }
            Err(e) => {
                let _ = state.send_signal(&msg.target, &format!("Error: {e}"), None).await;
            }
        }
        return;
    }

    if trimmed == "!model" || trimmed.starts_with("!model ") {
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() == 1 {
            if let Ok(v) = state.host.call("session_info", json!({ "sessionId": sid })).await {
                let engine = v.get("engine").and_then(Value::as_str).unwrap_or("?");
                let model = v.get("model").and_then(Value::as_str).unwrap_or("default");
                let _ = state
                    .send_signal(
                        &msg.target,
                        &format!("Current: {engine}/{model}\nUse `!model <engine> <model>` to switch."),
                        None,
                    )
                    .await;
            }
        } else if parts.len() >= 3 {
            let engine = parts[1];
            let model = parts[2..].join(" ");
            match state
                .host
                .call(
                    "set_model",
                    json!({ "sessionId": sid, "engine": engine, "model": model }),
                )
                .await
            {
                Ok(v) => {
                    let m = v.get("message").and_then(Value::as_str).unwrap_or("Updated.");
                    let _ = state.send_signal(&msg.target, m, None).await;
                }
                Err(e) => {
                    let _ = state.send_signal(&msg.target, &format!("Error: {e}"), None).await;
                }
            }
        } else {
            let _ = state
                .send_signal(&msg.target, "Usage: !model <engine> <model>", None)
                .await;
        }
        return;
    }

    // Check if there is a pending `ask` question for this session
    let pending_ask = state
        .turns
        .lock()
        .await
        .get(&sid)
        .and_then(|t| t.pending_ask.clone());
    if let Some(ask) = pending_ask {
        if !trimmed.is_empty() {
            let answer_params = if let Ok(n) = trimmed.parse::<usize>() {
                if n >= 1 && n <= ask.options.len() {
                    Some(json!({ "sessionId": sid, "choice": n - 1 }))
                } else if ask.freeform {
                    Some(json!({ "sessionId": sid, "text": trimmed }))
                } else {
                    None
                }
            } else if ask.freeform {
                Some(json!({ "sessionId": sid, "text": trimmed }))
            } else {
                None
            };
            if let Some(p) = answer_params {
                match state.host.call("answer_ask", p).await {
                    Ok(v) => {
                        if let Some(t) = state.turns.lock().await.get_mut(&sid) {
                            t.pending_ask = None;
                        }
                        let shown = v.get("shown").and_then(Value::as_str).unwrap_or(trimmed);
                        let _ = state.send_signal(&msg.target, &format!("✓ Answered: {shown}"), None).await;
                    }
                    Err(e) => {
                        let _ = state.send_signal(&msg.target, &format!("Could not answer: {e}"), None).await;
                    }
                }
                return;
            }
        }
    }

    // Process attachments (images, voice memos, files)
    let mut text = msg.text.clone();
    let mut images: Vec<Value> = Vec::new();
    let att_dir = state.data_dir.join("attachments");

    for att in &msg.attachments {
        let path = att_dir.join(&att.id);
        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };
        let mime = att.content_type.to_lowercase();
        if mime.starts_with("image/") {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            images.push(Value::String(format!("data:{mime};base64,{b64}")));
        } else if mime.starts_with("audio/") {
            let ogg_bytes = if mime.contains("ogg") || mime.contains("opus") {
                Some(bytes.clone())
            } else {
                transcode_to_ogg(&bytes).await
            };
            let mut transcribed = false;
            if let Some(ogg) = ogg_bytes {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&ogg);
                if let Ok(v) = state.host.call("transcribe_ogg", json!({ "base64": b64 })).await {
                    if let Some(t) = v.get("text").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t.trim());
                        transcribed = true;
                    }
                }
            }
            if !transcribed {
                let name = att.filename.clone().unwrap_or_else(|| format!("{}.audio", att.id));
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                if let Ok(v) = state
                    .host
                    .call("stage_upload", json!({ "sessionId": sid, "name": name, "base64": b64 }))
                    .await
                {
                    if let Some(note) = v.get("note").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(note);
                    }
                }
            }
        } else {
            let name = att.filename.clone().unwrap_or_else(|| att.id.clone());
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            if let Ok(v) = state
                .host
                .call("stage_upload", json!({ "sessionId": sid, "name": name, "base64": b64 }))
                .await
            {
                if let Some(note) = v.get("note").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(note);
                }
            }
        }
    }

    if text.trim().is_empty() && images.is_empty() {
        return;
    }

    match state
        .host
        .call(
            "deliver_message",
            json!({
                "sessionId": sid,
                "text": text,
                "images": images,
            }),
        )
        .await
    {
        Ok(v) => {
            if v.get("queued").and_then(Value::as_bool) == Some(true) {
                let _ = state
                    .send_signal(&msg.target, "⏳ Queued (send !send to interrupt now).", None)
                    .await;
            }
        }
        Err(e) => {
            let _ = state.send_signal(&msg.target, &format!("Error: {e}"), None).await;
        }
    }
}

async fn transcode_to_ogg(input: &[u8]) -> Option<Vec<u8>> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-i", "pipe:0", "-c:a", "libopus", "-f", "ogg", "pipe:1"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let data = input.to_vec();
    tokio::spawn(async move {
        let _ = stdin.write_all(&data).await;
        let _ = stdin.shutdown().await;
    });
    let out = child.wait_with_output().await.ok()?;
    if out.status.success() && !out.stdout.is_empty() {
        Some(out.stdout)
    } else {
        None
    }
}

// ---------- Turn events (Vicuna -> Signal) ----------

async fn handle_turn_event(state: Arc<AppState>, params: &Value) {
    let event = params.get("event").and_then(Value::as_str).unwrap_or("");
    let sid = params.get("sessionId").and_then(Value::as_str).unwrap_or("");
    let turn_id = params.get("turnId").and_then(Value::as_str).unwrap_or("");
    if sid.is_empty() {
        return;
    }

    let target = {
        let store = state.store.lock().await;
        store
            .sessions
            .get(sid)
            .filter(|s| s.enabled)
            .and_then(|s| s.paired.as_ref())
            .map(|p| p.target.clone())
    };
    let Some(target) = target else {
        return;
    };

    match event {
        "started" => {
            let ts = state.send_signal(&target, "🦙 Working…", None).await.ok().flatten();
            state.turns.lock().await.insert(
                sid.to_string(),
                ActiveTurn {
                    turn_id: turn_id.to_string(),
                    target,
                    msg_timestamp: ts,
                    edit_count: 0,
                    pending_ask: None,
                },
            );
        }
        "progress" => {
            let text = params.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                return;
            }
            let preview = if text.chars().count() > MAX_MSG_CHARS {
                let cut: String = text.chars().take(MAX_MSG_CHARS - 20).collect();
                format!("🦙 {cut}…")
            } else {
                format!("🦙 {text}")
            };
            let edit_info = {
                let mut turns = state.turns.lock().await;
                turns.get_mut(sid).and_then(|t| {
                    if t.turn_id == turn_id && t.edit_count < MAX_PROGRESS_EDITS {
                        t.msg_timestamp.map(|ts| {
                            t.edit_count += 1;
                            (t.target.clone(), ts)
                        })
                    } else {
                        None
                    }
                })
            };
            if let Some((tgt, ts)) = edit_info {
                let _ = state.send_signal(&tgt, &preview, Some(ts)).await;
            }
        }
        "ask" => {
            let question = params.get("question").and_then(Value::as_str).unwrap_or("Question:");
            let options: Vec<String> = params
                .get("options")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let freeform = params.get("freeform").and_then(Value::as_bool).unwrap_or(true);

            let mut msg = format!("❓ {question}");
            if !options.is_empty() {
                msg.push('\n');
                for (i, opt) in options.iter().enumerate() {
                    msg.push_str(&format!("\n{}. {opt}", i + 1));
                }
                msg.push_str("\n\nReply with a number");
                if freeform {
                    msg.push_str(" or your own text.");
                } else {
                    msg.push('.');
                }
            }
            {
                let mut turns = state.turns.lock().await;
                let entry = turns.entry(sid.to_string()).or_insert_with(|| ActiveTurn {
                    turn_id: turn_id.to_string(),
                    target: target.clone(),
                    msg_timestamp: None,
                    edit_count: 0,
                    pending_ask: None,
                });
                entry.pending_ask = Some(PendingAsk { options, freeform });
            }
            let _ = state.send_chunked(&target, &msg).await;
        }
        "finished" => {
            let reply = params.get("reply").and_then(Value::as_str).unwrap_or("(no reply text)");
            let active = state.turns.lock().await.remove(sid);
            let final_text = format!("🦙 {reply}");
            let can_edit = active
                .as_ref()
                .and_then(|t| {
                    if t.turn_id == turn_id && t.edit_count < 10 && final_text.chars().count() <= MAX_MSG_CHARS {
                        t.msg_timestamp
                    } else {
                        None
                    }
                });
            if let Some(ts) = can_edit {
                if state.send_signal(&target, &final_text, Some(ts)).await.is_err() {
                    let _ = state.send_chunked(&target, &final_text).await;
                }
            } else {
                let _ = state.send_chunked(&target, &final_text).await;
            }
        }
        _ => {}
    }
}

// ---------- Host -> Plugin RPC dispatcher ----------

async fn handle_host_request(state: Arc<AppState>, method: &str, params: &Value) -> Result<Value, String> {
    let sid = params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .and_then(Value::as_str)
        .unwrap_or("");

    match method {
        "bridge_list" => {
            let sids: Vec<String> = state.store.lock().await.sessions.keys().cloned().collect();
            let mut bridges = serde_json::Map::new();
            for id in sids {
                bridges.insert(id.clone(), state.status_for(&id).await);
            }
            Ok(json!({ "bridges": bridges }))
        }
        "bridge_status" => {
            if sid.is_empty() {
                return Err("missing sessionId".into());
            }
            state.detect_existing_account().await;
            Ok(state.status_for(sid).await)
        }
        "bridge_save" => {
            if sid.is_empty() {
                return Err("missing sessionId".into());
            }
            let input = params.get("token").and_then(Value::as_str).unwrap_or("");
            state.save_input(sid, input).await
        }
        "bridge_set" => {
            if sid.is_empty() {
                return Err("missing sessionId".into());
            }
            if let Some(enabled) = params.get("enabled").and_then(Value::as_bool) {
                let mut store = state.store.lock().await;
                if let Some(s) = store.sessions.get_mut(sid) {
                    s.enabled = enabled;
                    save_store(&state.cfg_path, &store)?;
                }
            }
            Ok(state.status_for(sid).await)
        }
        "bridge_unpair" => {
            if sid.is_empty() {
                return Err("missing sessionId".into());
            }
            {
                let mut store = state.store.lock().await;
                if let Some(s) = store.sessions.get_mut(sid) {
                    s.paired = None;
                    s.pair_code = random_pair_code();
                    save_store(&state.cfg_path, &store)?;
                }
            }
            Ok(state.status_for(sid).await)
        }
        "bridge_remove" => {
            if sid.is_empty() {
                return Err("missing sessionId".into());
            }
            {
                let mut store = state.store.lock().await;
                store.sessions.remove(sid);
                save_store(&state.cfg_path, &store)?;
                if store.account.is_none() {
                    *state.reg.lock().await = RegState::Idle;
                    *state.last_error.lock().await = String::new();
                }
            }
            state.turns.lock().await.remove(sid);
            Ok(state.status_for(sid).await)
        }
        _ => Err(format!("unknown method {method}")),
    }
}

fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for line in text.split_inclusive('\n') {
        let line_len = line.chars().count();
        if cur_len + line_len > max_chars && !cur.is_empty() {
            out.push(cur.trim_end().to_string());
            cur = String::new();
            cur_len = 0;
        }
        if line_len > max_chars {
            for ch in line.chars() {
                if cur_len >= max_chars {
                    out.push(cur);
                    cur = String::new();
                    cur_len = 0;
                }
                cur.push(ch);
                cur_len += 1;
            }
        } else {
            cur.push_str(line);
            cur_len += line_len;
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim_end().to_string());
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[tokio::main]
async fn main() {
    let cfg_path = config_path();
    let data_dir = signal_cli_data_dir(&cfg_path);
    let initial_store = load_store(&cfg_path);

    let (stdout_tx, mut stdout_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(mut line) = stdout_rx.recv().await {
            line.push('\n');
            if out.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = out.flush().await;
        }
    });

    let host = Arc::new(HostRpc::new(stdout_tx.clone()));
    let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel::<Value>();

    let state = Arc::new(AppState {
        cfg_path,
        data_dir,
        store: Mutex::new(initial_store),
        host: host.clone(),
        proc: Mutex::new(None),
        qr_data_url: Mutex::new(None),
        reg: Mutex::new(RegState::Idle),
        rt_status: Arc::new(std::sync::Mutex::new(runtime::Status::Idle)),
        rt_task: Mutex::new(None),
        last_error: Mutex::new(String::new()),
        linking_task: Mutex::new(None),
        turns: Mutex::new(HashMap::new()),
        sent: Mutex::new(SentTracker::default()),
        incoming_tx,
    });

    // If an account is already linked, start signal-cli daemon right away
    {
        let st_boot = state.clone();
        tokio::spawn(async move {
            if st_boot.store.lock().await.account.is_some() {
                let _ = st_boot.ensure_daemon().await;
            }
        });
    }

    // Incoming Signal messages loop
    {
        let st_in = state.clone();
        tokio::spawn(async move {
            while let Some(params) = incoming_rx.recv().await {
                let st = st_in.clone();
                tokio::spawn(async move {
                    handle_incoming(st, params).await;
                });
            }
        });
    }

    // Read Vicuna host JSON-RPC messages from stdin
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        if let Some(method) = v.get("method").and_then(Value::as_str) {
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = v.get("id").and_then(Value::as_u64) {
                let st = state.clone();
                let tx = stdout_tx.clone();
                let method = method.to_string();
                tokio::spawn(async move {
                    let reply = match handle_host_request(st, &method, &params).await {
                        Ok(res) => json!({ "id": id, "result": res }),
                        Err(e) => json!({ "id": id, "error": e }),
                    };
                    let _ = tx.send(reply.to_string());
                });
            } else {
                let st = state.clone();
                let method = method.to_string();
                tokio::spawn(async move {
                    match method.as_str() {
                        "turn_event" => handle_turn_event(st, &params).await,
                        "session_deleted" => {
                            if let Some(sid) = params.get("sessionId").and_then(Value::as_str) {
                                let mut store = st.store.lock().await;
                                if store.sessions.remove(sid).is_some() {
                                    let _ = save_store(&st.cfg_path, &store);
                                }
                                st.turns.lock().await.remove(sid);
                            }
                        }
                        _ => {}
                    }
                });
            }
        } else if let Some(id) = v.get("id").and_then(Value::as_u64) {
            let res = if let Some(err) = v.get("error") {
                Err(err
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| err.to_string()))
            } else {
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            };
            host.resolve(id, res).await;
        }
    }

    writer.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_valid_svg_qr_data_url() {
        let url = qr_svg_data_url("sgnl://linkdevice?uuid=abc&pub_key=xyz").unwrap();
        assert!(url.starts_with("data:image/svg+xml;base64,"));
        let raw = base64::engine::general_purpose::STANDARD
            .decode(url.strip_prefix("data:image/svg+xml;base64,").unwrap())
            .unwrap();
        let xml = String::from_utf8(raw).unwrap();
        assert!(xml.contains("<svg"));
    }

    #[test]
    fn parses_note_to_self_sync_message() {
        let params = json!({
            "account": "+15551234567",
            "envelope": {
                "timestamp": 1700000000000_i64,
                "syncMessage": {
                    "sentMessage": {
                        "destinationNumber": "+15551234567",
                        "timestamp": 1700000000000_i64,
                        "message": "!pair 123456"
                    }
                }
            }
        });
        let msg = parse_receive_params(&params, Some("+15551234567")).unwrap();
        assert_eq!(msg.target, SignalTarget::NoteToSelf);
        assert_eq!(msg.text, "!pair 123456");
        assert_eq!(msg.timestamp, 1700000000000);
    }

    #[test]
    fn parses_solo_group_sync_message_and_ignores_other_dms() {
        let group_params = json!({
            "account": "+15551234567",
            "envelope": {
                "syncMessage": {
                    "sentMessage": {
                        "timestamp": 1700000000001_i64,
                        "message": "hello solo group",
                        "groupInfo": {
                            "groupId": "abc123==",
                            "groupName": "Vicuna Work"
                        }
                    }
                }
            }
        });
        let msg = parse_receive_params(&group_params, Some("+15551234567")).unwrap();
        assert_eq!(
            msg.target,
            SignalTarget::Group {
                group_id: "abc123==".into()
            }
        );
        assert_eq!(msg.chat_name_hint.as_deref(), Some("Vicuna Work"));

        let other_dm = json!({
            "account": "+15551234567",
            "envelope": {
                "syncMessage": {
                    "sentMessage": {
                        "destinationNumber": "+15559999999",
                        "timestamp": 1700000000002_i64,
                        "message": "private message to friend"
                    }
                }
            }
        });
        assert!(parse_receive_params(&other_dm, Some("+15551234567")).is_none());
    }

    #[test]
    fn parses_incoming_dm_from_other_user() {
        let dm = json!({
            "account": "+15550000000",
            "envelope": {
                "sourceNumber": "+15551234567",
                "sourceName": "Alice",
                "dataMessage": {
                    "timestamp": 1700000000003_i64,
                    "message": "run tests"
                }
            }
        });
        let msg = parse_receive_params(&dm, Some("+15550000000")).unwrap();
        assert_eq!(
            msg.target,
            SignalTarget::Recipient {
                address: "+15551234567".into()
            }
        );
        assert_eq!(msg.chat_name_hint.as_deref(), Some("Alice"));
        assert_eq!(msg.text, "run tests");
    }

    #[test]
    fn chunks_long_messages_without_losing_content() {
        let text = "line1\nline2\nline3\nline4";
        let chunks = chunk_text(text, 12);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|c| c.chars().count() <= 12));
        assert_eq!(chunks.join("\n"), text);
    }

    #[test]
    fn sent_tracker_deduplicates_and_evicts() {
        let mut tracker = SentTracker::default();
        tracker.record(42);
        assert!(tracker.contains(42));
        assert!(!tracker.contains(43));
    }

    #[test]
    fn parses_register_input() {
        assert_eq!(parse_register_input(" link "), Ok(RegInput::Link));
        assert_eq!(
            parse_register_input("+1 555-123-4567 signalcaptcha://abc.def"),
            Ok(RegInput::Register { number: "+15551234567".into(), captcha: Some("signalcaptcha://abc.def".into()) })
        );
        assert_eq!(
            parse_register_input("signalcaptcha://x +447700900123"),
            Ok(RegInput::Register { number: "+447700900123".into(), captcha: Some("signalcaptcha://x".into()) })
        );
        assert_eq!(parse_register_input("+15551234567"), Ok(RegInput::Register { number: "+15551234567".into(), captcha: None }));
        assert!(parse_register_input("5551234567 signalcaptcha://x").is_err());
        assert!(parse_register_input("+12 signalcaptcha://x").is_err());
    }

    #[test]
    fn parses_code_input() {
        assert_eq!(parse_code_input("voice"), Ok(CodeInput::Voice));
        assert_eq!(parse_code_input("123-456"), Ok(CodeInput::Code { code: "123456".into(), pin: None }));
        assert_eq!(parse_code_input("123456 9876"), Ok(CodeInput::Code { code: "123456".into(), pin: Some("9876".into()) }));
        assert!(parse_code_input("12345").is_err());
        assert!(parse_code_input("abcdef").is_err());
    }

    #[test]
    fn input_prompt_follows_registration_step() {
        let a = input_prompt(&RegState::Idle);
        assert!(a["label"].as_str().unwrap().contains("type link"));
        let c = input_prompt(&RegState::NeedCaptcha { number: "+15551234567".into() });
        assert_eq!(c["capture"]["scheme"], "signalcaptcha");
        assert_eq!(c["capture"]["url"], CAPTCHA_URL);
        let b = input_prompt(&RegState::AwaitingCode { number: "+15551234567".into(), captcha: "c".into() });
        assert!(b["label"].as_str().unwrap().contains("+15551234567"));
        assert_eq!(b["submitLabel"], "Verify");
    }
}
