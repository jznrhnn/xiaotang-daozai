//! The island launcher and a native chat/task window. No browser or credentials form needed.
use crate::ai_assistant::{
    Agent, Control, Event, Message, Provider, Service, Status, Task, MAX_TEXT,
};
use eframe::egui::{self, Color32, RichText};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

struct EdgeNotice {
    status: Status,
    at: Instant,
    seen: bool,
}

#[derive(Default)]
pub(crate) struct State {
    pub open: bool,
    pub busy: bool,
    pub unread: bool,
    last_status: Option<Status>,
    edge_notices: HashMap<String, EdgeNotice>,
    focus: bool,
    service: Option<Service>,
    agents: Vec<Agent>,
    provider: Option<Provider>,
    tasks: Vec<Task>,
    selected: Option<String>,
    messages: Vec<Message>,
    input: String,
    search: String,
    connecting: bool,
    loading: bool,
    next: Option<String>,
    note: String,
    error: bool,
    pending_input: Option<String>,
    refresh: Option<Instant>,
}
impl State {
    pub fn edge_alert(&self) -> Option<(Status, bool)> {
        [Status::Waiting, Status::Failed, Status::Completed]
            .into_iter()
            .find_map(|status| {
                let notices: Vec<_> = self
                    .edge_notices
                    .values()
                    .filter(|n| {
                        n.status == status && (!n.seen || n.at.elapsed() < Duration::from_secs(8))
                    })
                    .collect();
                (!notices.is_empty()).then(|| {
                    (
                        status,
                        notices
                            .iter()
                            .any(|n| n.at.elapsed() < Duration::from_secs(8)),
                    )
                })
            })
    }
    fn observe_edge(&mut self, id: &str, status: Status, live: bool) {
        if status == Status::Waiting || live && matches!(status, Status::Completed | Status::Failed)
        {
            if self.edge_notices.get(id).is_none_or(|n| n.status != status) {
                self.edge_notices.insert(
                    id.into(),
                    EdgeNotice {
                        status,
                        at: Instant::now(),
                        seen: false,
                    },
                );
            }
        } else if live
            || self
                .edge_notices
                .get(id)
                .is_some_and(|n| n.status == Status::Waiting)
        {
            self.edge_notices.remove(id);
        }
    }
    fn acknowledge_edge(&mut self) {
        for notice in self.edge_notices.values_mut() {
            if notice.status != Status::Waiting {
                notice.seen = true;
            }
        }
    }
    pub fn indicator_status(&self) -> Status {
        if self.last_status == Some(Status::Waiting)
            || self.tasks.iter().any(|t| t.status == Status::Waiting)
        {
            return Status::Waiting;
        }
        if self.busy {
            return Status::Running;
        }
        if let Some(status) = self.last_status {
            return status;
        }
        if self.tasks.iter().any(|t| t.status == Status::Failed) {
            return Status::Failed;
        }
        Status::History
    }
    pub fn status_tooltip(&self) -> String {
        let current = match self.indicator_status() {
            Status::History => "暂无已确认的实时任务状态",
            Status::Waiting => "需要用户确认，请在原客户端处理",
            status => status.label(),
        };
        format!("AI 任务状态\n当前：{current}\n\n红灯：执行失败\n绿灯：执行完成\n橙灯：需要用户确认\n蓝灯：正在执行\n灰灯：空闲、已停止或状态未知\n\n点击打开 AI 助手")
    }
    pub fn open(&mut self, ctx: &egui::Context) {
        self.open = true;
        self.focus = true;
        self.unread = false;
        self.acknowledge_edge();
        if self.service.is_none() && !cfg!(test) {
            self.discover(ctx);
        }
    }
    fn command(&mut self, command: Control) -> bool {
        let result = self
            .service
            .as_ref()
            .ok_or_else(|| "请点击自动识别连接本机 AI".to_string())
            .and_then(|s| s.send(command));
        if let Err(error) = result {
            self.note = error;
            self.error = true;
            false
        } else {
            true
        }
    }
    fn discover(&mut self, ctx: &egui::Context) {
        if self.busy {
            return;
        }
        self.service = None;
        self.agents.clear();
        self.provider = None;
        self.tasks.clear();
        self.selected = None;
        self.messages.clear();
        self.next = None;
        match Service::start(ctx.clone()) {
            Ok(service) => {
                self.service = Some(service);
                self.connecting = true;
                self.loading = false;
                self.note = "正在识别本机 AI 客户端和登录状态…".into();
                self.error = false;
                self.command(Control::Discover);
            }
            Err(error) => {
                self.note = error;
                self.error = true;
                self.connecting = false;
            }
        }
    }
    fn list(&mut self, append: bool) {
        let Some(provider) = self.provider else {
            return;
        };
        let cursor = if append { self.next.clone() } else { None };
        if self.command(Control::List { provider, cursor }) {
            self.loading = true;
            self.refresh = Some(Instant::now());
        }
    }
    fn choose(&mut self, provider: Provider, id: String) {
        if self.busy || self.loading {
            return;
        }
        self.provider = Some(provider);
        self.selected = Some(id.clone());
        self.messages.clear();
        self.note.clear();
        self.error = false;
        if self.command(Control::Read { provider, id }) {
            self.loading = true;
        }
    }
    fn send(&mut self) {
        let Some(provider) = self.provider else {
            return;
        };
        if self.busy || self.loading || self.connecting || self.input.trim().is_empty() {
            return;
        }
        if !self
            .agents
            .iter()
            .any(|a| a.provider == provider && a.available)
        {
            return;
        }
        let text = self.input.trim().to_owned();
        if self.command(Control::Send {
            provider,
            id: self.selected.clone(),
            text: text.clone(),
        }) {
            self.pending_input = Some(text.clone());
            self.messages.push(Message {
                id: format!("user-{}", chrono::Utc::now().timestamp_millis()),
                user: true,
                text,
            });
            self.input.clear();
            self.busy = true;
            self.note = "正在发送消息…".into();
            self.error = false;
        }
    }
    fn poll(&mut self, ctx: &egui::Context) {
        self.edge_notices
            .retain(|_, n| !n.seen || n.at.elapsed() < Duration::from_secs(8));
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(service) = &self.service {
            for _ in 0..512 {
                match service.rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for event in events {
            self.receive(event);
        }
        if disconnected {
            if self.busy {
                self.last_status = Some(Status::Failed);
            }
            self.service = None;
            self.connecting = false;
            self.loading = false;
            self.busy = false;
            self.note = "AI 连接已退出，请点击自动识别重连".into();
            self.error = true;
        }
        if self.open
            && !self.busy
            && !self.loading
            && !self.connecting
            && self.provider.is_some()
            && self
                .refresh
                .is_some_and(|t| t.elapsed() > Duration::from_secs(30))
        {
            self.list(false);
        }
        if self.busy || self.loading || self.connecting {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.open {
            ctx.request_repaint_after(Duration::from_secs(2));
        }
    }
    fn receive(&mut self, event: Event) {
        match event {
            Event::Agents(agents) => {
                self.connecting = false;
                self.provider = agents
                    .iter()
                    .find(|a| a.available)
                    .or_else(|| agents.first())
                    .map(|a| a.provider);
                self.agents = agents;
                self.note = if self.agents.is_empty() {
                    "未发现可用客户端。先安装并登录 Codex 或 Claude Code，再点击自动识别。".into()
                } else {
                    "已识别本机客户端，直接复用各自的登录状态".into()
                };
                self.error = false;
                self.list(false);
            }
            Event::Tasks {
                provider,
                tasks,
                next,
                append,
            } => {
                if self.provider != Some(provider) {
                    return;
                }
                let active = self
                    .selected
                    .as_ref()
                    .and_then(|id| self.tasks.iter().find(|t| &t.id == id))
                    .cloned();
                if !append {
                    self.tasks.retain(|t| t.provider != provider);
                }
                for task in tasks {
                    self.observe_edge(&task.id, task.status, false);
                    if let Some(existing) = self
                        .tasks
                        .iter_mut()
                        .find(|t| t.provider == provider && t.id == task.id)
                    {
                        *existing = task;
                    } else {
                        self.tasks.push(task);
                    }
                }
                if self.busy {
                    if let Some(active) = active {
                        self.upsert(active);
                    }
                }
                self.tasks.sort_by_key(|t| std::cmp::Reverse(t.updated));
                self.tasks.truncate(200);
                self.next = next;
                self.loading = false;
            }
            Event::History {
                provider,
                id,
                messages,
            } => {
                if self.provider == Some(provider) && self.selected.as_deref() == Some(&id) {
                    self.messages = messages;
                    self.loading = false;
                }
            }
            Event::Started {
                provider,
                id,
                title,
                forked,
            } => {
                let old = self.selected.clone();
                let old_title = old
                    .as_ref()
                    .and_then(|old| self.tasks.iter().find(|t| t.id == *old))
                    .map(|t| t.title.clone());
                if old.as_ref().is_some_and(|s| s.starts_with("pending-")) {
                    self.tasks.retain(|t| Some(&t.id) != old.as_ref());
                }
                self.last_status = Some(Status::Running);
                self.observe_edge(&id, Status::Running, true);
                self.selected = Some(id.clone());
                self.provider = Some(provider);
                self.busy = true;
                self.pending_input = None;
                self.note = if forked {
                    "已创建对话副本，正在回复…"
                } else {
                    "正在回复…"
                }
                .into();
                self.error = false;
                self.upsert(Task {
                    provider,
                    id,
                    title: if title.is_empty() {
                        old_title.unwrap_or_else(|| "岛仔对话".into())
                    } else {
                        title
                    },
                    status: Status::Running,
                    updated: chrono::Utc::now().timestamp(),
                    owned: true,
                });
            }
            Event::Text {
                id,
                item,
                text,
                replace,
            } => {
                if self.selected.as_deref() != Some(&id) {
                    return;
                }
                if let Some(message) = self.messages.iter_mut().find(|m| !m.user && m.id == item) {
                    update_text(&mut message.text, &text, replace);
                } else {
                    let mut bounded = String::new();
                    update_text(&mut bounded, &text, true);
                    self.messages.push(Message {
                        id: item,
                        user: false,
                        text: bounded,
                    });
                }
                if self.messages.len() > 200 {
                    self.messages.drain(..self.messages.len() - 200);
                }
            }
            Event::Status { id, status, note } => {
                self.observe_edge(&id, status, true);
                self.last_status = Some(status);
                if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id) {
                    task.status = status;
                    task.updated = chrono::Utc::now().timestamp();
                }
                if self.selected.as_deref() == Some(&id) {
                    self.busy = matches!(status, Status::Running | Status::Waiting);
                    self.note = note;
                    self.error = status == Status::Failed;
                    if !self.busy && !self.open {
                        self.unread = true;
                    }
                }
            }
            Event::Error(error) => {
                self.note = error;
                self.error = true;
                self.loading = false;
                self.connecting = false;
                if let Some(text) = self.pending_input.take() {
                    self.last_status = Some(Status::Failed);
                    self.input = text;
                    if self.messages.last().is_some_and(|m| m.user) {
                        self.messages.pop();
                    }
                    self.busy = false;
                }
            }
        }
    }
    fn upsert(&mut self, task: Task) {
        if let Some(existing) = self
            .tasks
            .iter_mut()
            .find(|t| t.provider == task.provider && t.id == task.id)
        {
            *existing = task;
        } else {
            self.tasks.insert(0, task);
        }
    }
    fn theme_color(ui: &egui::Ui, status: Status) -> Color32 {
        light_color(status, ui.visuals().dark_mode)
    }
    fn legend(ui: &mut egui::Ui) {
        ui.label(RichText::new("AI 任务状态说明").strong());
        for (status, label) in [
            (Status::Failed, "红灯：执行失败"),
            (Status::Completed, "绿灯：执行完成"),
            (Status::Waiting, "橙灯：需要用户确认"),
            (Status::Running, "蓝灯：正在执行"),
            (Status::History, "灰灯：空闲、已停止或状态未知"),
        ] {
            status_row(ui, status, label);
        }
        ui.small("确认或授权操作请在原客户端处理；历史记录不会推测成已完成。");
    }
}
pub(crate) fn light_color(status: Status, dark: bool) -> Color32 {
    match status {
        Status::Running => {
            if dark {
                Color32::from_rgb(140, 190, 255)
            } else {
                Color32::from_rgb(35, 98, 180)
            }
        }
        Status::Completed => {
            if dark {
                Color32::from_rgb(112, 221, 160)
            } else {
                Color32::from_rgb(25, 117, 65)
            }
        }
        Status::Failed => {
            if dark {
                Color32::from_rgb(255, 120, 120)
            } else {
                Color32::from_rgb(190, 43, 53)
            }
        }
        Status::Waiting => {
            if dark {
                Color32::from_rgb(255, 178, 70)
            } else {
                Color32::from_rgb(181, 92, 9)
            }
        }
        _ => {
            if dark {
                Color32::from_gray(150)
            } else {
                Color32::from_gray(105)
            }
        }
    }
}
fn status_row(ui: &mut egui::Ui, status: Status, text: &str) -> egui::Response {
    ui.horizontal(|ui| {
        let color = light_color(status, ui.visuals().dark_mode);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 16.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.5, color);
        ui.label(RichText::new(text).small().color(color));
    })
    .response
}
impl State {
    fn content(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("assistant-toolbar")
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(12),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("AI 助手").size(19.0).strong());
                    let status = self.indicator_status();
                    status_row(
                        ui,
                        status,
                        if status == Status::History {
                            "暂无实时状态"
                        } else {
                            status.label()
                        },
                    )
                    .on_hover_ui(Self::legend);
                    let selected = self.provider.map(Provider::name).unwrap_or("自动识别中");
                    ui.add_enabled_ui(!self.busy && !self.loading && !self.connecting, |ui| {
                        egui::ComboBox::from_id_salt("assistant-provider")
                            .selected_text(selected)
                            .show_ui(ui, |ui| {
                                let agents = self.agents.clone();
                                for agent in agents {
                                    if ui
                                        .selectable_label(
                                            self.provider == Some(agent.provider),
                                            agent.provider.name(),
                                        )
                                        .clicked()
                                    {
                                        self.provider = Some(agent.provider);
                                        self.selected = None;
                                        self.messages.clear();
                                        self.next = None;
                                        self.list(false);
                                    }
                                }
                            });
                    });
                    if ui
                        .add_enabled(
                            !self.busy && !self.connecting,
                            egui::Button::new("自动识别"),
                        )
                        .clicked()
                    {
                        self.discover(ctx);
                    }
                    if self.connecting {
                        ui.spinner();
                    }
                    if let Some(agent) = self
                        .agents
                        .iter()
                        .find(|a| Some(a.provider) == self.provider)
                    {
                        ui.label(
                            RichText::new(&agent.note)
                                .small()
                                .color(if agent.available {
                                    Self::theme_color(ui, Status::Completed)
                                } else {
                                    ui.visuals().weak_text_color()
                                }),
                        );
                    }
                });
            });
        egui::SidePanel::left("assistant-tasks")
            .resizable(true)
            .default_width(230.0)
            .width_range(190.0..=320.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("会话与任务");
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.busy && !self.loading && !self.connecting,
                            egui::Button::new("＋ 新对话"),
                        )
                        .clicked()
                    {
                        self.selected = None;
                        self.messages.clear();
                        self.input.clear();
                        self.note.clear();
                        self.error = false;
                    }
                    if ui
                        .add_enabled(
                            !self.busy && !self.loading && !self.connecting,
                            egui::Button::new("刷新"),
                        )
                        .clicked()
                    {
                        self.list(false);
                    }
                });
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("搜索已加载的会话")
                        .desired_width(f32::INFINITY),
                );
                ui.label(
                    RichText::new("历史记录不代表实时运行状态")
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
                ui.small_button("任务状态说明").on_hover_ui(Self::legend);
                ui.separator();
                let tasks: Vec<_> = self
                    .tasks
                    .iter()
                    .filter(|t| {
                        Some(t.provider) == self.provider
                            && (self.search.is_empty()
                                || t.title.to_lowercase().contains(&self.search.to_lowercase()))
                    })
                    .cloned()
                    .collect();
                egui::ScrollArea::vertical()
                    .id_salt("assistant-task-list")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if tasks.is_empty() {
                            ui.label(if self.loading {
                                "正在读取本机会话…"
                            } else {
                                "暂无本机会话"
                            });
                        }
                        for task in tasks {
                            let selected = self.selected.as_ref() == Some(&task.id);
                            let fill = if selected {
                                ui.visuals().selection.bg_fill
                            } else {
                                Color32::TRANSPARENT
                            };
                            let row = egui::Frame::new()
                                .fill(fill)
                                .corner_radius(7.0)
                                .inner_margin(8)
                                .show(ui, |ui| {
                                    ui.set_min_width((ui.available_width() - 2.0).max(100.0));
                                    ui.add(
                                        egui::Label::new(RichText::new(&task.title).strong())
                                            .truncate(),
                                    );
                                    ui.horizontal(|ui| {
                                        status_row(ui, task.status, task.status.label())
                                            .on_hover_ui(Self::legend);
                                        if task.owned {
                                            ui.label(
                                                RichText::new("岛仔")
                                                    .small()
                                                    .color(ui.visuals().weak_text_color()),
                                            );
                                        }
                                    });
                                })
                                .response
                                .interact(egui::Sense::click());
                            if row.on_hover_text(&task.title).clicked()
                                && !self.busy
                                && !self.loading
                            {
                                self.choose(task.provider, task.id);
                            }
                            ui.add_space(3.0);
                        }
                        if self.next.is_some() && self.tasks.len() < 200 {
                            if ui
                                .add_enabled(
                                    !self.loading && !self.busy,
                                    egui::Button::new("加载更多会话"),
                                )
                                .clicked()
                            {
                                self.list(true);
                            }
                        } else if self.tasks.len() >= 200 {
                            ui.small("当前显示最近 200 条会话");
                        }
                    });
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(16),
            )
            .show(ctx, |ui| {
                let task = self
                    .selected
                    .as_ref()
                    .and_then(|id| self.tasks.iter().find(|t| &t.id == id));
                let title = task
                    .map(|t| t.title.as_str())
                    .unwrap_or("新的对话，从这里开始");
                ui.add(egui::Label::new(RichText::new(title).size(17.0).strong()).truncate());
                ui.label(
                    RichText::new(if task.is_some_and(|t| !t.owned) {
                        "继续此会话会创建副本，不影响原客户端。显示最近的对话片段。"
                    } else {
                        "直接复用本机登录 · 只读文本对话 · Ctrl+Enter 发送"
                    })
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
                ui.separator();
                let history_height = (ui.available_height() - 176.0).max(80.0);
                egui::ScrollArea::vertical()
                    .id_salt(("assistant-chat", self.selected.clone()))
                    .stick_to_bottom(true)
                    .max_height(history_height)
                    .min_scrolled_height(history_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.messages.is_empty() {
                            ui.add_space(24.0);
                            ui.label(
                                RichText::new(if self.loading {
                                    "正在读取对话…"
                                } else {
                                    "向你的 AI 问个问题，或从左侧选择已有会话。"
                                })
                                .color(ui.visuals().weak_text_color()),
                            );
                            if self.agents.is_empty() && !self.connecting {
                                ui.label(
                                    "自动发现本机 Codex 和 Claude Code，使用客户端已有的登录。",
                                );
                            }
                        }
                        for (index, message) in self.messages.iter().enumerate() {
                            ui.push_id(index, |ui| {
                                let dark = ui.visuals().dark_mode;
                                let fill = if message.user {
                                    if dark {
                                        Color32::from_rgb(33, 53, 79)
                                    } else {
                                        Color32::from_rgb(232, 240, 252)
                                    }
                                } else if dark {
                                    Color32::from_rgb(36, 36, 40)
                                } else {
                                    Color32::WHITE
                                };
                                egui::Frame::new()
                                    .fill(fill)
                                    .corner_radius(9.0)
                                    .inner_margin(12)
                                    .show(ui, |ui| {
                                        ui.set_min_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(if message.user {
                                                    "你"
                                                } else {
                                                    self.provider
                                                        .map(Provider::name)
                                                        .unwrap_or("AI")
                                                })
                                                .strong()
                                                .small(),
                                            );
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    if ui.small_button("复制").clicked() {
                                                        ui.ctx().copy_text(message.text.clone());
                                                    }
                                                },
                                            );
                                        });
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(&message.text).size(14.0).color(
                                                    if dark {
                                                        Color32::from_gray(235)
                                                    } else {
                                                        Color32::from_gray(35)
                                                    },
                                                ),
                                            )
                                            .selectable(true)
                                            .wrap(),
                                        );
                                    });
                                ui.add_space(8.0);
                            });
                        }
                    });
                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if self.busy {
                        ui.spinner();
                    }
                    let color = if self.error {
                        ui.visuals().error_fg_color
                    } else if self.busy {
                        Self::theme_color(ui, Status::Running)
                    } else if self.selected.as_ref().is_some_and(|id| {
                        self.tasks
                            .iter()
                            .any(|t| &t.id == id && t.status == Status::Completed)
                    }) {
                        Self::theme_color(ui, Status::Completed)
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    ui.add(egui::Label::new(RichText::new(&self.note).small().color(color)).wrap());
                });
                let ready = self
                    .agents
                    .iter()
                    .any(|a| Some(a.provider) == self.provider && a.available);
                let can_edit = ready && !self.busy && !self.loading && !self.connecting;
                let edit = ui.add_enabled(
                    can_edit,
                    egui::TextEdit::multiline(&mut self.input)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .char_limit(8000)
                        .hint_text(if ready {
                            "输入消息，Ctrl+Enter 发送"
                        } else {
                            "先在原客户端登录，然后点击自动识别"
                        }),
                );
                let shortcut = edit.has_focus()
                    && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            can_edit && !self.input.trim().is_empty(),
                            egui::Button::new(RichText::new("发送").color(Color32::WHITE))
                                .fill(Color32::from_rgb(41, 111, 190)),
                        )
                        .clicked()
                        || shortcut
                    {
                        self.send();
                    }
                    if self.busy && ui.button("停止回复").clicked() {
                        self.command(Control::Cancel);
                        self.note = "正在停止…".into();
                    }
                    ui.label(
                        RichText::new("需修改文件或处理授权时，请使用原客户端")
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            });
    }
    #[cfg(test)]
    pub(crate) fn preview_edge(&mut self, status: Status) {
        self.last_status=Some(status);
        self.observe_edge("preview-edge", status, true);
    }
    #[cfg(test)]
    pub(crate) fn preview(&mut self) {
        self.open = true;
        self.provider = Some(Provider::Codex);
        self.agents = vec![Agent {
            provider: Provider::Codex,
            available: true,
            note: "已复用本机登录（预览）".into(),
        }];
        self.selected = Some("demo-chat".into());
        self.last_status = Some(Status::Completed);
        self.tasks = vec![
            Task {
                provider: Provider::Codex,
                id: "demo-chat".into(),
                title: "整理今天的开发计划".into(),
                status: Status::Completed,
                updated: 0,
                owned: true,
            },
            Task {
                provider: Provider::Codex,
                id: "demo-history".into(),
                title: "讨论云同步的交互细节".into(),
                status: Status::History,
                updated: 0,
                owned: false,
            },
            Task {
                provider: Provider::Codex,
                id: "demo-failed".into(),
                title: "检查部署结果".into(),
                status: Status::Failed,
                updated: 0,
                owned: true,
            },
            Task {
                provider: Provider::Codex,
                id: "demo-confirm".into(),
                title: "确认下一步操作".into(),
                status: Status::Waiting,
                updated: 0,
                owned: false,
            },
        ];
        self.messages=vec![Message{id:"u".into(),user:true,text:"帮我整理一下今天的开发计划。".into()},Message{id:"a".into(),user:false,text:"可以先检查昨天的测试结果，再处理云同步的反馈。左侧会显示本机会话；正在进行的岛仔对话会实时更新状态。".into()}];
        self.note = "本轮回复已完成".into();
    }
}
fn update_text(target: &mut String, text: &str, replace: bool) {
    if replace {
        target.clear();
    }
    let mut end = text.len().min(MAX_TEXT.saturating_sub(target.len()));
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&text[..end]);
}
impl crate::app::App {
    pub fn ai_assistant_ui(&mut self, ctx: &egui::Context) {
        self.ai_assistant.poll(ctx);
        if !self.ai_assistant.open {
            return;
        }
        let focus = std::mem::take(&mut self.ai_assistant.focus);
        let mut close = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("daozai-ai-assistant"),
            egui::ViewportBuilder::default()
                .with_title("AI 助手 · 岛仔")
                .with_icon(crate::theme::app_icon())
                .with_inner_size([940.0, 700.0])
                .with_min_inner_size([760.0, 580.0]),
            |ctx, _| {
                if focus {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                close = ctx.input(|i| i.viewport().close_requested());
                if ctx.input(|i| i.viewport().focused == Some(true)) {
                    self.ai_assistant.acknowledge_edge();
                }
                let original = ctx.style();
                let mut style = (*original).clone();
                let dark = self
                    .settings
                    .settings_theme
                    .dark(crate::settings_theme::system_dark(ctx));
                style.visuals = if dark {
                    egui::Visuals::dark()
                } else {
                    egui::Visuals::light()
                };
                style.spacing.interact_size.y = 28.0;
                style.visuals.extreme_bg_color = if dark {
                    Color32::from_rgb(30, 30, 34)
                } else {
                    Color32::WHITE
                };
                style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(7);
                style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(7);
                style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(7);
                style.spacing.button_padding = egui::vec2(10.0, 5.0);
                ctx.set_style(style);
                self.ai_assistant.content(ctx);
                ctx.set_style(original);
            },
        );
        if close {
            self.ai_assistant.open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streaming_replaces_final_text_without_duplicating_it() {
        let mut state = State::default();
        state.selected = Some("chat".into());
        state.busy = true;
        for text in ["你", "好"] {
            state.receive(Event::Text {
                id: "chat".into(),
                item: "a".into(),
                text: text.into(),
                replace: false,
            });
        }
        state.receive(Event::Text {
            id: "chat".into(),
            item: "a".into(),
            text: "你好".into(),
            replace: true,
        });
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].text, "你好");
        state.receive(Event::Text {
            id: "other".into(),
            item: "b".into(),
            text: "不应显示".into(),
            replace: false,
        });
        assert_eq!(state.messages.len(), 1);
    }
    #[test]
    fn terminal_status_unlocks_input_and_tracks_completion_when_closed() {
        let mut state = State::default();
        state.selected = Some("chat".into());
        state.busy = true;
        state.receive(Event::Status {
            id: "chat".into(),
            status: Status::Completed,
            note: "已完成".into(),
        });
        assert!(!state.busy);
        assert!(state.unread);
    }
    #[test]
    fn status_lights_distinguish_failure_completion_and_confirmation_after_opening() {
        let mut state = State::default();
        state.selected = Some("chat".into());
        for status in [Status::Failed, Status::Completed, Status::Waiting] {
            state.receive(Event::Status {
                id: "chat".into(),
                status,
                note: status.label().into(),
            });
            assert_eq!(state.indicator_status(), status);
            state.open(&egui::Context::default());
            assert_eq!(
                state.indicator_status(),
                status,
                "打开窗口不能把失败或完成提示变成绿灯/灰灯"
            );
        }
        assert!(state.status_tooltip().contains("橙灯：需要用户确认"));
        for dark in [false, true] {
            let red = light_color(Status::Failed, dark);
            assert!(red.r() > red.g());
            let green = light_color(Status::Completed, dark);
            assert!(green.g() > green.r());
            let orange = light_color(Status::Waiting, dark);
            assert!(orange.r() > orange.g() && orange.g() > orange.b());
        }
    }
    #[test]
    fn edge_completion_waits_to_be_seen_then_expires_but_confirmation_stays() {
        let mut state = State::default();
        state.receive(Event::Status {
            id: "complete".into(),
            status: Status::Completed,
            note: "已完成".into(),
        });
        assert_eq!(state.edge_alert(), Some((Status::Completed, true)));
        state.edge_notices.get_mut("complete").unwrap().at =
            Instant::now() - Duration::from_secs(9);
        assert_eq!(
            state.edge_alert(),
            Some((Status::Completed, false)),
            "未查看的结果保留常亮，停止持续动画"
        );
        state.acknowledge_edge();
        assert!(state.edge_alert().is_none());
        state.receive(Event::Status {
            id: "confirm".into(),
            status: Status::Waiting,
            note: "请确认".into(),
        });
        state.edge_notices.get_mut("confirm").unwrap().at = Instant::now() - Duration::from_secs(9);
        state.acknowledge_edge();
        assert_eq!(state.edge_alert(), Some((Status::Waiting, false)));
        state.receive(Event::Status {
            id: "confirm".into(),
            status: Status::Running,
            note: "继续执行".into(),
        });
        assert!(state.edge_alert().is_none());
    }
    #[test]
    fn historical_records_do_not_light_the_edge_and_confirmation_has_priority() {
        let mut state = State::default();
        state.observe_edge("history", Status::History, false);
        assert!(state.edge_alert().is_none());
        state.observe_edge("done", Status::Completed, true);
        state.observe_edge("failed", Status::Failed, true);
        state.observe_edge("confirm", Status::Waiting, false);
        assert_eq!(state.edge_alert().unwrap().0, Status::Waiting);
        state.observe_edge("confirm", Status::Idle, false);
        assert_eq!(state.edge_alert().unwrap().0, Status::Failed);
    }
}
