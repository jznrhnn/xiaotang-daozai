//! Shared phone-interaction helpers; no Todo transport or legacy settings state.
use crate::ai_assistant::Task;
use eframe::egui;
use serde_json::Value;
use std::{collections::BTreeMap,path::PathBuf};
const STORE_LIMIT:usize=2*1024*1024;
fn file(name:&str)->PathBuf{crate::model::config_dir().join(name)}
fn key(task:&Task)->String{format!("{}:{}",task.provider.name(),task.id)}
fn uuid(value:&str)->bool{value.len()==36&&value.bytes().enumerate().all(|(i,b)|if [8,13,18,23].contains(&i){b==b'-'}else{b.is_ascii_hexdigit()})}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn mobile_queue_keeps_user_content_literal_and_requires_original_uuid(){let id="00000000-0000-0000-0000-000000000001";let text="--yolo; echo no";assert_eq!(queue_arguments(id,text).unwrap(),["queue",&format!("--thread={id}"),&format!("--message={text}")]);assert!(queue_arguments("任务名称",text).is_err());assert!(queue_arguments(id,"").is_err());}
}
pub(crate) fn queue_arguments(thread:&str,text:&str)->Result<[String;3],String>{
    if !uuid(thread)||text.trim().is_empty()||text.len()>16000{return Err("原会话编号或评论内容无效".into());}
    Ok(["queue".into(),format!("--thread={thread}"),format!("--message={text}")])
}
#[cfg(windows)]
pub(crate) fn locked()->Option<bool>{
    use windows::{core::PWSTR,Win32::{Foundation::HANDLE,System::RemoteDesktop::*}};
    let mut buffer=PWSTR::null();let mut size=0u32;
    unsafe {
        if WTSQuerySessionInformationW(HANDLE::default(),WTS_CURRENT_SESSION,WTSSessionInfoEx,&mut buffer,&mut size).is_err(){return None;}
        let value=if !buffer.is_null()&&size as usize>=std::mem::size_of::<WTSINFOEXW>(){
            let info=&*buffer.0.cast::<WTSINFOEXW>();
            if info.Level==1 {match info.Data.WTSInfoExLevel1.SessionFlags {0=>Some(true),1=>Some(false),_=>None}}else{None}
        }else{None};
        WTSFreeMemory(buffer.0.cast());value
    }
}
#[cfg(not(windows))]
pub(crate) fn locked()->Option<bool>{None}

pub(crate) fn background_tasks(fallback:&[Task])->Vec<Task>{
    let mut tasks:BTreeMap<String,Task>=fallback.iter().map(|t|(key(t),t.clone())).collect();
    for name in ["ai-task-inbox.json","ai-monitor/phone-tasks.json"]{
        let Ok(bytes)=std::fs::read(file(name)) else{continue;};if bytes.len()>STORE_LIMIT{continue;}
        let Ok(value)=serde_json::from_slice::<Value>(&bytes) else{continue;};
        for row in value.as_array().into_iter().flatten(){
            let row=row.get("task").unwrap_or(row);
            let Ok(task)=serde_json::from_value::<Task>(row.clone()) else{continue;};
            if !uuid(&task.id)||task.title.len()>4096{continue;}
            if tasks.get(&key(&task)).is_none_or(|old|old.updated<=task.updated){tasks.insert(key(&task),task);}
        }
    }
    tasks.into_values().collect()
}
impl crate::app::App{
    pub(crate) fn poll_mobile(&mut self,ctx:&egui::Context){
        self.dingtalk.poll(ctx);
        if self.dingtalk_bot.needs_authorization()&&!cfg!(test){self.dingtalk.ensure_checked(ctx);}
        let tasks=if self.dingtalk_bot.needs_task_snapshot()&&self.dingtalk.account_profile().is_some(){self.ai_assistant.phone_tasks()}else{Vec::new()};
        self.dingtalk_bot.tick(ctx,self.dingtalk.account_profile(),&tasks);
    }
    pub(crate) fn mobile_ui(&mut self,ui:&mut egui::Ui){
        self.dingtalk.ui(ui);ui.add_space(8.0);
        if self.dingtalk.component_missing(){return;}
        self.dingtalk_bot.ui(ui,self.dingtalk.account_profile());
    }
}
