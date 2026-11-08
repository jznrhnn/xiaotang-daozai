//! A dedicated task inbox. Opening the window never marks all tasks as read.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
struct Item {
    task: Task,
    unread: bool,
}

#[derive(Default)]
pub(super) struct Inbox {
    items: HashMap<String, Item>,
    loaded: bool,
    selected: Option<String>,
    filter: usize,
    query: String,
    save_error: Option<String>,
    dirty: bool,
}
fn task_key(task: &Task) -> String {
    format!("{}:{}", task.provider.name(), task.id)
}
impl Inbox {
    pub(super) fn has_unread(&self, status: Status) -> bool {
        self.items
            .values()
            .any(|item| item.unread && item.task.status == status)
    }
    pub(super) fn unread_count(&self) -> usize {
        self.items.values().filter(|i| i.unread).count()
    }
    fn save(&mut self) {
        self.dirty = false;
        if cfg!(test) {
            return;
        }
        let path = crate::model::config_dir().join("ai-task-inbox.json");
        let result = (|| -> Result<(), String> {
            let bytes = serde_json::to_vec(&self.items.values().cloned().collect::<Vec<_>>())
                .map_err(|_| "无法保存任务提醒")?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|_| "无法创建任务提醒目录")?;
            }
            crate::model::atomic_write(&path, &bytes)
                .map_err(|_| "任务未读状态保存失败，请检查数据目录权限".into())
        })();
        self.save_error = result.err();
    }
    pub(super) fn record(&mut self, task: Task, live: bool) {
        let key = task_key(&task);
        // Metadata-only changes must survive snapshots without changing unread
        // state, even when the status snapshot itself is older than our cache.
        if let Some(item) = self.items.get_mut(&key) {
            if item.task.title != task.title && task.title != "未命名任务" {
                item.task.title = task.title.clone();
                self.dirty = true;
            }
        }
        if self
            .items
            .get(&key)
            .is_some_and(|i| i.task.updated > task.updated)
        {
            return;
        }
        if !live
            && task.status == Status::History
            && self.items.get(&key).is_some_and(|i| {
                i.task.updated >= task.updated
                    && matches!(i.task.status, Status::Completed | Status::Failed)
            })
        {
            return;
        }
        let alert = matches!(
            task.status,
            Status::Waiting | Status::Completed | Status::Failed
        );
        let old = self.items.get(&key);
        let changed =
            old.is_none_or(|i| i.task.status != task.status || i.task.updated != task.updated);
        let unread = if live && changed && alert {
            true
        } else if !alert {
            false
        } else {
            old.is_some_and(|i| i.unread)
        };
        let dirty = changed || old.is_none_or(|i| i.unread != unread);
        self.items.insert(key, Item { task, unread });
        if self.items.len() > 200 {
            let mut keys: Vec<_> = self
                .items
                .iter()
                .map(|(k, i)| (k.clone(), i.unread, i.task.updated))
                .collect();
            keys.sort_by_key(|(_, unread, time)| (*unread, *time));
            for (key, _, _) in keys.into_iter().take(self.items.len() - 200) {
                self.items.remove(&key);
            }
        }
        if dirty {
            self.dirty = true;
        }
    }
    pub(super) fn flush(&mut self) {
        if self.dirty {
            self.save();
        }
    }
    fn matches(&self, item: &Item) -> bool {
        let state = match self.filter {
            1 => item.unread,
            2 => item.task.status == Status::Running,
            3 => item.task.status == Status::Waiting,
            4 => item.task.status == Status::Completed,
            5 => item.task.status == Status::Failed,
            _ => true,
        };
        state
            && (self.query.is_empty()
                || format!(
                    "{} {} {}",
                    item.task.title,
                    item.task.provider.name(),
                    item.task.id
                )
                .to_lowercase()
                .contains(&self.query.to_lowercase()))
    }
}

impl State {
    /// Compact island list, using the same four-row layout as the mail module.
    pub(crate) fn island_tasks(&mut self, c: &crate::paint::Card) {
        use crate::{
            paint::{Card, H},
            theme::{self, W},
        };
        let unread = self.inbox.unread_count();
        if unread > 0 {
            c.text(
                88.0,
                24.0,
                &format!("{unread} 未读"),
                10.0,
                W::Regular,
                theme::GRAY,
                H::Left,
            );
        }
        let mut items: Vec<_> = self
            .inbox
            .items
            .iter()
            .map(|(key, item)| (key.clone(), item.clone()))
            .collect();
        items.sort_by_key(|(_, i)| std::cmp::Reverse(i.task.updated));
        if items.is_empty() {
            c.text(
                43.0,
                63.0,
                "暂无本机 AI 任务",
                12.0,
                W::Bold,
                theme::WHITE,
                H::Left,
            );
            c.text(
                43.0,
                82.0,
                "任务状态会自动更新，点击右上角设置接入",
                10.5,
                W::Regular,
                theme::GRAY,
                H::Left,
            );
            return;
        }
        let mut content = egui::Ui::new(
            c.ui.ctx().clone(),
            egui::Id::new(("ai-island-list", c.live)),
            egui::UiBuilder::new()
                .max_rect(c.rect(36.0, 36.0, 344.0, 86.0))
                .layer_id(c.painter.layer_id())
                .style(c.ui.style().clone()),
        );
        content.set_clip_rect(
            c.rect(36.0, 36.0, 344.0, 86.0)
                .intersect(c.painter.clip_rect()),
        );
        content.set_opacity(c.painter.opacity());
        if !c.live {
            content.disable();
        }
        content.spacing_mut().item_spacing.y = 0.0;
        content.spacing_mut().scroll.floating = false;
        content.spacing_mut().scroll.bar_width = 4.0 * c.s;
        content.visuals_mut().widgets.inactive.bg_fill = Color32::from_gray(80);
        egui::ScrollArea::vertical()
            .id_salt("ai-island-scroll")
            .max_height(86.0 * c.s)
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .show_rows(&mut content, 21.5 * c.s, items.len(), |ui, range| {
                for (key, item) in &items[range] {
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(332.0 * c.s, 21.5 * c.s),
                        egui::Sense::hover(),
                    );
                    let mut row = Card::new(ui, rect.min, ui.clip_rect());
                    row.s = c.s;
                    row.live = c.live;
                    let hit = row.hit(0.0, 0.0, 332.0, 20.5, ("ai-task-row", key));
                    if hit.hovered() {
                        row.fill(0.0, 0.0, 332.0, 20.5, 5.0, Color32::from_gray(27));
                    }
                    let color = light_color(item.task.status, true);
                    row.circle(7.0, 10.0, 2.8, color);
                    row.text(
                        17.0,
                        14.0,
                        if item.task.provider == Provider::Codex {
                            "Codex"
                        } else {
                            "Claude"
                        },
                        10.0,
                        W::Regular,
                        theme::GRAY,
                        H::Left,
                    );
                    row.text(
                        61.0,
                        14.0,
                        &row.fit(
                            &item.task.title,
                            10.5,
                            if item.unread { W::Bold } else { W::Regular },
                            201.0,
                        ),
                        10.5,
                        if item.unread { W::Bold } else { W::Regular },
                        theme::WHITE,
                        H::Left,
                    );
                    row.text(
                        327.0,
                        14.0,
                        match item.task.status {
                            Status::Waiting => "需确认",
                            Status::History => "未知",
                            s => s.label(),
                        },
                        10.0,
                        W::Regular,
                        color,
                        H::Right,
                    );
                    row.seg((4.0, 21.0), (328.0, 21.0), 0.5, Color32::from_gray(40));
                    if hit
                        .on_hover_text(format!(
                            "{}\n{} · {}\n点击查看任务详情",
                            item.task.title,
                            item.task.provider.name(),
                            item.task.status.label()
                        ))
                        .clicked()
                    {
                        if self.busy || self.loading || self.connecting {
                            self.open_chat(ui.ctx());
                            self.note = "当前对话正在回复或读取，请结束后再切换任务".into();
                        } else {
                            self.inbox.selected = Some(key.clone());
                            if self.service.is_none() && !cfg!(test) {
                                self.pending_task_chat =
                                    Some((item.task.provider, item.task.id.clone()));
                                self.open_chat(ui.ctx());
                            } else {
                                self.open_chat(ui.ctx());
                                self.choose(item.task.provider, item.task.id.clone());
                            }
                            self.read_task(key);
                        }
                    }
                }
            });
    }
    pub(super) fn load_inbox(&mut self) {
        if self.inbox.loaded {
            return;
        }
        self.inbox.loaded = true;
        if cfg!(test) {
            return;
        }
        let path = crate::model::config_dir().join("ai-task-inbox.json");
        if let Ok(file) = std::fs::File::open(path) {
            use std::io::Read;
            match serde_json::from_reader::<_, Vec<Item>>(file.take(2 * 1024 * 1024)) {
                Ok(mut items) => {
                    items.sort_by_key(|i| std::cmp::Reverse(i.task.updated));
                    for item in items.into_iter().take(200) {
                        if item.unread {
                            self.observe_edge(
                                &format!("external:{}:{}", item.task.provider.name(), item.task.id),
                                item.task.status,
                                true,
                            );
                        }
                        self.inbox.items.insert(task_key(&item.task), item);
                    }
                }
                Err(_) => {
                    self.inbox.save_error =
                        Some("任务提醒记录无法读取；本次任务监听仍可使用".into())
                }
            }
        }
        self.unread = self.inbox.unread_count() > 0;
    }
    fn read_task(&mut self, key: &str) {
        let Some(item) = self.inbox.items.get_mut(key) else {
            return;
        };
        item.unread = false;
        let task = item.task.clone();
        for id in [
            &task.id,
            &format!("external:{}:{}", task.provider.name(), task.id),
        ] {
            if let Some(notice) = self.edge_notices.get_mut(id) {
                // An outstanding approval stays orange even after being read.
                if notice.status != Status::Waiting {
                    notice.seen = true;
                }
            }
        }
        self.inbox.save();
        self.unread = self.inbox.unread_count() > 0;
    }
    #[cfg(test)]
    fn tasks_content(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("ai-tasks-heading")
            .frame(egui::Frame::new().inner_margin(14))
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("AI 任务").size(21.0).strong());
                    ui.label(
                        RichText::new(format!("{} 个未读", self.inbox.unread_count()))
                            .color(Self::theme_color(ui, Status::Running)),
                    );
                    if ui.button("AI 对话 ↗").clicked() {
                        self.open_chat(ctx);
                    }
                    if ui.button("全部标为已读").clicked() {
                        for item in self.inbox.items.values_mut() {
                            item.unread = false;
                        }
                        self.acknowledge_edge();
                        self.inbox.save();
                        self.unread = false;
                    }
                });
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    for (index, name) in ["全部", "未读", "进行中", "需确认", "已完成", "失败"]
                        .iter()
                        .enumerate()
                    {
                        ui.selectable_value(&mut self.inbox.filter, index, *name);
                    }
                });
            });
        egui::SidePanel::left("ai-task-inbox-list")
            .default_width(310.0)
            .min_width(260.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.inbox.query)
                        .hint_text("搜索任务、客户端")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(8.0);
                let mut items: Vec<_> = self
                    .inbox
                    .items
                    .iter()
                    .filter(|(_, i)| self.inbox.matches(i))
                    .map(|(k, i)| (k.clone(), i.clone()))
                    .collect();
                items.sort_by_key(|(_, i)| std::cmp::Reverse(i.task.updated));
                egui::ScrollArea::vertical()
                    .id_salt("ai-task-inbox-scroll")
                    .show(ui, |ui| {
                        if items.is_empty() {
                            ui.label(if self.inbox.items.is_empty() {
                                "等待发现本机任务…"
                            } else {
                                "没有符合条件的任务"
                            });
                        }
                        for (key, item) in items {
                            let selected = self.inbox.selected.as_ref() == Some(&key);
                            let row = egui::Frame::new()
                                .fill(if selected {
                                    ui.visuals().selection.bg_fill
                                } else {
                                    Color32::TRANSPARENT
                                })
                                .corner_radius(7.0)
                                .inner_margin(10)
                                .show(ui, |ui| {
                                    ui.set_min_width((ui.available_width() - 2.0).max(100.0));
                                    ui.horizontal(|ui| {
                                        if item.unread {
                                            ui.label(
                                                RichText::new("●")
                                                    .color(Self::theme_color(ui, Status::Running)),
                                            );
                                        }
                                        ui.add(
                                            egui::Label::new(if item.unread {
                                                RichText::new(&item.task.title).strong()
                                            } else {
                                                RichText::new(&item.task.title)
                                            })
                                            .truncate(),
                                        );
                                    });
                                    ui.horizontal(|ui| {
                                        status_row(ui, item.task.status, item.task.status.label());
                                        ui.small(item.task.provider.name());
                                    });
                                    ui.label(
                                        RichText::new(task_time(item.task.updated))
                                            .small()
                                            .color(ui.visuals().weak_text_color()),
                                    );
                                })
                                .response
                                .interact(egui::Sense::click());
                            if row.on_hover_text(&item.task.title).clicked() {
                                self.inbox.selected = Some(key.clone());
                                self.read_task(&key);
                            }
                            ui.add_space(4.0);
                        }
                    });
            });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(ctx.style().visuals.panel_fill).inner_margin(22)).show(ctx, |ui| {
            let item = self.inbox.selected.as_ref().and_then(|k| self.inbox.items.get(k)).cloned();
            if let Some(item) = item {
                let task = item.task;
                ui.heading(&task.title); ui.add_space(12.0);
                status_row(ui, task.status, task.status.label()).on_hover_ui(Self::legend);
                ui.label(format!("{}  ·  {}", task.provider.name(), if task.owned { "岛仔任务" } else { "本机客户端任务" }));
                ui.label(format!("最近更新：{}", task_time(task.updated))); ui.add_space(12.0); ui.separator(); ui.add_space(12.0);
                ui.label(match task.status {
                    Status::Running => "任务正在执行，状态会自动更新。你可以继续查看其他任务。",
                    Status::Waiting => "任务需要你确认或回答问题，请回到原客户端处理。查看此提醒不会自动授权。",
                    Status::Completed => "本轮执行已完成。你可以查看对话，了解结果和后续事项。",
                    Status::Failed => "任务执行失败，请在对话或原客户端中查看原因。",
                    Status::Cancelled => "本轮执行已停止。",
                    Status::Idle => "会话当前空闲。",
                    Status::History => "暂时没有可确认的实时状态，这不代表任务已完成。",
                });
                ui.add_space(16.0);
                let child = task.provider == Provider::Claude && task.id.starts_with("agent-");
                if ui.add_enabled(!self.busy && !child, egui::Button::new("查看对话 ↗")).clicked() {
                    if self.service.is_none() && !cfg!(test) {
                        self.pending_task_chat = Some((task.provider, task.id.clone())); self.open_chat(ctx);
                    } else { self.open_chat(ctx); self.choose(task.provider, task.id.clone()); }
                }
                if self.busy { ui.small("岛仔当前正在回复，结束后可切换查看其他对话。"); }
                if child { ui.small("子任务详情请在原客户端查看。"); }
                if ui.small_button("复制任务编号").clicked() { ctx.copy_text(task.id.clone()); }
                ui.add_space(12.0); ui.label(RichText::new(format!("任务编号：{}", task.id)).small().color(ui.visuals().weak_text_color()));
            } else {
                ui.add_space(65.0); ui.heading("每个任务，单独查看"); ui.add_space(10.0);
                ui.label("从左侧选择任务查看状态。打开窗口不会清除未读提醒，只有点击任务或“全部标为已读”才会标记已读。");
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                if let Some(error) = &self.inbox.save_error { ui.colored_label(ui.visuals().error_fg_color, error); }
                ui.label(RichText::new(&self.monitor_note).small().color(ui.visuals().weak_text_color()));
                ui.horizontal_wrapped(|ui| {
                    ui.small_button("状态灯说明").on_hover_ui(Self::legend);
                    if ui.small_button(if self.monitor_disabled { "开启实时事件" } else { "关闭实时事件" }).clicked() {
                        self.monitor_disabled = !self.monitor_disabled; if let Some(m) = &self.monitor { m.configure(!self.monitor_disabled); }
                    }
                    if ui.small_button("重新接入").clicked() { self.monitor_disabled = false; if let Some(m) = &self.monitor { m.configure(true); } }
                });
            });
        });
    }
    #[cfg(test)]
    pub(crate) fn preview_tasks(&mut self) {
        self.preview();
        self.open = false;
        self.tasks_open = true;
        let now = chrono::Utc::now().timestamp();
        for (index, (name, provider, status)) in [
            ("检查云同步的配置兼容性", Provider::Codex, Status::Running),
            ("等待确认数据库迁移方案", Provider::Claude, Status::Waiting),
            ("整理今天的开发计划", Provider::Codex, Status::Completed),
            ("运行项目回归检查", Provider::Claude, Status::Failed),
            ("更新项目使用说明", Provider::Codex, Status::Completed),
        ]
        .into_iter()
        .enumerate()
        {
            self.inbox.record(
                Task {
                    provider,
                    id: format!("demo-task-{index}"),
                    title: name.into(),
                    status,
                    updated: now - index as i64 * 60,
                    owned: false,
                },
                true,
            );
        }
        self.inbox.selected = Some("Claude Code:demo-task-1".into());
        self.read_task("Claude Code:demo-task-1");
    }
}

fn task_time(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "更新时间未知".into())
}

#[cfg(test)]
impl crate::app::App {
    pub(super) fn ai_tasks_ui(&mut self, ctx: &egui::Context) {
        if !self.ai_assistant.tasks_open {
            return;
        }
        let focus = std::mem::take(&mut self.ai_assistant.tasks_focus);
        let mut close = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("daozai-ai-tasks"),
            egui::ViewportBuilder::default()
                .with_title("AI 任务 · 岛仔")
                .with_icon(crate::theme::app_icon())
                .with_inner_size([940.0, 680.0])
                .with_min_inner_size([760.0, 560.0]),
            |ctx, _| {
                if focus {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                close = ctx.input(|i| i.viewport().close_requested());
                let original = ctx.style();
                let mut style = (*original).clone();
                style.visuals = if self
                    .settings
                    .settings_theme
                    .dark(crate::settings_theme::system_dark(ctx))
                {
                    egui::Visuals::dark()
                } else {
                    egui::Visuals::light()
                };
                style.visuals.override_text_color = Some(if style.visuals.dark_mode {
                    Color32::from_gray(230)
                } else {
                    Color32::from_gray(35)
                });
                style.spacing.interact_size.y = 28.0;
                style.spacing.button_padding = egui::vec2(10.0, 5.0);
                ctx.set_style(style);
                self.ai_assistant.tasks_content(ctx);
                ctx.set_style(original);
            },
        );
        if close {
            self.ai_assistant.tasks_open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn eyes_show_unread_results_then_return_to_running_or_normal() {
        let mut state = State::default();
        state.busy = true;
        assert_eq!(state.eye_status(), Some(Status::Running));
        state.inbox.record(task("done", Status::Completed, 1), true);
        assert_eq!(state.eye_status(), Some(Status::Completed));
        state.read_task("Codex:done");
        assert_eq!(state.eye_status(), Some(Status::Running));
        state.busy = false;
        assert_eq!(state.eye_status(), None);
        state.inbox.record(task("old", Status::Completed, 1), false);
        assert_eq!(state.eye_status(), None);
        state.inbox.record(task("failed", Status::Failed, 1), true);
        assert_eq!(state.eye_status(), Some(Status::Failed));
        state.inbox.record(task("ask", Status::Waiting, 2), true);
        assert_eq!(state.eye_status(), Some(Status::Waiting));
    }
    fn task(id: &str, status: Status, updated: i64) -> Task {
        Task {
            provider: Provider::Codex,
            id: id.into(),
            title: id.into(),
            status,
            updated,
            owned: false,
        }
    }
    #[test]
    fn reading_one_task_does_not_clear_other_notifications() {
        let mut state = State::default();
        for id in ["one", "two"] {
            state.inbox.record(task(id, Status::Completed, 1), true);
            state.observe_edge(id, Status::Completed, true);
        }
        state.open(&egui::Context::default());
        assert_eq!(state.inbox.unread_count(), 2);
        state.read_task("Codex:one");
        assert_eq!(state.inbox.unread_count(), 1);
        assert!(!state.edge_notices["two"].seen);
        assert!(!state.tasks_open);
        assert!(state.open);
    }
    #[test]
    fn unread_survives_snapshot_and_new_completion_reopens_it() {
        let mut inbox = Inbox::default();
        inbox.record(task("one", Status::Completed, 1), true);
        inbox.record(task("one", Status::Completed, 1), false);
        assert_eq!(inbox.unread_count(), 1);
        let mut renamed = task("one", Status::Completed, 1);
        renamed.title = "原客户端名称".into();
        inbox.record(renamed, false);
        assert_eq!(inbox.items["Codex:one"].task.title, "原客户端名称");
        assert_eq!(inbox.unread_count(), 1);
        inbox.record(task("one", Status::History, 1), false);
        assert_eq!(inbox.items["Codex:one"].task.status, Status::Completed);
        assert_eq!(inbox.unread_count(), 1);
        let bytes = serde_json::to_vec(&inbox.items.values().cloned().collect::<Vec<_>>()).unwrap();
        assert!(serde_json::from_slice::<Vec<Item>>(&bytes).unwrap()[0].unread);
        inbox.items.get_mut("Codex:one").unwrap().unread = false;
        inbox.record(task("one", Status::Running, 2), true);
        inbox.record(task("one", Status::Completed, 3), true);
        assert_eq!(inbox.unread_count(), 1);
    }
    #[test]
    fn history_is_read_and_reading_wait_does_not_approve_it() {
        let mut state = State::default();
        state.inbox.record(task("old", Status::Completed, 1), false);
        assert_eq!(state.inbox.unread_count(), 0);
        state
            .inbox
            .record(task("approval", Status::Waiting, 2), true);
        state.observe_edge("approval", Status::Waiting, true);
        state.read_task("Codex:approval");
        assert_eq!(state.edge_alert().unwrap().0, Status::Waiting);
        assert_eq!(
            state.inbox.items["Codex:approval"].task.status,
            Status::Waiting
        );
    }
}
