//! Observe existing local clients without resuming or changing their conversations.
use crate::ai_assistant::{Provider, Status, Task};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

#[path="ai_native_read.rs"]
mod native_read;

const TAIL: u64 = 2 * 1024 * 1024;
const MAX_RECORD: usize = 256 * 1024;
pub(crate) struct Update {
    pub tasks: Vec<Task>,
    pub changed: Vec<(Provider, String, Status)>,
    pub note: String,
    pub read: Vec<(Provider,String,i64)>,
}
pub(crate) struct Monitor {
    pub rx: Receiver<Update>,
    tx: Sender<bool>,
}
impl Monitor {
    pub fn start(ctx: eframe::egui::Context) -> Result<Self, String> {
        let (tx, commands) = mpsc::channel();
        let (events, rx) = mpsc::channel();
        thread::Builder::new()
            .name("daozai-ai-monitor".into())
            .spawn(move || {
                let root = crate::model::config_dir().join("ai-monitor");
                let homes = homes();
                let mut enabled = !root.join("disabled").exists();
                let mut note = configure(&homes, &root, enabled);
                let mut watcher = Watcher::default();
                loop {
                    let started = Instant::now();
                    let mut changed_config = false;
                    while let Ok(value) = commands.try_recv() {
                        enabled = value;
                        note = configure(&homes, &root, enabled);
                        changed_config = true;
                    }
                    let update = watcher.scan(&homes, &root, enabled, &note);
                    if !update.changed.is_empty()
                        || watcher.first
                        || changed_config
                        || watcher.dirty
                    {
                        watcher.first = false;
                        watcher.dirty = false;
                        if events.send(update).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                    match commands
                        .recv_timeout(Duration::from_secs(2).saturating_sub(started.elapsed()))
                    {
                        Ok(value) => {
                            enabled = value;
                            note = configure(&homes, &root, enabled);
                            watcher.dirty = true;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(_) => {}
                    }
                }
            })
            .map_err(|_| "无法启动外部任务监听".to_string())?;
        Ok(Self { rx, tx })
    }
    pub fn configure(&self, enabled: bool) {
        let _ = self.tx.send(enabled);
    }
}

fn homes() -> [(Provider, PathBuf); 2] {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default();
    [
        (
            Provider::Codex,
            std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
        ),
        (
            Provider::Claude,
            std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
        ),
    ]
}

// A stable standalone script avoids loading the GUI executable for each event.
// Only status metadata is persisted; prompts, responses and tool arguments are discarded.
const HOOK_SCRIPT: &str = r#"param([string]$Provider, [string]$Event)
$ErrorActionPreference = 'Stop'
try {
  $raw = [Console]::In.ReadToEnd()
  if ($raw.Length -gt 8388608) { exit 0 }
  $v = $raw | ConvertFrom-Json
  $id = $v.session_id
  if (-not $id) { $id = $v.thread_id }
  if (-not $id -or $id -notmatch '^[a-zA-Z0-9_-]{1,160}$') { exit 0 }
  $out = @{ provider=$Provider; event=$Event; id=$id; at=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds(); agent_id=$v.agent_id; notification_type=$v.notification_type; tool_name=$v.tool_name; stop_reason=$v.stop_reason; turn_id=$v.turn_id }
  $queue = Join-Path $PSScriptRoot 'events'
  [IO.Directory]::CreateDirectory($queue) | Out-Null
  if ([IO.Directory]::GetFiles($queue, '*.json').Length -ge 1024) { exit 0 }
  $name = ([Guid]::NewGuid().ToString())
  $tmp = Join-Path $queue ($name + '.tmp')
  $dest = Join-Path $queue ($name + '.json')
  [IO.File]::WriteAllText($tmp, ($out | ConvertTo-Json -Compress), (New-Object Text.UTF8Encoding($false)))
  [IO.File]::Move($tmp, $dest)
} catch {}
exit 0
"#;

fn hook_events(provider: Provider) -> Vec<&'static str> {
    let mut events = vec![
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "PermissionRequest",
        "Stop",
        "SessionEnd",
        "SubagentStart",
        "SubagentStop",
    ];
    if provider == Provider::Claude {
        events.extend(["Notification", "StopFailure"]);
    } else {
        events.push("Interrupt");
    }
    events
}

fn merge_hooks(
    config: &mut Value,
    provider: Provider,
    script: &Path,
    enabled: bool,
) -> Result<(), String> {
    let obj = config
        .as_object_mut()
        .ok_or("事件配置不是 JSON 对象，未修改原文件")?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("原 hooks 格式无效，未修改原文件")?;
    for list in hooks.values_mut() {
        let entries = list
            .as_array_mut()
            .ok_or("原 hooks 列表格式无效，未修改原文件")?;
        // Remove only our own handlers, preserving other handlers in the same entry.
        for entry in entries.iter_mut() {
            if let Some(handlers) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                handlers.retain(|h| {
                    !h["command"]
                        .as_str()
                        .is_some_and(|s| s.contains("daozai-ai-monitor-hook.ps1"))
                });
            }
        }
        entries.retain(|e| !e["hooks"].as_array().is_some_and(Vec::is_empty));
    }
    if enabled {
        let path = script.to_string_lossy().replace('\\', "/");
        if path.contains(['"', '\n', '\r', '$', '`']) {
            return Err("事件脚本目录包含不支持的字符".into());
        }
        for event in hook_events(provider) {
            let list = hooks
                .entry(event)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or("事件列表格式无效")?;
            list.push(json!({"hooks":[{"type":"command","command":format!("powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{path}\" {} {event}", if provider == Provider::Codex {"codex"} else {"claude"}),"timeout":3}]}));
        }
    }
    Ok(())
}

fn install(path: &Path, provider: Provider, script: &Path, enabled: bool) -> Result<(), String> {
    let before = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("无法读取客户端事件配置".into()),
    };
    if before.is_none() && !enabled {
        return Ok(());
    }
    let mut config = match &before {
        Some(bytes) => {
            serde_json::from_slice(bytes).map_err(|_| "客户端事件配置无法解析，保留原文件")?
        }
        None => json!({}),
    };
    let original = config.clone();
    merge_hooks(&mut config, provider, script, enabled)?;
    if original == config {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| "无法创建事件配置目录")?;
    }
    if let Some(bytes) = &before {
        let backup = path.with_extension("daozai-backup.json");
        if !backup.exists() {
            fs::write(backup, bytes).map_err(|_| "无法备份原事件配置")?;
        }
    }
    // Refuse to overwrite a concurrent edit made while preparing our merge.
    if fs::read(path).ok() != before {
        return Err("事件配置正在被其他程序修改，请重试".into());
    }
    crate::model::atomic_write(
        path,
        &serde_json::to_vec_pretty(&config).map_err(|_| "无法保存事件配置")?,
    )
    .map_err(|_| "无法保存事件配置".into())
}

fn configure(homes: &[(Provider, PathBuf)], root: &Path, enabled: bool) -> String {
    let result = (|| -> Result<usize, String> {
        fs::create_dir_all(root).map_err(|_| "无法创建监听目录")?;
        if enabled {
            let _ = fs::remove_file(root.join("disabled"));
        } else {
            fs::write(root.join("disabled"), b"disabled").map_err(|_| "无法保存监听设置")?;
        }
        let script = root.join("daozai-ai-monitor-hook.ps1");
        if fs::read(&script).ok().as_deref() != Some(HOOK_SCRIPT.as_bytes()) {
            crate::model::atomic_write(&script, HOOK_SCRIPT.as_bytes())
                .map_err(|_| "无法保存事件脚本")?;
        }
        let mut installed = 0;
        for (provider, home) in homes {
            if !home.is_dir() {
                continue;
            }
            let path = home.join(if *provider == Provider::Codex {
                "hooks.json"
            } else {
                "settings.json"
            });
            install(&path, *provider, &script, enabled)?;
            installed += 1;
        }
        Ok(installed)
    })();
    match result {
        Err(e) => format!("外部任务接入：{e}。本地记录监听仍可用"),
        Ok(_) if !enabled => "实时事件已关闭；仍监听本地记录".into(),
        Ok(0) => "监听已启动，尚未发现本机 Codex / Claude 配置目录".into(),
        Ok(_) => "事件已配置；请重启客户端。Codex 需在设置 → Hooks 或 /hooks 中信任岛仔脚本".into(),
    }
}

#[derive(Default)]
struct Cursor {
    offset: u64,
    pending: Vec<u8>,
}
impl Cursor {
    fn read(&mut self, path: &Path) -> Vec<Value> {
        let Ok(mut file) = fs::File::open(path) else {
            return vec![];
        };
        let Ok(meta) = file.metadata() else {
            return vec![];
        };
        let len = meta.len();
        if len < self.offset {
            self.offset = 0;
            self.pending.clear();
        }
        let skip = len.saturating_sub(self.offset) > TAIL;
        if skip {
            self.offset = len - TAIL;
            self.pending.clear();
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return vec![];
        }
        let mut data = Vec::new();
        if file.take(TAIL).read_to_end(&mut data).is_err() {
            return vec![];
        }
        self.offset += data.len() as u64;
        self.pending.extend(data);
        let mut values = Vec::new();
        let mut consumed = 0;
        for (index, byte) in self.pending.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            if !(skip && consumed == 0) && index - consumed <= MAX_RECORD {
                if let Ok(v) = serde_json::from_slice(&self.pending[consumed..index]) {
                    values.push(v);
                }
            }
            consumed = index + 1;
        }
        self.pending.drain(..consumed);
        if self.pending.len() > MAX_RECORD {
            self.pending.clear();
        }
        values
    }
}

struct Entry {
    task: Task,
    children: HashSet<String>,
    stopping: bool,
    event_at: i64,
    hook_at: i64,
    turn_id: Option<String>,
}
struct Watcher {
    files: HashMap<PathBuf, Cursor>,
    entries: HashMap<String, Entry>,
    first: bool,
    dirty: bool,
    last_discovery: Option<Instant>,
    paths: Vec<(Provider, PathBuf)>,
    last_read: Vec<(Provider,String,i64)>,
}
impl Default for Watcher {
    fn default() -> Self {
        Self {
            files: HashMap::new(),
            entries: HashMap::new(),
            first: true,
            dirty: true,
            last_discovery: None,
            paths: vec![],
            last_read: vec![],
        }
    }
}
fn key(provider: Provider, id: &str) -> String {
    format!("{}:{id}", provider.name())
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn timestamp(v: &Value) -> i64 {
    v["timestamp"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
        .or_else(|| v["at"].as_i64())
        .unwrap_or(0)
}
fn walk(root: &Path, depth: usize, files: &mut Vec<PathBuf>, budget: &mut usize) {
    if depth == 0 || *budget == 0 {
        return;
    }
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.filter_map(Result::ok) {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                walk(&entry.path(), depth - 1, files, budget);
            } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "jsonl") {
                files.push(entry.path());
            }
        }
    }
}

// Native names are independent of rollout activity: /rename does not need to
// append a task event. Reading this local index never resumes a conversation.
fn codex_titles(home: &Path) -> HashMap<String, String> {
    let mut titles = HashMap::new();
    let Ok(mut file) = fs::File::open(home.join("session_index.jsonl")) else {
        return titles;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(8 * 1024 * 1024)))
        .is_err()
    {
        return titles;
    }
    let mut bytes = Vec::new();
    if file.take(8 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
        return titles;
    }
    for line in bytes.split(|b| *b == b'\n') {
        if line.len() > MAX_RECORD {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if let (Some(id), Some(name)) = (value["id"].as_str(), value["thread_name"].as_str()) {
            let name = name.trim();
            if valid_id(id) && !name.is_empty() {
                titles.insert(id.into(), name.chars().take(240).collect());
            }
        }
    }
    titles
}

fn initial_title(provider: Provider, path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(256 * 1024).read_to_end(&mut bytes).ok()?;
    bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find_map(|v| {
            log_event(provider, &v)
                .and_then(|(_, title)| title)
                .filter(|s| !s.trim().is_empty())
        })
}
impl Watcher {
    fn scan(
        &mut self,
        homes: &[(Provider, PathBuf)],
        root: &Path,
        enabled: bool,
        note: &str,
    ) -> Update {
        let now = chrono::Utc::now().timestamp_millis();
        let mut changed = Vec::new();
        if self
            .last_discovery
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(10))
        {
            self.paths.clear();
            for (provider, home) in homes {
                let mut paths = Vec::new();
                walk(
                    &home.join(if *provider == Provider::Codex {
                        "sessions"
                    } else {
                        "projects"
                    }),
                    6,
                    &mut paths,
                    &mut 12000,
                );
                paths.sort_by_key(|p| {
                    std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok())
                });
                self.paths
                    .extend(paths.into_iter().take(100).map(|p| (*provider, p)));
            }
            self.last_discovery = Some(Instant::now());
            self.files
                .retain(|p, _| self.paths.iter().any(|(_, active)| p == active));
        }
        for (provider, path) in self.paths.clone() {
            // Claude subagent transcripts carry the parent's sessionId. Give
            // them separate rows so their final answer cannot finish the parent.
            let subagent = provider == Provider::Claude
                && path.components().any(|c| c.as_os_str() == "subagents");
            let baseline = !self.files.contains_key(&path);
            let values = self.files.entry(path.clone()).or_default().read(&path);
            let fallback = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let mut id = if provider == Provider::Claude {
                fallback.to_owned()
            } else {
                fallback
                    .get(fallback.len().saturating_sub(36)..)
                    .unwrap_or_default()
                    .to_owned()
            };
            for value in values {
                if value["type"] == "session_meta" {
                    if let Some(s) = value["payload"]["id"].as_str() {
                        id = s.into();
                    }
                }
                if let Some(s) = value["sessionId"].as_str().filter(|_| !subagent) {
                    id = s.into();
                }
                let at = timestamp(&value);
                let Some((event, title)) = log_event(provider, &value) else {
                    continue;
                };
                self.apply(
                    provider,
                    &id,
                    &event,
                    &value,
                    at,
                    false,
                    title,
                    (!baseline || !self.first) && at > now - 120_000,
                    &mut changed,
                );
            }
            if baseline {
                if let Some(name) = initial_title(provider, &path) {
                    if let Some(entry) = self.entries.get_mut(&key(provider, &id)) {
                        entry.task.title = name;
                    }
                }
            }
        }
        let mut queued: Vec<_> = fs::read_dir(root.join("events"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .take(4000)
            .collect();
        queued.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
        for file in queued {
            if file.path().extension().is_none_or(|e| e != "json") {
                continue;
            }
            if enabled && file.metadata().is_ok_and(|m| m.len() <= 8192) {
                if let Ok(bytes) = fs::read(file.path()) {
                    if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                        let provider = if v["provider"] == "codex" {
                            Provider::Codex
                        } else if v["provider"] == "claude" {
                            Provider::Claude
                        } else {
                            let _ = fs::remove_file(file.path());
                            continue;
                        };
                        let at = timestamp(&v);
                        if at > now - 120_000 && at <= now + 5000 {
                            self.apply(
                                provider,
                                v["id"].as_str().unwrap_or_default(),
                                v["event"].as_str().unwrap_or_default(),
                                &v,
                                at,
                                true,
                                None,
                                !self.first,
                                &mut changed,
                            );
                        }
                    }
                }
            }
            let _ = fs::remove_file(file.path());
        }
        for (provider, home) in homes {
            if *provider != Provider::Codex {
                continue;
            }
            for (id, name) in codex_titles(home) {
                if let Some(entry) = self.entries.get_mut(&key(*provider, &id)) {
                    if entry.task.title != name {
                        entry.task.title = name;
                        self.dirty = true;
                    }
                }
            }
        }
        for entry in self.entries.values_mut() {
            if entry.task.status == Status::Running && now - entry.event_at > 10 * 60_000 {
                entry.task.status = Status::History;
                self.dirty = true;
                changed.push((entry.task.provider, entry.task.id.clone(), Status::History));
            }
        }
        self.entries.retain(|_, e| now - e.event_at < 7 * 86400_000);
        // Coalesce intermediate transitions within one scan. A completed record
        // followed by a new prompt must not produce an obsolete completion alert.
        changed.retain(|(p, id, status)| {
            self.entries
                .get(&key(*p, id))
                .is_some_and(|e| e.task.status == *status)
        });
        let mut tasks: Vec<_> = self.entries.values().map(|e| e.task.clone()).collect();
        tasks.sort_by_key(|t| std::cmp::Reverse(t.updated));
        tasks.truncate(200);
        let active: HashSet<_> = tasks.iter().map(|t| key(t.provider, &t.id)).collect();
        self.entries.retain(|id, _| active.contains(id));
        let mut read=Vec::new();
        for (provider,home) in homes {
            if *provider!=Provider::Codex {continue;}
            let receipts=native_read::receipts(home);
            for entry in self.entries.values() {
                if entry.task.provider==Provider::Codex && entry.task.status==Status::Completed
                    && entry.turn_id.as_ref().is_some_and(|turn|receipts.get(&entry.task.id)==Some(turn)) {
                    read.push((Provider::Codex,entry.task.id.clone(),entry.task.updated));
                }
            }
        }
        read.sort_by(|a,b|a.1.cmp(&b.1));
        if read!=self.last_read {self.dirty=true;self.last_read=read.clone();}
        Update {
            read,
            tasks,
            changed,
            note: note.into(),
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn apply(
        &mut self,
        provider: Provider,
        id: &str,
        event: &str,
        v: &Value,
        at: i64,
        hook: bool,
        title: Option<String>,
        live: bool,
        changed: &mut Vec<(Provider, String, Status)>,
    ) {
        if !valid_id(id) || at <= 0 {
            return;
        }
        let entry = self
            .entries
            .entry(key(provider, id))
            .or_insert_with(|| Entry {
                task: Task {
                    provider,
                    id: id.into(),
                    title: "未命名任务".into(),
                    status: Status::History,
                    updated: at / 1000,
                    owned: false,
                },
                children: HashSet::new(),
                stopping: false,
                event_at: 0,
                hook_at: 0,
                turn_id: None,
            });
        if at < entry.event_at || (!hook && at <= entry.hook_at) {
            return;
        }
        let old = entry.task.status;
        if let Some(title) = title {
            if !title.is_empty() && entry.task.title == "未命名任务" {
                entry.task.title = title;
            }
        }
        let status = match event {
            "SessionStart" => Status::Idle,
            "UserPromptSubmit" | "task_started" | "PreToolUse" | "PostToolUse" | "working" => {
                if matches!(event, "PreToolUse" | "PostToolUse")
                    && v["agent_id"]
                        .as_str()
                        .is_some_and(|id| entry.children.contains(id))
                {
                    return;
                }
                if event == "PreToolUse"
                    && v["tool_name"]
                        .as_str()
                        .is_some_and(|s| s == "AskUserQuestion" || s == "request_user_input")
                {
                    Status::Waiting
                } else {
                    entry.stopping = false;
                    Status::Running
                }
            }
            "PermissionRequest" | "waiting" => Status::Waiting,
            "Notification"
                if matches!(
                    v["notification_type"].as_str(),
                    Some("permission_prompt" | "elicitation_dialog")
                ) =>
            {
                Status::Waiting
            }
            "SubagentStart" => {
                if let Some(id) = v["agent_id"].as_str() {
                    entry.children.insert(id.into());
                }
                Status::Running
            }
            "SubagentStop" => {
                if let Some(id) = v["agent_id"].as_str() {
                    entry.children.remove(id);
                }
                if entry.children.is_empty() && entry.stopping {
                    Status::Completed
                } else {
                    old
                }
            }
            "Stop" | "task_complete" => {
                entry.stopping = true;
                if v["stop_reason"] == "error" {
                    Status::Failed
                } else if entry.children.is_empty() {
                    Status::Completed
                } else {
                    Status::Running
                }
            }
            "StopFailure" | "failed" => Status::Failed,
            "Interrupt" | "turn_aborted" => {
                entry.stopping = false;
                Status::Cancelled
            }
            "SessionEnd" => {
                entry.children.clear();
                entry.stopping = false;
                if matches!(old, Status::Failed | Status::Cancelled | Status::Completed) {
                    old
                } else {
                    Status::Idle
                }
            }
            _ => return,
        };
        if provider==Provider::Codex {
            if let Some(turn)=v["payload"]["turn_id"].as_str().or_else(||v["turn_id"].as_str()) {
                entry.turn_id=Some(turn.to_owned());
            } else if matches!(event,"task_started"|"UserPromptSubmit") {
                entry.turn_id=None;
            }
        }
        entry.event_at = at;
        if hook {
            entry.hook_at = at;
        }
        entry.task.status = status;
        entry.task.updated = at / 1000;
        self.dirty = true;
        if live && status != old {
            changed.push((provider, id.into(), status));
        }
    }
}

fn short(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(60).collect()
}
fn log_event(provider: Provider, v: &Value) -> Option<(String, Option<String>)> {
    if provider == Provider::Codex {
        if v["type"] != "event_msg" {
            return None;
        }
        let p = &v["payload"];
        return match p["type"].as_str()? {
            "task_started" => Some(("task_started".into(), None)),
            "task_complete" => Some(("task_complete".into(), None)),
            "turn_aborted" => Some(("turn_aborted".into(), None)),
            "user_message" => Some(("working".into(), p["message"].as_str().map(short))),
            _ => None,
        };
    }
    match v["type"].as_str()? {
        "user" if v["isApiErrorMessage"] == true => Some(("failed".into(), None)),
        "user" if v["isMeta"] != true => {
            let content = &v["message"]["content"];
            let title = content.as_str().map(short).or_else(|| {
                content
                    .as_array()?
                    .iter()
                    .find_map(|b| b["text"].as_str().map(short))
            });
            Some(("working".into(), title))
        }
        "assistant" => {
            let message = &v["message"];
            let waiting = message["content"].as_array().is_some_and(|a| {
                a.iter()
                    .any(|b| b["type"] == "tool_use" && b["name"] == "AskUserQuestion")
            });
            let event = if waiting {
                "waiting"
            } else if matches!(
                message["stop_reason"].as_str(),
                Some("end_turn" | "stop_sequence")
            ) {
                "task_complete"
            } else {
                "working"
            };
            Some((event.into(), None))
        }
        "system" if v["subtype"] == "turn_duration" => Some(("task_complete".into(), None)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn apply(w: &mut Watcher, event: &str, v: Value, time: i64) -> Vec<(Provider, String, Status)> {
        let mut changes = vec![];
        w.apply(
            Provider::Codex,
            "test-id",
            event,
            &v,
            time,
            true,
            None,
            true,
            &mut changes,
        );
        changes
    }
    #[test]
    fn monitor_children_prevent_early_completion_and_stale_events() {
        let mut w = Watcher::default();
        apply(&mut w, "UserPromptSubmit", json!({}), 1000);
        apply(&mut w, "SubagentStart", json!({"agent_id":"child"}), 2000);
        apply(&mut w, "Stop", json!({}), 3000);
        assert_eq!(w.entries["Codex:test-id"].task.status, Status::Running);
        apply(&mut w, "SubagentStop", json!({"agent_id":"child"}), 4000);
        assert_eq!(w.entries["Codex:test-id"].task.status, Status::Completed);
        assert!(apply(&mut w, "UserPromptSubmit", json!({}), 1000).is_empty());
        apply(&mut w, "PermissionRequest", json!({}), 5000);
        assert_eq!(w.entries["Codex:test-id"].task.status, Status::Waiting);
        apply(&mut w, "PostToolUse", json!({}), 6000);
        apply(&mut w, "StopFailure", json!({}), 7000);
        apply(&mut w, "SessionEnd", json!({}), 8000);
        assert_eq!(w.entries["Codex:test-id"].task.status, Status::Failed);
    }
    #[test]
    fn monitor_merge_preserves_other_hooks_and_is_idempotent() {
        let mut config = json!({"other":true,"hooks":{"Stop":[{"hooks":[{"type":"command","command":"other-tool"}]}]}});
        let script = Path::new("C:/test path/daozai-ai-monitor-hook.ps1");
        merge_hooks(&mut config, Provider::Codex, script, true).unwrap();
        let first = config.clone();
        merge_hooks(&mut config, Provider::Codex, script, true).unwrap();
        assert_eq!(config, first);
        merge_hooks(&mut config, Provider::Codex, script, false).unwrap();
        assert_eq!(
            config["hooks"]["Stop"][0]["hooks"][0]["command"],
            "other-tool"
        );
        assert_eq!(config["other"], true);
    }
    #[test]
    fn monitor_partial_lines_and_truncation() {
        let root = std::env::temp_dir().join(format!("daozai-monitor-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("partial.jsonl");
        fs::write(&file, b"{\"a\":").unwrap();
        let mut cursor = Cursor::default();
        assert!(cursor.read(&file).is_empty());
        fs::write(&file, b"{\"a\":1}\n").unwrap();
        assert_eq!(cursor.read(&file)[0]["a"], 1);
        fs::write(&file, b"{}\n").unwrap();
        assert_eq!(cursor.read(&file).len(), 1);
        fs::remove_file(file).unwrap();
        let _ = fs::remove_dir(root);
    }
    #[test]
    fn monitor_baseline_does_not_notify() {
        let mut w = Watcher::default();
        let mut changes = vec![];
        w.apply(
            Provider::Codex,
            "history",
            "task_complete",
            &json!({}),
            1000,
            false,
            None,
            false,
            &mut changes,
        );
        assert!(changes.is_empty());
        assert_eq!(log_event(Provider::Claude, &json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"AskUserQuestion"}]}})).unwrap().0, "waiting");
    }
    #[test]
    fn native_names_override_prompts_and_renames_do_not_notify() {
        let root = std::env::temp_dir().join(format!("daozai-native-title-{}", std::process::id()));
        let home = root.join("codex");
        let sessions = home.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let id = "00000000-0000-0000-0000-000000000002";
        let path = sessions.join(format!("rollout-{id}.jsonl"));
        let now = chrono::Utc::now().to_rfc3339();
        let records = [
            json!({"timestamp":now,"type":"event_msg","payload":{"type":"user_message","message":"首次提问摘要"}}),
            json!({"timestamp":now,"type":"event_msg","payload":{"type":"user_message","message":"后续追问不应替换名称"}}),
            json!({"timestamp":now,"type":"event_msg","payload":{"type":"task_complete"}}),
        ];
        fs::write(
            &path,
            records.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let index = home.join("session_index.jsonl");
        fs::write(
            &index,
            format!("{}\n", json!({"id":id,"thread_name":"客户端任务名称"})),
        )
        .unwrap();
        let homes = [(Provider::Codex, home.clone())];
        let mut w = Watcher::default();
        let first = w.scan(&homes, &root, false, "");
        assert_eq!(first.tasks[0].title, "客户端任务名称");
        assert!(first.changed.is_empty());
        w.first = false;
        w.dirty = false;
        fs::write(
            &index,
            format!(
                "{}\n{}\n",
                json!({"id":id,"thread_name":"客户端任务名称"}),
                json!({"id":id,"thread_name":"用户重命名后的名称"})
            ),
        )
        .unwrap();
        let renamed = w.scan(&homes, &root, false, "");
        assert_eq!(renamed.tasks[0].title, "用户重命名后的名称");
        assert!(renamed.changed.is_empty());
        assert!(w.dirty);
        assert_eq!(
            initial_title(Provider::Codex, &path).as_deref(),
            Some("首次提问摘要")
        );
        fs::remove_file(index).unwrap();
        fs::remove_file(path).unwrap();
        let _ = fs::remove_dir(sessions);
        let _ = fs::remove_dir(home);
        let _ = fs::remove_dir(root);
    }
    #[test]
    fn monitor_scan_ignores_history_but_not_new_completion() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("daozai-monitor-scan-{}", std::process::id()));
        let sessions = root.join("codex/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("rollout-00000000-0000-0000-0000-000000000001.jsonl");
        let now = chrono::Utc::now();
        fs::write(&path, format!("{}\n", json!({"timestamp":now.to_rfc3339(),"type":"event_msg","payload":{"type":"task_started"}}))).unwrap();
        let homes = [(Provider::Codex, root.join("codex"))];
        let mut w = Watcher::default();
        let first = w.scan(&homes, &root, false, "test");
        assert!(first.changed.is_empty());
        assert_eq!(first.tasks[0].status, Status::Running);
        w.first = false;
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", json!({"timestamp":(now + chrono::Duration::milliseconds(10)).to_rfc3339(),"type":"event_msg","payload":{"type":"task_complete","turn_id":"completed-turn"}})).unwrap();
        drop(file);
        let update = w.scan(&homes, &root, false, "test");
        assert_eq!(update.changed.len(), 1);
        assert_eq!(update.changed[0].2, Status::Completed);
        assert_eq!(w.entries.values().next().unwrap().turn_id.as_deref(),Some("completed-turn"));
        assert!(w.scan(&homes, &root, false, "test").changed.is_empty());
        fs::remove_file(path).unwrap();
        let _ = fs::remove_dir(sessions);
        let _ = fs::remove_dir(root.join("codex"));
        let _ = fs::remove_dir(root);
    }
    #[test]
    #[ignore = "手动只读检查本机记录发现，不输出会话内容，不修改客户端配置"]
    fn monitor_local_discovery_smoke() {
        let mut w = Watcher::default();
        let update = w.scan(
            &homes(),
            &std::env::temp_dir().join("daozai-no-hook-queue"),
            false,
            "只读检查",
        );
        assert!(update.changed.is_empty());
        let native = homes()
            .into_iter()
            .find(|(p, _)| *p == Provider::Codex)
            .map(|(_, p)| codex_titles(&p))
            .unwrap_or_default();
        let matched: Vec<_> = update
            .tasks
            .iter()
            .filter(|t| t.provider == Provider::Codex && native.contains_key(&t.id))
            .collect();
        assert!(matched.iter().all(|t| native.get(&t.id) == Some(&t.title)));
        println!("与原客户端名称一致的任务：{}", matched.len());
        println!(
            "发现记录文件：{}；有效任务：{}；运行中：{}",
            w.paths.len(),
            update.tasks.len(),
            update
                .tasks
                .iter()
                .filter(|t| t.status == Status::Running)
                .count()
        );
    }
    #[cfg(windows)]
    #[test]
    fn monitor_windows_hook_records_only_metadata_and_preserves_config() {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        let root =
            std::env::temp_dir().join(format!("daozai-hook-中文 space-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let script = root.join("daozai-ai-monitor-hook.ps1");
        fs::write(&script, HOOK_SCRIPT).unwrap();
        use std::os::windows::process::CommandExt;
        let mut child = Command::new("powershell.exe")
            .creation_flags(0x08000000)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .args(["codex", "PermissionRequest"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(br#"{"session_id":"fixture-id","prompt":"PRIVATE_PROMPT","tool_input":{"secret":"PRIVATE_SECRET"}}"#).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        let files: Vec<_> = fs::read_dir(root.join("events"))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(files.len(), 1);
        let bytes = fs::read(files[0].path()).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("PRIVATE"));
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["event"], "PermissionRequest");
        assert_eq!(event["id"], "fixture-id");
        let config = root.join("hooks.json");
        fs::write(&config, b"{broken").unwrap();
        assert!(install(&config, Provider::Codex, &script, true).is_err());
        assert_eq!(fs::read(&config).unwrap(), b"{broken");
        fs::remove_file(files[0].path()).unwrap();
        fs::remove_file(config).unwrap();
        fs::remove_file(script).unwrap();
        let _ = fs::remove_dir(root.join("events"));
        let _ = fs::remove_dir(root);
    }
}
