//! Confirm a newly sent notification once, then use its platform message ID for replies.
use crate::dingtalk_bot::Config;
use serde::{Deserialize,Serialize};
use serde_json::Value;
use std::{collections::BTreeSet,path::Path,sync::atomic::{AtomicBool,Ordering},time::Duration};

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq,PartialOrd,Ord)]
pub(crate) struct MessageId{pub conversation:String,pub message:String}
fn valid_id(value:&str)->bool{!value.is_empty()&&value.len()<=2048&&!value.chars().any(char::is_control)}
impl MessageId{
    pub(crate) fn from_row(value:&Value)->Option<Self>{
        let conversation=value["conversationId"].as_str()?;
        let message=value["messageId"].as_str().or_else(||value["openMessageId"].as_str())?;
        (valid_id(conversation)&&valid_id(message)).then(||Self{conversation:conversation.into(),message:message.into()})
    }
}
#[derive(Clone,Serialize,Deserialize)]
pub(crate) struct Delivery{pub receipt:String,pub started:i64,pub confirmed:i64}
pub(crate) struct Probe{profile:String,robot:String,target:String,known:BTreeSet<MessageId>}

pub(crate) fn message_time(value:&Value)->Option<i64>{
    let date=value["createTime"].as_str()?;
    chrono::DateTime::parse_from_rfc3339(date).ok().map(|v|v.timestamp()).or_else(||
        chrono::NaiveDateTime::parse_from_str(date,"%Y-%m-%d %H:%M:%S").ok()?.and_local_timezone(chrono_tz::Asia::Shanghai).single().map(|v|v.timestamp()))
}
fn rows(value:&Value)->Result<&Vec<Value>,String>{
    if value["complete"]!=true||value["hasMore"]==true||value["partial"]==true||value["truncated"]==true||value["failedCount"].as_u64().is_some_and(|v|v>0){return Err("消息记录未读取完整，未绑定消息 ID".into());}
    value["messages"].as_array().ok_or_else(||"无法读取消息记录，未绑定消息 ID".into())
}
fn run(cli:&Path,args:&[&str],cancel:&AtomicBool)->Result<Value,String>{
    if cancel.load(Ordering::Relaxed){return Err("已取消消息 ID 确认".into());}
    crate::dingtalk_ui::run(cli,args,Duration::from_secs(15),cancel)
}
fn target(cli:&Path,c:&Config,cancel:&AtomicBool)->Result<String,String>{
    if c.name.trim().is_empty(){return Err("机器人名称为空，未绑定消息 ID".into());}
    let value=run(cli,&["chat","bot","find","--query",&c.name,"--limit","20","--profile",&c.profile,"--format","json"],cancel)?;
    select_target(&value,&c.name)
}
fn select_target(value:&Value,name:&str)->Result<String,String>{
    if value["success"]!=true||(!value["errorCode"].is_null()&&value["errorCode"].as_i64()!=Some(0))||value["result"]["hasMore"]!=false{return Err("机器人目录未完整确认，未绑定消息 ID".into());}
    let bots=value["result"]["bots"].as_array().ok_or("机器人目录格式无效")?;
    let matches=bots.iter().filter(|row|row["name"].as_str()==Some(name)).collect::<Vec<_>>();
    if matches.len()!=1{return Err("机器人名称未唯一确认，请检查是否重名".into());}
    matches[0]["botOpenDingTalkId"].as_str().filter(|v|valid_id(v)).map(str::to_owned).ok_or_else(||"无法读取机器人身份，未绑定消息 ID".into())
}
fn read(cli:&Path,c:&Config,target:&str,start:i64,end:Option<i64>,cancel:&AtomicBool)->Result<Value,String>{
    let start=chrono::DateTime::from_timestamp(start,0).ok_or("通知时间无效")?.to_rfc3339();
    let end=end.map(|at|chrono::DateTime::from_timestamp(at,0).map(|v|v.to_rfc3339()).ok_or("通知时间无效")).transpose()?;
    let mut args=vec!["chat","+chat-messages","--open-dingtalk-id",target,"--start",&start,"--order","desc","--page-all","--page-limit","2","--max-items","100","--no-reactions","--profile",&c.profile,"--format","json"];
    if let Some(end)=&end{args.extend(["--end",end]);}
    run(cli,&args,cancel)
}
pub(crate) fn prepare(c:&Config,cancel:&AtomicBool)->Result<Probe,String>{
    let cli=crate::dingtalk_ui::client().ok_or("未找到钉钉组件")?;
    let target=target(&cli,c,cancel)?;
    let value=read(&cli,c,&target,chrono::Utc::now().timestamp()-120,None,cancel)?;
    let mut known=BTreeSet::new();
    for row in rows(&value)?{known.insert(MessageId::from_row(row).ok_or("已有消息缺少标识，未建立绑定基线")?);}
    Ok(Probe{profile:c.profile.clone(),robot:c.robot.clone(),target,known})
}
fn select(probe:&Probe,c:&Config,delivery:&Delivery,body:&str,value:&Value)->Result<Option<MessageId>,String>{
    if probe.profile!=c.profile||probe.robot!=c.robot||delivery.confirmed<delivery.started{return Err("账号、机器人或发送时间已变化，未绑定消息 ID".into());}
    let mut candidates=BTreeSet::new();
    for row in rows(value)?{
        if row["senderId"].as_str()!=Some(probe.target.as_str()){continue;}
        let Some(id)=MessageId::from_row(row) else{return Err("通知缺少消息标识，未绑定消息 ID".into());};
        if probe.known.contains(&id){continue;}
        let Some(at)=message_time(row) else{continue;};
        if at<delivery.started.saturating_sub(2)||at>delivery.confirmed.saturating_add(2){continue;}
        let Some(text)=row["text"].as_str().or_else(||row["content"].as_str()) else{continue;};
        if crate::dingtalk_reply::notification_text_matches(body,text){candidates.insert(id);}
    }
    match candidates.len(){0=>Ok(None),1=>Ok(candidates.into_iter().next()),_=>Err("发现多条候选通知，未猜测绑定消息 ID".into())}
}
pub(crate) fn confirm(probe:&Probe,c:&Config,delivery:&Delivery,body:&str,cancel:&AtomicBool)->Result<MessageId,String>{
    let cli=crate::dingtalk_ui::client().ok_or("未找到钉钉组件")?;
    for attempt in 0..4 {
        if attempt>0{
            for _ in 0..attempt*5{if cancel.load(Ordering::Relaxed){return Err("已取消消息 ID 确认".into());}std::thread::sleep(Duration::from_millis(100));}
        }
        let value=read(&cli,c,&probe.target,delivery.started.saturating_sub(2),Some(delivery.confirmed.saturating_add(3)),cancel)?;
        if let Some(id)=select(probe,c,delivery,body,&value)?{return Ok(id);}
    }
    Err("通知消息记录尚未唯一确认，消息 ID 未绑定；可发送/tasks使用备用指令".into())
}

#[cfg(test)]mod tests{
    use super::*;
    fn fixture()->(Config,Probe,Delivery){
        let c=Config{profile:"corp:me".into(),robot:"robot".into(),name:"测试机器人".into(),..Default::default()};
        let probe=Probe{profile:c.profile.clone(),robot:c.robot.clone(),target:"bot-user".into(),known:BTreeSet::new()};
        (c,probe,Delivery{receipt:"send-receipt-is-not-message-id".into(),started:100,confirmed:105})
    }
    fn row(id:&str,body:&str)->Value{serde_json::json!({"conversationId":"cid","messageId":id,"senderId":"bot-user","createTime":"1970-01-01T00:01:42Z","text":body})}
    fn response(messages:Vec<Value>)->Value{serde_json::json!({"complete":true,"hasMore":false,"messages":messages})}
    #[test]fn bot_binding_directory_requires_one_exact_complete_identity(){
        let v=serde_json::json!({"success":true,"errorCode":0,"result":{"hasMore":false,"bots":[{"name":"测试机器人","botOpenDingTalkId":"bot-user"},{"name":"其他","botOpenDingTalkId":"other"}]}});
        assert_eq!(select_target(&v,"测试机器人").unwrap(),"bot-user");assert!(select_target(&v,"不存在").is_err());
        let mut no_error=v.clone();no_error["errorCode"]=Value::Null;assert_eq!(select_target(&no_error,"测试机器人").unwrap(),"bot-user");
        no_error["errorCode"]=serde_json::json!(403);assert!(select_target(&no_error,"测试机器人").is_err());
        let mut duplicate=v.clone();duplicate["result"]["bots"].as_array_mut().unwrap().push(v["result"]["bots"][0].clone());assert!(select_target(&duplicate,"测试机器人").is_err());
        let mut partial=v;partial["result"]["hasMore"]=Value::Bool(true);assert!(select_target(&partial,"测试机器人").is_err());
    }
    #[test]fn bot_binding_does_not_bind_a_preexisting_identical_message(){
        let(c,mut probe,delivery)=fixture();let old=row("old","结果相同");probe.known.insert(MessageId::from_row(&old).unwrap());
        assert!(select(&probe,&c,&delivery,"结果相同",&response(vec![old.clone()])).unwrap().is_none());
        let selected=select(&probe,&c,&delivery,"结果相同",&response(vec![old,row("new","结果相同")])).unwrap().unwrap();
        assert_eq!(selected.message,"new");assert_ne!(selected.message,delivery.receipt);
    }
    #[test]fn bot_binding_requires_unique_new_message_and_correct_sender_time_and_scope(){
        let(c,probe,delivery)=fixture();
        assert!(select(&probe,&c,&delivery,"完成",&response(vec![row("one","完成"),row("two","完成")])).is_err());
        let mut foreign=row("foreign","完成");foreign["senderId"]=Value::String("another-bot".into());
        let mut old=row("old","完成");old["createTime"]=Value::String("1970-01-01T00:00:01Z".into());
        assert!(select(&probe,&c,&delivery,"完成",&response(vec![foreign,old])).unwrap().is_none());
        let mut other=c.clone();other.profile="another:me".into();assert!(select(&probe,&other,&delivery,"完成",&response(vec![row("new","完成")])).is_err());
        other=c;other.robot="other-robot".into();assert!(select(&probe,&other,&delivery,"完成",&response(vec![row("new","完成")])).is_err());
    }
    #[test]fn bot_binding_never_accepts_incomplete_reads_or_missing_message_ids(){
        let(c,probe,delivery)=fixture();let mut partial=response(vec![row("new","完成")]);partial["complete"]=Value::Bool(false);
        assert!(select(&probe,&c,&delivery,"完成",&partial).is_err());
        let mut missing=row("new","完成");missing.as_object_mut().unwrap().remove("messageId");
        assert!(select(&probe,&c,&delivery,"完成",&response(vec![missing])).is_err());
        let mut failed=response(vec![row("new","完成")]);failed["failedCount"]=serde_json::json!(1);
        assert!(select(&probe,&c,&delivery,"完成",&failed).is_err());
    }
    #[test]fn bot_binding_matches_rendered_markdown_only_during_initial_confirmation(){
        let(c,probe,delivery)=fixture();let original="**完成**\n| 项目 | 数值 |\n|---|---:|\n| A | 10 |";
        let rendered="完成\n| 项目 | 数值 |\n| --- | --- |\n| A | 10 |";
        assert_eq!(select(&probe,&c,&delivery,original,&response(vec![row("actual-id",rendered)])).unwrap().unwrap().message,"actual-id");
        assert!(select(&probe,&c,&delivery,original,&response(vec![row("wrong-value",&rendered.replace("10","11"))])).unwrap().is_none());
    }
}
