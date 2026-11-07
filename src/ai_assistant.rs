//! Local AI clients: reuse their own authentication and protocols, never copy tokens.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

const MAX_LINE: usize = 8 * 1024 * 1024;
pub(crate) const MAX_TEXT: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Provider {
    Codex,
    Claude,
}
impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    History,
    Idle,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}
impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::History => "历史记录",
            Self::Idle => "空闲",
            Self::Running => "进行中",
            Self::Waiting => "等待处理",
            Self::Completed => "已完成",
            Self::Failed => "失败",
            Self::Cancelled => "已停止",
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct Agent {
    pub provider: Provider,
    pub available: bool,
    pub note: String,
}
#[derive(Clone, Debug)]
pub(crate) struct Task {
    pub provider: Provider,
    pub id: String,
    pub title: String,
    pub status: Status,
    pub updated: i64,
    pub owned: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct Message {
    pub id: String,
    pub user: bool,
    pub text: String,
}
pub(crate) enum Control {
    Discover,
    List {
        provider: Provider,
        cursor: Option<String>,
    },
    Read {
        provider: Provider,
        id: String,
    },
    Send {
        provider: Provider,
        id: Option<String>,
        text: String,
    },
    Cancel,
    Shutdown,
}
pub(crate) enum Event {
    Agents(Vec<Agent>),
    Tasks {
        provider: Provider,
        tasks: Vec<Task>,
        next: Option<String>,
        append: bool,
    },
    History {
        provider: Provider,
        id: String,
        messages: Vec<Message>,
    },
    Started {
        provider: Provider,
        id: String,
        title: String,
        forked: bool,
    },
    Text {
        id: String,
        item: String,
        text: String,
        replace: bool,
    },
    Status {
        id: String,
        status: Status,
        note: String,
    },
    Error(String),
}
pub(crate) struct Service {
    tx: Sender<Control>,
    pub rx: Receiver<Event>,
}
impl Service {
    pub fn start(ctx: eframe::egui::Context) -> Result<Self, String> {
        let (tx, commands) = mpsc::channel();
        let (events, rx) = mpsc::channel();
        thread::Builder::new()
            .name("daozai-ai".into())
            .spawn(move || Worker::new(commands, events, ctx).run())
            .map_err(|_| "无法启动 AI 助手".to_string())?;
        Ok(Self { tx, rx })
    }
    pub fn send(&self, command: Control) -> Result<(), String> {
        self.tx
            .send(command)
            .map_err(|_| "AI 连接已退出，请重新识别".into())
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.tx.send(Control::Shutdown);
    }
}

fn hidden(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}
fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}
pub(crate) fn find_client(provider: Provider) -> Option<PathBuf> {
    let name = match provider {
        Provider::Codex => "codex",
        Provider::Claude => "claude",
    };
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join(if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.into()
            }));
        }
    }
    if let Some(roaming) = std::env::var_os("APPDATA") {
        let npm = PathBuf::from(roaming).join("npm/node_modules");
        match provider {
            Provider::Codex => {
                for arch in ["codex-win32-x64", "codex-win32-arm64"] {
                    let triple = if arch.ends_with("arm64") {
                        "aarch64-pc-windows-msvc"
                    } else {
                        "x86_64-pc-windows-msvc"
                    };
                    for folder in ["bin", "codex"] {
                        candidates.push(npm.join(format!("@openai/codex/node_modules/@openai/{arch}/vendor/{triple}/{folder}/codex.exe")));
                    }
                }
                candidates
                    .push(npm.join("@openai/codex/vendor/x86_64-pc-windows-msvc/codex/codex.exe"));
            }
            Provider::Claude => {
                candidates.push(npm.join("@anthropic-ai/claude-code/bin/claude.exe"))
            }
        }
    }
    if provider == Provider::Codex {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let root = PathBuf::from(local).join("OpenAI/Codex/bin");
            let mut installs: Vec<_> = fs::read_dir(root)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .collect();
            installs
                .sort_by_key(|e| std::cmp::Reverse(e.metadata().and_then(|m| m.modified()).ok()));
            for install in installs.into_iter().take(20) {
                candidates.push(install.path().join("codex.exe"));
            }
        }
    }
    candidates.push(home().join(".local/bin").join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    }));
    candidates.into_iter().find(|p| p.is_file())
}

struct Managed(Child);
impl Drop for Managed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn output_json(path: &Path, args: &[&str]) -> Result<Value, String> {
    let mut command = Command::new(path);
    hidden(&mut command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = Managed(command.spawn().map_err(|_| "无法启动本机客户端")?);
    let stdout = child.0.stdout.take().ok_or("无法读取客户端")?;
    let reader = thread::spawn(move || {
        let mut raw = Vec::new();
        let _ = stdout.take(65536).read_to_end(&mut raw);
        raw
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.0.try_wait().map_err(|_| "无法检查客户端")?.is_some() {
            break;
        }
        if Instant::now() > deadline {
            return Err("客户端响应超时".into());
        }
        thread::sleep(Duration::from_millis(30));
    }
    serde_json::from_slice(&reader.join().map_err(|_| "无法读取客户端状态")?)
        .map_err(|_| "客户端状态格式不兼容".into())
}
fn json_lines(stdout: impl Read + Send + 'static) -> Receiver<Result<Value, String>> {
    let (tx, rx) = mpsc::sync_channel(128);
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = Vec::new();
            match reader
                .by_ref()
                .take((MAX_LINE + 1) as u64)
                .read_until(b'\n', &mut line)
            {
                Ok(0) => break,
                Ok(_) if line.len() > MAX_LINE => {
                    let _ = tx.send(Err("客户端消息过大，已断开连接".into()));
                    break;
                }
                Ok(_) => {
                    if !line.iter().all(u8::is_ascii_whitespace) {
                        match serde_json::from_slice(&line) {
                            Ok(value) => {
                                if tx.send(Ok(value)).is_err() {
                                    break;
                                }
                            }
                            Err(_) => {
                                let _ = tx.send(Err("客户端消息格式不兼容".into()));
                                break;
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}
struct Rpc {
    _child: Option<Managed>,
    input: Box<dyn Write + Send>,
    rx: Receiver<Result<Value, String>>,
    serial: u64,
    pub pending: VecDeque<Value>,
}
impl Rpc {
    fn start(path: &Path, cwd: &Path) -> Result<Self, String> {
        let root = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".codex"));
        if root
            .join("app-server-control/app-server-control.sock")
            .exists()
        {
            if let Ok(rpc) = Self::spawn(path, cwd, true) {
                return Ok(rpc);
            }
        }
        Self::spawn(path, cwd, false)
    }
    fn spawn(path: &Path, cwd: &Path, proxy: bool) -> Result<Self, String> {
        let mut command = Command::new(path);
        hidden(&mut command)
            .args(if proxy {
                ["app-server", "proxy"]
            } else {
                ["app-server", "--stdio"]
            })
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = Managed(
            command
                .spawn()
                .map_err(|_| "无法启动 Codex，请检查本机安装")?,
        );
        let input = child.0.stdin.take().ok_or("无法连接 Codex")?;
        let rx = json_lines(child.0.stdout.take().ok_or("无法读取 Codex")?);
        let mut rpc = Self {
            _child: Some(child),
            input: Box::new(input),
            rx,
            serial: 0,
            pending: VecDeque::new(),
        };
        rpc.call("initialize",json!({"clientInfo":{"name":"daozai_ai","title":"岛仔 AI 助手","version":env!("CARGO_PKG_VERSION")}}))?;
        rpc.write(&json!({"method":"initialized"}))?;
        Ok(rpc)
    }
    fn write(&mut self, value: &Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.input, value).map_err(|_| "Codex 连接已断开")?;
        self.input
            .write_all(b"\n")
            .and_then(|_| self.input.flush())
            .map_err(|_| "Codex 连接已断开".into())
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.serial += 1;
        let id = self.serial;
        self.write(&json!({"id":id,"method":method,"params":params}))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Ok(value))
                    if value.get("id").and_then(Value::as_u64) == Some(id)
                        && value.get("method").is_none() =>
                {
                    if value.get("error").is_some() {
                        return Err(format!(
                            "Codex 请求未完成（{}），请检查客户端登录与版本",
                            value["error"]["code"]
                        ));
                    }
                    return Ok(value["result"].clone());
                }
                Ok(Ok(value)) => self.queue(value)?,
                Ok(Err(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("Codex 客户端已退出".into())
                }
                Err(_) => {}
            }
        }
        Err("Codex 响应超时，请重新识别客户端".into())
    }
    fn queue(&mut self, value: Value) -> Result<(), String> {
        if value.get("method").is_some() && value.get("id").is_some() {
            let method = value["method"].as_str().unwrap_or_default();
            if method.ends_with("requestApproval") {
                self.write(&json!({"id":value["id"],"result":{"decision":"decline"}}))?;
            } else {
                self.write(&json!({"id":value["id"],"error":{"code":-32601,"message":"This client supports text chat only; use the original client for this action."}}))?;
            }
        }
        self.pending.push_back(value);
        Ok(())
    }
    fn drain(&mut self) -> Result<Vec<Value>, String> {
        for _ in 0..256 {
            match self.rx.try_recv() {
                Ok(Ok(value)) => self.queue(value)?,
                Ok(Err(error)) => return Err(error),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("Codex 客户端已退出，请重新识别".into())
                }
            }
        }
        Ok(self.pending.drain(..).collect())
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Store {
    codex: BTreeSet<String>,
    claude: BTreeSet<String>,
}
impl Store {
    fn load() -> Self {
        fs::read(crate::model::config_dir().join("ai-assistant-sessions.json"))
            .ok()
            .and_then(|r| serde_json::from_slice(&r).ok())
            .unwrap_or_default()
    }
    fn save(&self) -> Result<(), String> {
        let root = crate::model::config_dir();
        fs::create_dir_all(&root).map_err(|_| "无法保存对话索引")?;
        crate::model::atomic_write(
            &root.join("ai-assistant-sessions.json"),
            &serde_json::to_vec(self).map_err(|_| "无法保存对话索引")?,
        )
        .map_err(|_| "无法保存对话索引".into())
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id.len() < 256
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
pub(crate) fn codex_status(value: &Value) -> Status {
    match value["type"].as_str().unwrap_or_default() {
        "idle" => Status::Idle,
        "active" => {
            if value["activeFlags"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
            {
                Status::Waiting
            } else {
                Status::Running
            }
        }
        "systemError" => Status::Failed,
        _ => Status::History,
    }
}
fn task_from_codex(value: &Value, store: &Store) -> Option<Task> {
    let id = value["id"].as_str()?.to_owned();
    if !valid_id(&id) {
        return None;
    }
    let title = value["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| value["preview"].as_str())
        .unwrap_or("未命名对话");
    Some(Task {
        provider: Provider::Codex,
        owned: store.codex.contains(&id),
        id,
        title: title.chars().take(160).collect(),
        status: codex_status(&value["status"]),
        updated: value["updatedAt"].as_i64().unwrap_or_default(),
    })
}
pub(crate) fn messages_from_turns(turns: &[Value]) -> Vec<Message> {
    let mut out = Vec::new();
    for turn in turns {
        for item in turn["items"].as_array().into_iter().flatten() {
            let kind = item["type"].as_str().unwrap_or_default();
            let text = match kind {
                "agentMessage" => item["text"].as_str().unwrap_or_default().to_owned(),
                "userMessage" => item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => continue,
            };
            if !text.is_empty() {
                out.push(Message {
                    id: item["id"].as_str().unwrap_or_default().into(),
                    user: kind == "userMessage",
                    text: clip(&text, MAX_TEXT),
                });
            }
        }
    }
    if out.len() > 200 {
        out.drain(..out.len() - 200);
    }
    out
}
fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.into()
    } else {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n（内容过长，仅显示部分）", &text[..end])
    }
}
fn title(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("新对话")
        .chars()
        .take(80)
        .collect()
}

struct Active {
    provider: Provider,
    id: String,
    turn: String,
    started: Instant,
    cancelled: Option<Instant>,
    ready: bool,
    interrupt_sent: bool,
    last_interrupt: Option<Instant>,
}
struct ClaudeRun {
    child: Managed,
    rx: Receiver<Result<Value, String>>,
    last_text: String,
    got_result: bool,
}
struct Worker {
    commands: Receiver<Control>,
    events: Sender<Event>,
    ctx: eframe::egui::Context,
    codex: Option<Rpc>,
    claude_path: Option<PathBuf>,
    claude_ready: bool,
    claude_run: Option<ClaudeRun>,
    active: Option<Active>,
    store: Store,
    cwd: PathBuf,
}
impl Worker {
    fn new(commands: Receiver<Control>, events: Sender<Event>, ctx: eframe::egui::Context) -> Self {
        Self {
            commands,
            events,
            ctx,
            codex: None,
            claude_path: None,
            claude_ready: false,
            claude_run: None,
            active: None,
            store: Store::load(),
            cwd: crate::model::config_dir().join("ai-workspace"),
        }
    }
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        self.ctx.request_repaint();
    }
    fn run(mut self) {
        loop {
            match self.commands.recv_timeout(if self.active.is_some() {
                Duration::from_millis(40)
            } else {
                Duration::from_secs(1)
            }) {
                Ok(Control::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(command) => {
                    if let Err(error) = self.control(command) {
                        self.emit(Event::Error(error));
                    }
                }
                Err(_) => {}
            }
            if let Some(rpc) = self.codex.as_mut() {
                match rpc.drain() {
                    Ok(values) => {
                        for value in values {
                            self.codex_event(value);
                        }
                    }
                    Err(error) => {
                        self.codex = None;
                        self.fail_active(Provider::Codex, &error);
                        self.emit(Event::Error(error));
                    }
                }
            }
            self.interrupt_codex();
            self.claude_events();
            if self.active.as_ref().is_some_and(|a| {
                a.started.elapsed() > Duration::from_secs(600)
                    || a.cancelled
                        .is_some_and(|t| t.elapsed() > Duration::from_secs(10))
            }) {
                let provider = self.active.as_ref().unwrap().provider;
                match provider {
                    Provider::Codex => self.codex = None,
                    Provider::Claude => self.claude_run = None,
                }
                self.fail_active(provider, "客户端未及时结束，已断开本次对话连接，请重新识别");
            }
        }
    }
    fn control(&mut self, command: Control) -> Result<(), String> {
        match command {
            Control::Discover => {
                if self.active.is_some() {
                    return Err("请先停止当前回复，再重新识别".into());
                }
                fs::create_dir_all(&self.cwd).map_err(|_| "无法创建 AI 对话工作目录")?;
                self.codex = None;
                let mut agents = Vec::new();
                if let Some(path) = find_client(Provider::Codex) {
                    match Rpc::start(&path, &self.cwd) {
                        Ok(mut rpc) => {
                            match rpc.call("account/read", json!({"refreshToken":false})) {
                                Ok(value) => {
                                    let ready = !value["account"].is_null()
                                        || value["requiresOpenaiAuth"] == false;
                                    agents.push(Agent {
                                        provider: Provider::Codex,
                                        available: ready,
                                        note: if ready {
                                            "已复用本机登录"
                                        } else {
                                            "已安装，请先在 Codex 客户端登录"
                                        }
                                        .into(),
                                    });
                                    self.codex = Some(rpc);
                                }
                                Err(error) => agents.push(Agent {
                                    provider: Provider::Codex,
                                    available: false,
                                    note: error,
                                }),
                            }
                        }
                        Err(error) => agents.push(Agent {
                            provider: Provider::Codex,
                            available: false,
                            note: error,
                        }),
                    }
                }
                self.claude_path = find_client(Provider::Claude);
                self.claude_ready = false;
                if let Some(path) = &self.claude_path {
                    let status = output_json(path, &["auth", "status", "--json"]);
                    self.claude_ready = status.as_ref().is_ok_and(|s| s["loggedIn"] == true);
                    agents.push(Agent {
                        provider: Provider::Claude,
                        available: self.claude_ready,
                        note: if self.claude_ready {
                            "已复用本机登录".into()
                        } else {
                            status
                                .err()
                                .unwrap_or_else(|| "已安装，请先在 Claude Code 登录".into())
                        },
                    });
                }
                self.emit(Event::Agents(agents));
            }
            Control::List { provider, cursor } => self.list(provider, cursor)?,
            Control::Read { provider, id } => {
                if self.active.is_some() {
                    return Err("请先等待当前回复完成或点击停止".into());
                }
                if !valid_id(&id) {
                    return Err("对话编号无效".into());
                }
                let messages = match provider {
                    Provider::Codex => {
                        let rpc = self.codex.as_mut().ok_or("Codex 尚未连接，请重新识别")?;
                        match rpc.call("thread/turns/list",json!({"threadId":id,"limit":10,"sortDirection":"desc","itemsView":"full"})) {
                            Ok(value)=>{let mut turns=value["data"].as_array().cloned().unwrap_or_default();turns.reverse();messages_from_turns(&turns)},
                            Err(_)=>{let value=rpc.call("thread/read",json!({"threadId":id,"includeTurns":true}))?;messages_from_turns(value["thread"]["turns"].as_array().map(Vec::as_slice).unwrap_or(&[]))},
                        }
                    }
                    Provider::Claude => read_claude_history(&id)?,
                };
                self.emit(Event::History {
                    provider,
                    id,
                    messages,
                });
            }
            Control::Send { provider, id, text } => self.send(provider, id, text)?,
            Control::Cancel => {
                if let Some(active) = self.active.as_mut() {
                    active.cancelled = Some(Instant::now());
                    match active.provider {
                        Provider::Codex => {}
                        Provider::Claude => {
                            self.claude_run = None;
                            let active = self.active.take().unwrap();
                            self.emit(Event::Status {
                                id: active.id,
                                status: Status::Cancelled,
                                note: "已停止回复".into(),
                            });
                        }
                    }
                }
            }
            Control::Shutdown => {}
        }
        Ok(())
    }
    fn interrupt_codex(&mut self) {
        let Some(active) = self.active.as_mut().filter(|a| {
            a.provider == Provider::Codex
                && a.cancelled.is_some()
                && a.ready
                && !a.interrupt_sent
                && a.last_interrupt
                    .is_none_or(|t| t.elapsed() > Duration::from_millis(250))
        }) else {
            return;
        };
        active.last_interrupt = Some(Instant::now());
        let (id, turn) = (active.id.clone(), active.turn.clone());
        if let Some(rpc) = self.codex.as_mut() {
            // A very early interrupt can arrive before the engine has registered the turn.
            // Wait for turn/started and retry boundedly instead of reporting a false failure.
            if rpc
                .call("turn/interrupt", json!({"threadId":id,"turnId":turn}))
                .is_ok()
            {
                if let Some(active) = self.active.as_mut() {
                    active.interrupt_sent = true;
                }
            }
        }
    }
    fn list(&mut self, provider: Provider, cursor: Option<String>) -> Result<(), String> {
        match provider {
            Provider::Codex => {
                let value=self.codex.as_mut().ok_or("Codex 尚未连接")?.call("thread/list",json!({"limit":40,"cursor":cursor,"sortKey":"updated_at","sourceKinds":["cli","vscode","appServer","exec","unknown"],"modelProviders":[],"useStateDbOnly":true}))?;
                let mut tasks: Vec<_> = value["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| task_from_codex(v, &self.store))
                    .collect();
                if let Some(active) = &self.active {
                    for task in &mut tasks {
                        if task.id == active.id {
                            task.status = Status::Running;
                        }
                    }
                }
                self.emit(Event::Tasks {
                    provider,
                    tasks,
                    next: value["nextCursor"].as_str().map(str::to_owned),
                    append: cursor.is_some(),
                });
            }
            Provider::Claude => self.emit(Event::Tasks {
                provider,
                tasks: claude_tasks(&self.store),
                next: None,
                append: false,
            }),
        }
        Ok(())
    }
    fn send(&mut self, provider: Provider, id: Option<String>, text: String) -> Result<(), String> {
        if self.active.is_some() {
            return Err("当前仍在回复，请稍候或先停止".into());
        }
        if text.trim().is_empty() || text.len() > 32000 {
            return Err("请输入消息，单次不超过 32 KB".into());
        }
        if id.as_ref().is_some_and(|id| !valid_id(id)) {
            return Err("对话编号无效".into());
        }
        let owned = id.as_ref().is_some_and(|id| match provider {
            Provider::Codex => self.store.codex.contains(id),
            Provider::Claude => self.store.claude.contains(id),
        });
        let forked = id.is_some() && !owned;
        match provider {
            Provider::Codex => {
                let rpc = self.codex.as_mut().ok_or("Codex 尚未连接，请重新识别")?;
                let params = json!({"cwd":self.cwd,"sandbox":"read-only","approvalPolicy":"never","developerInstructions":"你是岛仔桌面助手。使用中文简洁交流。只有用户明确要求时才读取文件或调用工具；当前入口只读，不执行写入操作。"});
                let result = if let Some(id) = id {
                    let mut params = params;
                    params["threadId"] = json!(id);
                    params["excludeTurns"] = json!(true);
                    rpc.call(
                        if owned {
                            "thread/resume"
                        } else {
                            "thread/fork"
                        },
                        params,
                    )?
                } else {
                    rpc.call("thread/start", params)?
                };
                let id = result["thread"]["id"]
                    .as_str()
                    .filter(|s| valid_id(s))
                    .ok_or("Codex 没有返回有效对话编号")?
                    .to_owned();
                self.store.codex.insert(id.clone());
                // Persist ownership before starting inference; never continue a foreign thread in-place.
                self.store.save()?;
                self.emit(Event::Started {
                    provider,
                    id: id.clone(),
                    title: title(&text),
                    forked,
                });
                self.active = Some(Active {
                    provider,
                    id: id.clone(),
                    turn: String::new(),
                    started: Instant::now(),
                    cancelled: None,
                    ready: false,
                    interrupt_sent: false,
                    last_interrupt: None,
                });
                let result=self.codex.as_mut().unwrap().call("turn/start",json!({"threadId":id,"input":[{"type":"text","text":text}],"approvalPolicy":"never","sandboxPolicy":{"type":"readOnly","networkAccess":false}}));
                match result {
                    Ok(value) => {
                        if let Some(active) = self.active.as_mut() {
                            active.turn = value["turn"]["id"].as_str().unwrap_or_default().into();
                        }
                    }
                    Err(error) => self.fail_active(provider, &error),
                }
            }
            Provider::Claude => {
                if !self.claude_ready {
                    return Err("请先在本机 Claude Code 登录，再点击重新识别".into());
                }
                let path = self.claude_path.as_ref().ok_or("未发现 Claude Code")?;
                let mut command = Command::new(path);
                hidden(&mut command)
                    .current_dir(&self.cwd)
                    .args([
                        "--safe-mode",
                        "-p",
                        "--output-format",
                        "stream-json",
                        "--verbose",
                        "--include-partial-messages",
                        "--tools",
                        "",
                        "--strict-mcp-config",
                        "--mcp-config",
                        "{\"mcpServers\":{}}",
                        "--settings",
                        "{\"disableAllHooks\":true}",
                        "--permission-mode",
                        "plan",
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                if let Some(id) = &id {
                    command.arg("--resume").arg(id);
                    if forked {
                        command.arg("--fork-session");
                    }
                }
                let mut child = Managed(command.spawn().map_err(|_| "无法启动 Claude Code")?);
                let mut input = child.0.stdin.take().ok_or("无法发送消息")?;
                input
                    .write_all(text.as_bytes())
                    .and_then(|_| input.write_all(b"\n"))
                    .map_err(|_| "无法发送消息")?;
                drop(input);
                let rx = json_lines(child.0.stdout.take().ok_or("无法读取回复")?);
                let provisional = if forked || id.is_none() {
                    format!("pending-{}", chrono::Utc::now().timestamp_millis())
                } else {
                    id.unwrap()
                };
                self.active = Some(Active {
                    provider,
                    id: provisional.clone(),
                    turn: String::new(),
                    started: Instant::now(),
                    cancelled: None,
                    ready: false,
                    interrupt_sent: false,
                    last_interrupt: None,
                });
                self.claude_run = Some(ClaudeRun {
                    child,
                    rx,
                    last_text: String::new(),
                    got_result: false,
                });
                self.emit(Event::Started {
                    provider,
                    id: provisional,
                    title: title(&text),
                    forked,
                });
            }
        }
        Ok(())
    }
    fn fail_active(&mut self, provider: Provider, note: &str) {
        if self.active.as_ref().is_some_and(|a| a.provider == provider) {
            let active = self.active.take().unwrap();
            self.emit(Event::Status {
                id: active.id,
                status: Status::Failed,
                note: note.into(),
            });
        }
    }
    fn codex_event(&mut self, value: Value) {
        let method = value["method"].as_str().unwrap_or_default();
        let p = &value["params"];
        let id = p["threadId"].as_str().unwrap_or_default();
        if !self
            .active
            .as_ref()
            .is_some_and(|a| a.provider == Provider::Codex && a.id == id)
        {
            return;
        }
        let event_turn = p["turnId"].as_str().or_else(|| p["turn"]["id"].as_str());
        if self
            .active
            .as_ref()
            .is_some_and(|a| !a.turn.is_empty() && event_turn.is_some_and(|id| id != a.turn))
        {
            return;
        }
        if method == "item/agentMessage/delta" {
            self.emit(Event::Text {
                id: id.into(),
                item: p["itemId"].as_str().unwrap_or("reply").into(),
                text: p["delta"].as_str().unwrap_or_default().into(),
                replace: false,
            });
        } else if method == "item/completed" && p["item"]["type"] == "agentMessage" {
            self.emit(Event::Text {
                id: id.into(),
                item: p["item"]["id"].as_str().unwrap_or("reply").into(),
                text: clip(p["item"]["text"].as_str().unwrap_or_default(), MAX_TEXT),
                replace: true,
            });
        } else if method == "turn/started" {
            if let Some(active) = self.active.as_mut() {
                active.turn = p["turn"]["id"].as_str().unwrap_or_default().into();
                active.ready = true;
            }
        } else if method == "turn/completed" {
            let status = match p["turn"]["status"].as_str() {
                Some("completed") => Status::Completed,
                Some("interrupted") => Status::Cancelled,
                _ => Status::Failed,
            };
            self.active = None;
            self.emit(Event::Status {
                id: id.into(),
                status,
                note: if status == Status::Failed {
                    "回复失败，请检查原客户端的登录、额度和网络"
                } else {
                    status.label()
                }
                .into(),
            });
        } else if value.get("id").is_some() && method.contains("request") {
            self.emit(Event::Status {
                id: id.into(),
                status: Status::Waiting,
                note: "此操作需要完整客户端处理；岛仔入口仅提供文本对话".into(),
            });
        }
    }
    fn claude_events(&mut self) {
        let Some(run) = self.claude_run.as_mut() else {
            return;
        };
        let mut values = Vec::new();
        for _ in 0..256 {
            match run.rx.try_recv() {
                Ok(value) => values.push(value),
                Err(_) => break,
            }
        }
        for value in values {
            let value = match value {
                Ok(value) => value,
                Err(error) => {
                    self.claude_run = None;
                    self.fail_active(Provider::Claude, &error);
                    return;
                }
            };
            if !self
                .active
                .as_ref()
                .is_some_and(|a| a.provider == Provider::Claude)
            {
                continue;
            }
            if value["type"] == "system" && value["subtype"] == "init" {
                if let Some(id) = value["session_id"].as_str().filter(|id| valid_id(id)) {
                    self.store.claude.insert(id.into());
                    let _ = self.store.save();
                    self.active.as_mut().unwrap().id = id.into();
                    self.emit(Event::Started {
                        provider: Provider::Claude,
                        id: id.into(),
                        title: String::new(),
                        forked: false,
                    });
                }
            }
            let id = self.active.as_ref().unwrap().id.clone();
            if value["type"] == "stream_event" {
                if let Some(text) = value.pointer("/event/delta/text").and_then(Value::as_str) {
                    if let Some(run) = self.claude_run.as_mut() {
                        if run.last_text.len() < MAX_TEXT {
                            run.last_text.push_str(text);
                        }
                    }
                    self.emit(Event::Text {
                        id,
                        item: "claude-reply".into(),
                        text: text.into(),
                        replace: false,
                    });
                }
            } else if value["type"] == "result" {
                let failed = value["is_error"] == true;
                if let Some(text) = value["result"].as_str().filter(|s| !s.is_empty()) {
                    self.emit(Event::Text {
                        id: id.clone(),
                        item: "claude-reply".into(),
                        text: clip(text, MAX_TEXT),
                        replace: true,
                    });
                }
                if let Some(run) = self.claude_run.as_mut() {
                    run.got_result = true;
                }
                self.active = None;
                self.emit(Event::Status {
                    id,
                    status: if failed {
                        Status::Failed
                    } else {
                        Status::Completed
                    },
                    note: if failed {
                        "Claude 回复失败，请检查原客户端登录和额度"
                    } else {
                        "已完成"
                    }
                    .into(),
                });
            }
        }
        if let Some(run) = self.claude_run.as_mut() {
            if run.child.0.try_wait().ok().flatten().is_some() {
                let complete = run.got_result;
                self.claude_run = None;
                if !complete {
                    self.fail_active(Provider::Claude, "Claude 客户端已退出，未收到完整回复");
                }
            }
        }
    }
}

fn claude_root() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".claude"))
        .join("projects")
}
fn claude_files() -> Vec<PathBuf> {
    let mut projects: Vec<_> = fs::read_dir(claude_root())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .collect();
    projects.sort_by_key(|e| std::cmp::Reverse(e.metadata().and_then(|m| m.modified()).ok()));
    let mut files = Vec::new();
    for project in projects.into_iter().take(100) {
        for entry in fs::read_dir(project.path())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
        {
            if entry.path().extension().is_some_and(|e| e == "jsonl") {
                files.push(entry.path());
            }
        }
    }
    files.sort_by_key(|p| std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok()));
    files.truncate(200);
    files
}
fn claude_tasks(store: &Store) -> Vec<Task> {
    claude_files()
        .into_iter()
        .take(80)
        .filter_map(|path| {
            let id = path.file_stem()?.to_str()?.to_owned();
            if !valid_id(&id) {
                return None;
            }
            let mut name = "Claude 对话".to_owned();
            if let Ok(file) = fs::File::open(&path) {
                for line in BufReader::new(file.take(128 * 1024))
                    .lines()
                    .take(40)
                    .filter_map(Result::ok)
                {
                    if let Ok(value) = serde_json::from_str::<Value>(&line) {
                        if value["type"] == "user" {
                            let text = claude_text(&value["message"]["content"]);
                            if !text.is_empty() {
                                name = title(&text);
                                break;
                            }
                        }
                    }
                }
            }
            let updated = path
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or_default();
            Some(Task {
                provider: Provider::Claude,
                owned: store.claude.contains(&id),
                id,
                title: name,
                status: Status::History,
                updated,
            })
        })
        .collect()
}
fn read_claude_history(id: &str) -> Result<Vec<Message>, String> {
    let path = claude_files()
        .into_iter()
        .find(|p| p.file_stem().and_then(|s| s.to_str()) == Some(id))
        .ok_or("未找到本机 Claude 对话记录")?;
    let mut file = fs::File::open(path).map_err(|_| "无法读取 Claude 对话")?;
    let limit = 8 * 1024 * 1024;
    let tail = file.metadata().map_err(|_| "无法读取 Claude 对话")?.len() > limit;
    if tail {
        file.seek(SeekFrom::End(-(limit as i64)))
            .map_err(|_| "无法读取 Claude 最近对话")?;
    }
    let mut reader = BufReader::new(file.take(limit));
    if tail {
        let mut partial = Vec::new();
        reader
            .read_until(b'\n', &mut partial)
            .map_err(|_| "无法读取 Claude 最近对话")?;
    }
    let mut messages = VecDeque::new();
    for line in reader.lines().filter_map(Result::ok) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = value["type"].as_str().unwrap_or_default();
        if kind != "user" && kind != "assistant" {
            continue;
        }
        let content = &value["message"]["content"];
        let text = claude_text(content);
        if !text.is_empty() {
            messages.push_back(Message {
                id: value["uuid"].as_str().unwrap_or_default().into(),
                user: kind == "user",
                text: clip(&text, MAX_TEXT),
            });
            if messages.len() > 200 {
                messages.pop_front();
            }
        }
    }
    Ok(messages.into_iter().collect())
}
fn claude_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        text.into()
    } else {
        content
            .as_array()
            .into_iter()
            .flatten()
            .filter(|v| v["type"] == "text")
            .filter_map(|v| v["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_status_is_never_fabricated_as_completed() {
        assert_eq!(codex_status(&json!({"type":"notLoaded"})), Status::History);
        assert_eq!(
            codex_status(&json!({"type":"active","activeFlags":[]})),
            Status::Running
        );
        assert_eq!(
            codex_status(&json!({"type":"active","activeFlags":["waitingOnApproval"]})),
            Status::Waiting
        );
    }
    #[test]
    fn delayed_completion_cannot_finish_the_next_turn() {
        let (_tx, rx) = mpsc::channel();
        let (events, _out) = mpsc::channel();
        let mut worker = Worker::new(rx, events, eframe::egui::Context::default());
        worker.active = Some(Active {
            provider: Provider::Codex,
            id: "chat".into(),
            turn: "new-turn".into(),
            started: Instant::now(),
            cancelled: None,
            ready: false,
            interrupt_sent: false,
            last_interrupt: None,
        });
        worker.codex_event(json!({"method":"turn/completed","params":{"threadId":"chat","turn":{"id":"old-turn","status":"completed"}}}));
        assert!(worker.active.is_some());
        worker.codex_event(json!({"method":"turn/completed","params":{"threadId":"chat","turn":{"id":"new-turn","status":"completed"}}}));
        assert!(worker.active.is_none());
    }
    #[test]
    fn histories_only_show_visible_text_and_keep_unicode() {
        let turns = vec![
            json!({"items":[{"type":"userMessage","id":"u","content":[{"type":"text","text":"你好"}]},{"type":"commandExecution","command":"private"},{"type":"agentMessage","id":"a","text":"你好，岛仔"}]}),
        ];
        let messages = messages_from_turns(&turns);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].user);
        assert_eq!(messages[1].text, "你好，岛仔");
        assert!(clip("中文文本", 4).starts_with("中\n"));
    }
    #[test]
    fn incoming_json_lines_handle_fragmentation_and_unicode() {
        let rx = json_lines(std::io::Cursor::new(
            "{\"id\":1,\"result\":{\"text\":\"你好\"}}\n"
                .as_bytes()
                .to_vec(),
        ));
        assert_eq!(rx.recv().unwrap().unwrap()["result"]["text"], "你好");
    }
    #[test]
    fn session_ids_cannot_be_paths_or_cli_arguments() {
        for id in ["../auth.json", "--resume", "a/b", "", "x y"] {
            assert!(!valid_id(id));
        }
        assert!(valid_id("019aaa-bbbb-1234"));
    }
    #[test]
    fn rpc_keeps_stream_events_and_declines_unsupported_approvals() {
        use std::sync::{Arc, Mutex};
        struct Wire {
            raw: Vec<u8>,
            tx: Sender<Result<Value, String>>,
            writes: Arc<Mutex<Vec<Value>>>,
        }
        impl Write for Wire {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.raw.extend_from_slice(bytes);
                while let Some(end) = self.raw.iter().position(|b| *b == b'\n') {
                    let line: Vec<_> = self.raw.drain(..=end).collect();
                    let value: Value = serde_json::from_slice(&line).unwrap();
                    self.writes.lock().unwrap().push(value.clone());
                    if value["method"] == "thread/list" {
                        self.tx.send(Ok(json!({"method":"item/agentMessage/delta","params":{"threadId":"chat","itemId":"msg","delta":"你好"}}))).unwrap();
                        self.tx.send(Ok(json!({"id":99,"method":"item/commandExecution/requestApproval","params":{"threadId":"chat"}}))).unwrap();
                        self.tx
                            .send(Ok(json!({"id":value["id"],"result":{"data":[]}})))
                            .unwrap();
                    }
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = mpsc::channel();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let mut rpc = Rpc {
            _child: None,
            input: Box::new(Wire {
                raw: Vec::new(),
                tx,
                writes: writes.clone(),
            }),
            rx,
            serial: 0,
            pending: VecDeque::new(),
        };
        assert_eq!(
            rpc.call("thread/list", json!({})).unwrap()["data"],
            json!([])
        );
        assert_eq!(rpc.pending.front().unwrap()["params"]["delta"], "你好");
        assert!(writes
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["id"] == 99 && v["result"]["decision"] == "decline"));
    }
    #[test]
    fn oversized_protocol_lines_fail_without_unbounded_buffering() {
        let rx = json_lines(std::io::Cursor::new(vec![b'x'; MAX_LINE + 2]));
        assert!(rx.recv().unwrap().is_err());
    }
    #[test]
    fn claude_history_accepts_both_text_formats_without_tool_payloads() {
        assert_eq!(claude_text(&json!("你好")), "你好");
        assert_eq!(
            claude_text(
                &json!([{"type":"tool_result","content":"private"},{"type":"text","text":"回复"}])
            ),
            "回复"
        );
    }
    #[test]
    #[ignore = "手动验证本机 Codex 登录、历史读取与两轮真实对话；消耗少量账号额度"]
    fn live_codex_roundtrip() {
        assert!(
            std::env::var_os("DAOZAI_DATA_DIR").is_some(),
            "必须指定隔离数据目录"
        );
        let ctx = eframe::egui::Context::default();
        let service = Service::start(ctx).unwrap();
        service.send(Control::Discover).unwrap();
        let deadline = Instant::now() + Duration::from_secs(150);
        loop {
            assert!(Instant::now() < deadline, "识别超时");
            match service.rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Event::Agents(agents) => {
                    assert!(agents
                        .iter()
                        .any(|a| a.provider == Provider::Codex && a.available));
                    break;
                }
                Event::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
        service
            .send(Control::List {
                provider: Provider::Codex,
                cursor: None,
            })
            .unwrap();
        let first = loop {
            match service.rx.recv_timeout(Duration::from_secs(25)).unwrap() {
                Event::Tasks { tasks, .. } => break tasks.first().map(|t| t.id.clone()),
                Event::Error(error) => panic!("{error}"),
                _ => {}
            }
        };
        if let Some(id) = first {
            service
                .send(Control::Read {
                    provider: Provider::Codex,
                    id,
                })
                .unwrap();
            loop {
                match service.rx.recv_timeout(Duration::from_secs(25)).unwrap() {
                    Event::History { .. } => break,
                    Event::Error(error) => panic!("{error}"),
                    _ => {}
                }
            }
        }
        let mut current = None;
        for (prompt, expected) in [
            ("连通性测试，请只回复：连接正常。不要调用工具。", "连接正常"),
            (
                "继续连通性测试，请只回复：继续正常。不要调用工具。",
                "继续正常",
            ),
        ] {
            service
                .send(Control::Send {
                    provider: Provider::Codex,
                    id: current.clone(),
                    text: prompt.into(),
                })
                .unwrap();
            let mut answer = String::new();
            let mut deltas = 0;
            loop {
                assert!(Instant::now() < deadline, "真实对话超时");
                match service.rx.recv_timeout(Duration::from_secs(30)).unwrap() {
                    Event::Started { id, .. } => {
                        if let Some(old) = &current {
                            assert_eq!(&id, old);
                        }
                        current = Some(id);
                    }
                    Event::Text { text, replace, .. } => {
                        if replace {
                            answer = text;
                        } else {
                            answer.push_str(&text);
                            deltas += 1;
                        }
                    }
                    Event::Status { status, .. } => {
                        assert_eq!(status, Status::Completed);
                        break;
                    }
                    Event::Error(error) => panic!("{error}"),
                    _ => {}
                }
            }
            assert!(answer.contains(expected));
            assert!(deltas > 0, "未收到流式事件");
        }
        service
            .send(Control::Send {
                provider: Provider::Codex,
                id: current.clone(),
                text: "连通性停止测试。请逐行列出从 1 到 200 的数字，不要使用工具。".into(),
            })
            .unwrap();
        let mut requested = false;
        loop {
            assert!(Instant::now() < deadline, "停止回复超时");
            match service.rx.recv_timeout(Duration::from_secs(30)).unwrap() {
                Event::Started { .. } => {
                    service.send(Control::Cancel).unwrap();
                    requested = true;
                }
                Event::Status { status, .. } => {
                    assert!(requested);
                    assert_eq!(status, Status::Cancelled);
                    break;
                }
                Event::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
        drop(service);
        let cwd = crate::model::config_dir().join("ai-workspace");
        let mut rpc = Rpc::start(&find_client(Provider::Codex).unwrap(), &cwd).unwrap();
        rpc.call("thread/archive", json!({"threadId":current.unwrap()}))
            .unwrap();
        println!("本机登录、历史读取、流式回复、两轮持续对话及停止回复验证通过；测试会话已归档");
    }
}
