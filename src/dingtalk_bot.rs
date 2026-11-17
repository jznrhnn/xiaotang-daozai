//! Application-bot notifications and optional original-task reply integration.
use crate::ai_assistant::{Status,Task};
use eframe::egui;
use serde::{Deserialize,Serialize};
use serde_json::Value;
use std::{collections::BTreeMap,path::PathBuf,sync::{Arc,atomic::{AtomicBool,Ordering},mpsc},time::{Duration,Instant}};

#[derive(Clone,Serialize,Deserialize,PartialEq)]
#[serde(default)]
pub(crate) struct Config{pub(crate) enabled:bool,pub(crate) only_locked:bool,pub(crate) summary:bool,pub(crate) profile:String,pub(crate) robot:String,pub(crate) name:String,pub(crate) replies:bool}
impl Default for Config{fn default()->Self{Self{enabled:false,only_locked:true,summary:true,profile:String::new(),robot:String::new(),name:String::new(),replies:false}}}
pub(crate) fn read_config()->Result<Config,String>{read("dingtalk-bot-settings.json")}
#[derive(Clone,Default,Serialize,Deserialize)]
struct Ledger{sent:BTreeMap<String,String>}
#[derive(Clone)]
struct Bot{code:String,name:String}
fn path(name:&str)->PathBuf{crate::model::config_dir().join(name)}
fn read<T:serde::de::DeserializeOwned+Default>(name:&str)->Result<T,String>{
    match std::fs::read(path(name)){
        Ok(bytes) if bytes.len()<=2*1024*1024=>serde_json::from_slice(&bytes).map_err(|_|"通知记录损坏，已停止自动发送".into()),
        Ok(_)=>Err("通知记录过大，已停止自动发送".into()),
        Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(T::default()),
        Err(_)=>Err("无法读取通知设置或记录".into()),
    }
}
fn write<T:Serialize>(name:&str,value:&T)->Result<(),String>{
    let bytes=serde_json::to_vec(value).map_err(|_|"无法保存通知设置")?;
    if bytes.len()>2*1024*1024{return Err("通知记录已满，已停止自动发送".into());}
    std::fs::create_dir_all(crate::model::config_dir()).map_err(|_|"无法保存通知记录")?;
    crate::model::atomic_write(&path(name),&bytes).map_err(|_|"无法保存通知记录，未继续发送".into())
}
fn key(t:&Task)->String{format!("{}:{}",t.provider.name(),t.id)}
fn revision(t:&Task)->String{format!("{}:{}",t.updated,t.status.label())}
fn event(config:&Config,t:&Task)->String{format!("{}|{}|{}|{}",config.profile,config.robot,key(t),revision(t))}
fn alert(t:&Task)->bool{matches!(t.status,Status::Completed|Status::Failed|Status::Waiting)}
fn text(t:&Task,summary:bool)->String{
    let reply=if summary{
        crate::ai_monitor::completion_summary(t).filter(|s|!s.trim().is_empty()).map(|s|{
            let mut result=s.trim().chars().take(1500).collect::<String>();
            if s.trim().chars().nth(1500).is_some(){result.push_str("…（内容已截断）");}result
        }).unwrap_or_else(||"暂无可读取的回复".into())
    }else{"未开启回复内容显示".into()};
    reply
}
fn send(c:&Config,body:&str,cancel:&AtomicBool)->Result<crate::dingtalk_binding::Delivery,String>{crate::dingtalk_transport::send(c,body,cancel)}
pub(crate) fn current_profile()->Option<String>{read::<Value>("dingtalk-link.json").ok()?["profile"].as_str().map(str::to_owned)}
fn candidates(tasks:&[Task],observed:&BTreeMap<String,String>,primed:bool)->Vec<Task>{
    if !primed{return Vec::new();}
    tasks.iter().filter(|t|alert(t)&&observed.get(&key(t))!=Some(&revision(t))).cloned().collect()
}
fn deliver(ledger:&mut Ledger,id:String,mut persist:impl FnMut(&Ledger)->Result<(),String>,dispatch:impl FnOnce()->Result<(),String>)->Result<bool,String>{
    let at=Instant::now();let trace=crate::dingtalk_diagnostics::correlation(&id);
    if ledger.sent.contains_key(&id){crate::dingtalk_diagnostics::local("notify","journal","duplicate","ALREADY_ATTEMPTED",0,&trace);return Ok(false);}
    ledger.sent.insert(id.clone(),"发送结果待核对".into());persist(ledger).map_err(|e|{crate::dingtalk_diagnostics::local("notify","journal","failure","JOURNAL_WRITE_FAILED",0,&trace);e})?;
    let result=dispatch();
    crate::dingtalk_diagnostics::local("notify","worker",if result.is_ok(){"success"}else{"failure"},if result.is_ok(){"DELIVERED"}else{"DISPATCH_FAILED"},at.elapsed().as_millis() as u64,&trace);
    ledger.sent.insert(id,if result.is_ok(){"钉钉已接收"}else{"发送失败或结果不明，不自动重发"}.into());persist(ledger).map_err(|e|{crate::dingtalk_diagnostics::local("notify","journal","failure","JOURNAL_RESULT_WRITE_FAILED",0,&trace);e})?;
    result.map(|_|true)
}
struct Worker{cancel:Arc<AtomicBool>,rx:mpsc::Receiver<Result<String,String>>,config:Config}
enum Discovery{Bots(String,Vec<Bot>)}
pub(crate) struct State{
    replies:crate::dingtalk_reply::State,
    config:Config,worker:Option<Worker>,paused:Option<Config>,discovery:Option<mpsc::Receiver<Result<Discovery,String>>>,
    bots:Vec<Bot>,bots_profile:Option<String>,note:String,error:bool,locked:Option<bool>,lock_checked:Instant,valid:bool,
}
impl Default for State{fn default()->Self{
    let config=read("dingtalk-bot-settings.json");let ledger=read::<Ledger>("dingtalk-bot-ledger.json");
    let valid=config.is_ok()&&ledger.is_ok();
    let uncertain=ledger.as_ref().map(|l|l.sent.values().filter(|v|v.as_str()!="钉钉已接收").count()).unwrap_or(0);
    Self{replies:Default::default(),config:config.unwrap_or_default(),worker:None,paused:None,discovery:None,bots:Vec::new(),bots_profile:None,note:if !valid{"通知设置或记录无法读取，已暂停".into()}else if uncertain>0{format!("有 {uncertain} 条通知发送失败或结果待核对，不会自动重发")}else{String::new()},error:!valid||uncertain>0,locked:None,lock_checked:Instant::now()-Duration::from_secs(3),valid}
}}
impl Drop for State{fn drop(&mut self){if let Some(w)=&self.worker{w.cancel.store(true,Ordering::Relaxed);}}}
impl State{
    pub(crate) fn needs_authorization(&self)->bool{self.valid&&(self.config.enabled||self.config.replies)&&!self.config.robot.is_empty()}
    pub(crate) fn needs_task_snapshot(&self)->bool{self.enabled()&&self.worker.is_none()}
    pub(crate) fn enabled(&self)->bool{self.config.enabled&&self.valid}
    pub(crate) fn tick(&mut self,ctx:&egui::Context,profile:Option<&str>,tasks:&[Task]){
        self.replies.tick(&self.config,if self.valid{profile}else{None},ctx);
        if self.bots_profile.as_deref()!=profile{self.bots.clear();self.bots_profile=None;}
        if self.lock_checked.elapsed()>Duration::from_secs(2){self.locked=crate::mobile::locked();self.lock_checked=Instant::now();}
        if let Some(w)=&self.worker{if !self.enabled()||Some(w.config.profile.as_str())!=profile||w.config!=self.config{w.cancel.store(true,Ordering::Relaxed);}}
        if let Some(rx)=&self.discovery{match rx.try_recv(){
            Ok(Ok(Discovery::Bots(owner,bots)))=>{if profile==Some(owner.as_str()){self.bots=bots;self.bots_profile=Some(owner);self.note="机器人列表已刷新，请选择通知机器人".into();self.error=false;}else{self.bots.clear();self.note="账号已变化，请重新刷新机器人".into();}self.discovery=None;},
            Ok(Err(e))=>{self.note=e;self.error=true;self.discovery=None;},
            Err(mpsc::TryRecvError::Disconnected)=>{self.note="机器人列表查询已中断".into();self.error=true;self.discovery=None;},_=>{},
        }}
        let mut stopped=false;
        if let Some(w)=&self.worker{loop{match w.rx.try_recv(){Ok(Ok(note))=>{self.note=note;self.error=false;},Ok(Err(e))=>{self.note=e;self.error=true;},Err(mpsc::TryRecvError::Disconnected)=>{stopped=true;break;},Err(mpsc::TryRecvError::Empty)=>break,}}}
        if stopped{let expected=self.worker.as_ref().is_some_and(|w|w.cancel.load(Ordering::Relaxed));self.worker=None;self.worker_stopped(expected);}
        if !self.enabled()||self.paused.as_ref().is_some_and(|c|c!=&self.config){self.paused=None;}
        if self.enabled()&&self.worker.is_none()&&self.paused.is_none()&&Some(self.config.profile.as_str())==profile&&!cfg!(test){self.start(ctx,tasks);}
        if self.worker.is_some()||self.discovery.is_some(){ctx.request_repaint_after(Duration::from_secs(2));}
    }
    fn worker_stopped(&mut self,expected:bool){
        if !expected&&self.config.enabled{
            crate::dingtalk_diagnostics::local("worker","worker","paused","NOTIFICATION_WORKER_PAUSED",0,"");
            self.paused=Some(self.config.clone());
            if !self.error{self.note="通知监听异常暂停，开关已保留；请关闭后重新开启".into();}
            self.error=true;
        }
    }
    fn start(&mut self,ctx:&egui::Context,tasks:&[Task]){
        let config=self.config.clone();let fallback=tasks.to_vec();let cancel=Arc::new(AtomicBool::new(false));let flag=cancel.clone();
        let (tx,rx)=mpsc::sync_channel(4);let ctx=ctx.clone();
        let spawned=std::thread::Builder::new().name("daozai-bot-notify".into()).spawn(move||{
            let mut ledger=match read::<Ledger>("dingtalk-bot-ledger.json"){Ok(v)=>v,Err(e)=>{let _=tx.try_send(Err(e));return;}};
            let mut observed=BTreeMap::new();let mut primed=false;
            let mut backlog=BTreeMap::new();
            while !flag.load(Ordering::Relaxed){
                if current_profile().as_deref()!=Some(config.profile.as_str()){break;}
                let tasks=crate::mobile::background_tasks(&fallback);
                let allowed=!config.only_locked||crate::mobile::locked()==Some(true);
                if allowed{for task in candidates(&tasks,&observed,primed){backlog.insert(event(&config,&task),task);}}
                observed=tasks.iter().map(|t|(key(t),revision(t))).collect();primed=true;
                if allowed{
                    let keys:Vec<_>=backlog.keys().take(2).cloned().collect();
                    for id in keys{
                        if flag.load(Ordering::Relaxed)||current_profile().as_deref()!=Some(config.profile.as_str()){break;}
                        let Some(task)=backlog.remove(&id) else{continue;};
                        if observed.get(&key(&task))!=Some(&revision(&task)){continue;}
                        let mut binding_warning=None;let mut reply_bound=false;
                        let result=deliver(&mut ledger,id,|l|write("dingtalk-bot-ledger.json",l),||{
                            let body=text(&task,config.summary);
                            let code=crate::dingtalk_reply::prepare(&config,&task,&body)?;
                            // A failed preflight must not suppress a notification or trigger a resend.
                            let probe=code.as_ref().map(|_|crate::dingtalk_binding::prepare(&config,&flag));
                            let delivery=send(&config,&body,&flag)?;
                            if let Some(code)=code{
                                crate::dingtalk_reply::activate(&code,&delivery)?;
                                let binding=probe.unwrap().and_then(|probe|crate::dingtalk_binding::confirm(&probe,&config,&delivery,&body,&flag))
                                    .and_then(|id|crate::dingtalk_reply::bind_message(&code,&config,&delivery,id));
                                crate::dingtalk_diagnostics::local("notify","binding",if binding.is_ok(){"success"}else{"failure"},if binding.is_ok(){"MESSAGE_ID_BOUND"}else{"MESSAGE_ID_UNCONFIRMED"},0,"");
                                match binding{Ok(())=>reply_bound=true,Err(note)=>{crate::dingtalk_reply::binding_failed(&code,&note);binding_warning=Some(note);}}
                            }
                            Ok(())
                        });
                        if matches!(result,Ok(false)){continue;}
                        let _=tx.try_send(result.and_then(|_|if let Some(note)=binding_warning{Err(format!("通知已送达，但引用消息 ID 尚未绑定：{note}。可发送/tasks使用备用指令"))}else{Ok(format!("{}：通知已被钉钉接收{}",task.title.chars().take(60).collect::<String>(),if reply_bound{"，引用消息 ID 已绑定"}else{""}))}));ctx.request_repaint();
                    }
                }
                for _ in 0..15{if flag.load(Ordering::Relaxed){break;}std::thread::sleep(Duration::from_secs(1));}
            }
        });
        match spawned{Ok(_)=>{self.worker=Some(Worker{cancel,rx,config:self.config.clone()});self.note="机器人通知监听中；不补发历史任务".into();},Err(_)=>{crate::dingtalk_diagnostics::local("worker","worker","paused","THREAD_START_FAILED",0,"");self.paused=Some(self.config.clone());self.note="通知监听启动失败，开关已保留；请关闭后重新开启".into();self.error=true;}}
    }
    fn discover(&mut self,ctx:&egui::Context,profile:&str){
        if self.discovery.is_some(){return;}
        let profile=profile.to_owned();let (tx,rx)=mpsc::channel();let ctx=ctx.clone();
        std::thread::spawn(move||{
            let result=(||{
                let cli=crate::dingtalk_ui::client().ok_or("未找到钉钉官方组件")?;
                let mut bots=Vec::new();
                for page in 1..=10{
                    let p=page.to_string();let v=crate::dingtalk_ui::run(&cli,&["chat","bot","search","--page",&p,"--size","20","--profile",&profile,"--format","json"],Duration::from_secs(30),&AtomicBool::new(false))?;
                    if v["success"]!=true{return Err("机器人查询未成功，请检查权限".into());}
                    let rows=v["robotList"].as_array().ok_or("无法读取机器人列表")?;
                    for row in rows{if let (Some(code),Some(name))=(row["robotCode"].as_str(),row["robotName"].as_str()){bots.push(Bot{code:code.into(),name:name.into()});}}
                    if rows.len()<20{return Ok(Discovery::Bots(profile,bots));}
                }
                Err("机器人列表过长，未加载完整，请核对后重试".into())
            })();let _=tx.send(result);ctx.request_repaint();
        });self.discovery=Some(rx);
    }
    pub(crate) fn ui(&mut self,ui:&mut egui::Ui,profile:Option<&str>){
        use crate::extras::{settings_card,settings_hint,settings_label};
        settings_label(ui,"应用机器人通知");
        settings_card(ui,|ui|{
            ui.label(format!("电脑状态：{}",match self.locked{Some(true)=>"已锁屏",Some(false)=>"未锁屏",_=>"正在检查"}));
            let before=self.config.clone();
            ui.add_enabled_ui(profile.is_some()&&self.valid,|ui|{
                if ui.add_enabled(self.discovery.is_none(),egui::Button::new("刷新我的机器人")).clicked(){self.discover(ui.ctx(),profile.unwrap());}
                egui::ComboBox::from_id_salt("notify-robot").selected_text(if self.config.name.is_empty(){"选择应用机器人"}else{&self.config.name}).width(ui.available_width()-20.0).show_ui(ui,|ui|{
                    for bot in &self.bots{if ui.selectable_label(self.config.robot==bot.code,&bot.name).clicked(){self.config.robot=bot.code.clone();self.config.name=bot.name.clone();self.config.profile=profile.unwrap().into();self.config.enabled=false;}}
                });
                ui.add_enabled_ui(!self.config.robot.is_empty()&&Some(self.config.profile.as_str())==profile,|ui|{
                    ui.checkbox(&mut self.config.enabled,"通过机器人通知我本人");
                    ui.checkbox(&mut self.config.only_locked,"仅电脑锁屏时通知");
                    ui.checkbox(&mut self.config.summary,"通知包含回复内容（最多 1500 字）");
                    ui.checkbox(&mut self.config.replies,"允许我回复机器人，继续 Codex 原任务");
                });
                if !self.config.profile.is_empty()&&Some(self.config.profile.as_str())!=profile{ui.colored_label(ui.visuals().error_fg_color,"账号已变更，请刷新列表并重新选择机器人。");}
            });
            if before!=self.config{if let Err(e)=write("dingtalk-bot-settings.json",&self.config){self.config=before;self.note=e;self.error=true;}else{
                if before.robot!=self.config.robot{crate::dingtalk_diagnostics::local("worker","worker","auto_disabled","ROBOT_CHANGED",0,"");}
                else if before.enabled!=self.config.enabled{crate::dingtalk_diagnostics::local("worker","worker",if self.config.enabled{"start"}else{"stopped"},if self.config.enabled{"USER_ENABLED_NOTIFICATIONS"}else{"USER_DISABLED_NOTIFICATIONS"},0,"");}
                if before.replies!=self.config.replies{crate::dingtalk_diagnostics::local("receive","worker",if self.config.replies{"start"}else{"stopped"},if self.config.replies{"USER_ENABLED_REPLIES"}else{"USER_DISABLED_REPLIES"},0,"");}
            }}
            if self.discovery.is_some(){ui.spinner();}
            if !self.note.is_empty(){ui.colored_label(if self.error{ui.visuals().error_fg_color}else{crate::theme::GREEN},&self.note);}
            ui.horizontal_wrapped(|ui|{ui.colored_label(crate::theme::GREEN,"● 已完成");ui.colored_label(ui.visuals().error_fg_color,"● 执行失败");ui.colored_label(egui::Color32::from_rgb(230,150,40),"● 需要确认");});
            settings_hint(ui,"使用选定的应用机器人，单聊通知当前登录账号本人。默认关闭；不补发历史任务。启用表示允许发送后续任务通知。旧待办交互已停用，不再创建待办或读取评论。");
            self.replies.ui(ui);
            settings_hint(ui,"开启回复后，由收发组件接收当前账号本人的机器人单聊；群消息不接入。请使用专供岛仔的机器人，避免与其他设备的接收端分流。普通消息默认回复上一条任务通知；引用消息回复指定任务。以 / 开头仅执行命令。也可发送“/tasks”获取备用指令。仅支持 Codex 原任务；消息排队不代表执行，不代替审批。通知与回复记录仅保存在本机，不参与云同步。");
        });
    }
    #[cfg(test)]pub(crate) fn preview(&mut self){self.config=Config{enabled:true,only_locked:true,profile:"demo:user".into(),robot:"demoBot".into(),name:"示例机器人".into(),summary:false,replies:true};self.locked=Some(false);self.note="示例任务：通知已被钉钉接收（合成预览）".into();if std::env::var_os("DAOZAI_RECONNECT_PREVIEW").is_some(){self.replies.preview_failed();}}
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn dingtalk_notification_worker_failure_keeps_user_switch_and_saved_configuration(){
        let _guard=crate::ui_tests::ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        let root=std::env::temp_dir().join(format!("daozai-notify-switch-{}-{}",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap()));std::env::set_var("DAOZAI_DATA_DIR",&root);
        let mut state=State::default();state.config=Config{enabled:true,profile:"corp:me".into(),robot:"bot".into(),..Default::default()};write("dingtalk-bot-settings.json",&state.config).unwrap();
        let before=std::fs::read(root.join("dingtalk-bot-settings.json")).unwrap();
        state.worker_stopped(false);assert!(state.config.enabled);assert!(state.paused.is_some());
        assert_eq!(std::fs::read(root.join("dingtalk-bot-settings.json")).unwrap(),before);assert!(read_config().unwrap().enabled);
        state.paused=None;state.worker_stopped(true);assert!(state.paused.is_none());assert!(state.config.enabled);
    }
    #[test]fn bot_notification_failure_does_not_prevent_a_different_new_notification(){
        let mut ledger=Ledger::default();
        assert!(deliver(&mut ledger,"failed-event".into(),|_|Ok(()),||Err("uncertain send".into())).is_err());
        assert!(deliver(&mut ledger,"new-event".into(),|_|Ok(()),||Ok(())).unwrap());
        assert!(!deliver(&mut ledger,"failed-event".into(),|_|panic!("must not rewrite"),||panic!("must not replay")).unwrap());
    }
    #[test]fn dingtalk_reply_only_mode_checks_authorization_after_restart(){
        let _guard=crate::ui_tests::ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        let root=std::env::temp_dir().join(format!("daozai-reply-auth-{}-{}",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap()));std::env::set_var("DAOZAI_DATA_DIR",root);
        let mut state=State::default();state.config=Config{enabled:false,replies:true,robot:"bot".into(),profile:"corp:me".into(),..Default::default()};
        assert!(state.needs_authorization());assert!(!state.enabled());
        state.config.replies=false;assert!(!state.needs_authorization());
        state.config.enabled=true;assert!(state.needs_authorization());
        state.config.robot.clear();assert!(!state.needs_authorization());
    }
    fn task()->Task{Task{provider:crate::ai_assistant::Provider::Codex,id:"demo".into(),title:"修复连接问题".into(),status:Status::Completed,updated:1,owned:false}}
    #[test]fn bot_notifications_skip_history_and_only_alert_on_new_events(){let mut t=task();let seen=BTreeMap::from([(key(&t),revision(&t))]);assert!(candidates(&[t.clone()],&BTreeMap::new(),false).is_empty());assert!(candidates(&[t.clone()],&seen,true).is_empty());t.updated=2;assert_eq!(candidates(&[t.clone()],&seen,true).len(),1);t.status=Status::Running;assert!(candidates(&[t],&seen,true).is_empty());}
    #[test]fn bot_notifications_are_bound_to_robot_and_profile(){let t=task();let a=Config{profile:"corp:me".into(),robot:"robotA".into(),..Default::default()};let mut b=a.clone();b.robot="robotB".into();assert_ne!(event(&a,&t),event(&b,&t));b=a.clone();b.profile="other:me".into();assert_ne!(event(&a,&t),event(&b,&t));}
    #[test]fn bot_notifications_do_not_include_reply_codes_or_conversation_history(){let body=text(&task(),false);assert_eq!(body,"未开启回复内容显示");assert!(!body.contains("来源："));assert!(!body.contains("修复连接问题"));}
    #[test]fn bot_notifications_unknown_send_is_not_replayed_after_restart(){
        use std::cell::RefCell;
        let disk=RefCell::new(Ledger::default());let mut ledger=Ledger::default();
        let result=deliver(&mut ledger,"event".into(),|l|{*disk.borrow_mut()=l.clone();Ok(())},||{assert!(disk.borrow().sent.contains_key("event"));Err("timeout".into())});assert!(result.is_err());
        let bytes=serde_json::to_vec(&*disk.borrow()).unwrap();let mut restart:Ledger=serde_json::from_slice(&bytes).unwrap();
        assert!(!deliver(&mut restart,"event".into(),|_|panic!("must not rewrite"),||panic!("must not resend")).unwrap());
    }
    #[test]fn bot_notifications_never_send_if_journal_cannot_be_saved(){let mut ledger=Ledger::default();assert!(deliver(&mut ledger,"event".into(),|_|Err("disk full".into()),||panic!("must not send")).is_err());}
}
