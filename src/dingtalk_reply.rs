//! Standalone DingTalk Stream callbacks route literal input to the original task.
use crate::{ai_assistant::{Provider,Task},dingtalk_bot::Config};
use serde::{Deserialize,Serialize};
use serde_json::Value;
use std::{collections::BTreeMap,fs::{File,OpenOptions},sync::{Arc,atomic::{AtomicBool,Ordering},mpsc},time::Duration};

#[derive(Clone,Serialize,Deserialize)]
struct Route{profile:String,robot:String,task:Task,turn:String,created:i64,active:bool,used:bool,#[serde(default)]body:String,
    #[serde(default)]delivery:Option<crate::dingtalk_binding::Delivery>,
    #[serde(default)]message_id:Option<crate::dingtalk_binding::MessageId>,#[serde(default)]binding_error:String}
#[derive(Default,Serialize,Deserialize)]
struct Routes{items:BTreeMap<String,Route>}
fn file(name:&str)->std::path::PathBuf{crate::model::config_dir().join(name)}
fn lock()->Result<File,String>{
    std::fs::create_dir_all(crate::model::config_dir()).map_err(|_|"无法保存回复关联")?;
    let mut options=OpenOptions::new();options.read(true).write(true).create(true);
    #[cfg(windows)]{use std::os::windows::fs::OpenOptionsExt;options.share_mode(0);}
    #[cfg(not(windows))]return Err("当前回复连接仅支持 Windows".into());
    options.open(file("dingtalk-reply.lock")).map_err(|_|"正在处理另一条回复，请稍后再试".into())
}
fn load()->Result<Routes,String>{
    match std::fs::read(file("dingtalk-reply-routes.json")){
        Ok(bytes) if bytes.len()<=2*1024*1024=>serde_json::from_slice(&bytes).map_err(|_|"回复关联记录损坏，已停止发送".into()),
        Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(Routes::default()),
        _=>Err("无法读取回复关联记录".into()),
    }
}
fn save(routes:&Routes)->Result<(),String>{
    let bytes=serde_json::to_vec(routes).map_err(|_|"无法保存回复关联")?;
    if bytes.len()>2*1024*1024{return Err("回复关联记录已满".into());}
    crate::model::atomic_write(&file("dingtalk-reply-routes.json"),&bytes).map_err(|_|"无法保存回复记录，已停止发送".into())
}
fn same_task(a:&Task,b:&Task)->bool{a.provider==b.provider&&a.id==b.id&&a.updated==b.updated&&a.status==b.status}
fn fresh(r:&Route,now:i64)->bool{r.active&&!r.used&&now>=r.created&&now-r.created<86400}
pub(crate) fn prepare(c:&Config,t:&Task,body:&str)->Result<Option<String>,String>{
    if !c.replies||t.provider!=Provider::Codex{return Ok(None);}
    let Some(turn)=crate::ai_monitor::current_codex_turn(&t.id) else{return Ok(None);};
    let _guard=lock()?;let mut routes=load()?;let now=chrono::Utc::now().timestamp();
    routes.items.retain(|_,r|now-r.created<86400);
    if routes.items.len()>=128{return Err("今日回复关联已满，请稍后再试".into());}
    use ring::rand::{SecureRandom,SystemRandom};
    let mut bytes=[0u8;6];SystemRandom::new().fill(&mut bytes).map_err(|_|"无法生成任务回复标识")?;
    let code=bytes.iter().map(|b|format!("{b:02X}")).collect::<String>();
    // Only the latest notification for a task remains actionable.
    for r in routes.items.values_mut().filter(|r|r.profile==c.profile&&r.robot==c.robot&&r.task.provider==t.provider&&r.task.id==t.id){r.active=false;}
    routes.items.insert(code.clone(),Route{profile:c.profile.clone(),robot:c.robot.clone(),task:t.clone(),turn,created:now,active:false,used:false,body:body.into(),delivery:None,message_id:None,binding_error:String::new()});save(&routes)?;
    Ok(Some(code))
}
pub(crate) fn activate(code:&str,delivery:&crate::dingtalk_binding::Delivery)->Result<(),String>{
    let _guard=lock()?;let mut routes=load()?;let r=routes.items.get_mut(code).ok_or("回复关联不存在")?;
    r.active=true;r.delivery=Some(delivery.clone());save(&routes)
}
pub(crate) fn bind_message(code:&str,c:&Config,delivery:&crate::dingtalk_binding::Delivery,id:crate::dingtalk_binding::MessageId)->Result<(),String>{
    let _guard=lock()?;let mut routes=load()?;
    let r=routes.items.get(code).ok_or("回复关联不存在")?;
    if r.profile!=c.profile||r.robot!=c.robot||r.delivery.as_ref().is_none_or(|d|d.receipt!=delivery.receipt){return Err("通知关联已变化，未保存消息 ID".into());}
    if routes.items.iter().any(|(other,r)|other!=code&&r.profile==c.profile&&r.robot==c.robot&&r.message_id.as_ref()==Some(&id)){return Err("消息 ID 已关联其他通知，未重复绑定".into());}
    let r=routes.items.get_mut(code).unwrap();r.message_id=Some(id);r.binding_error.clear();save(&routes)
}
pub(crate) fn binding_failed(code:&str,note:&str){
    let Ok(_guard)=lock() else{return;};let Ok(mut routes)=load() else{return;};
    if let Some(r)=routes.items.get_mut(code){r.binding_error=note.chars().take(512).collect();let _=save(&routes);}
}
fn execute(routes:&mut Routes,profile:&str,robot:&str,input:&str,tasks:&[Task],turn:impl Fn(&str)->Option<String>,mut persist:impl FnMut(&Routes)->Result<(),String>,queue:impl FnOnce(&str,&str)->Result<(),String>)->Result<String,String>{
    let now=chrono::Utc::now().timestamp();
    let input=input.trim();
    if input=="/help"{return Ok("- /tasks：查看任务\n- /tasks 2：下一页\n- 普通消息：回复上一条任务通知\n- 引用消息：回复指定任务".into());}
    if input=="/tasks"||input.starts_with("/tasks "){
        let page=if input=="/tasks"{1}else{input.strip_prefix("/tasks ").unwrap().trim().parse::<usize>().ok().filter(|p|*p>0).ok_or("请发送：/tasks，或/tasks 2")?};
        let mut entries=routes.items.iter().filter(|(_,r)|r.profile==profile&&r.robot==robot&&fresh(r,now)&&tasks.iter().any(|t|same_task(t,&r.task))).collect::<Vec<_>>();
        entries.sort_by(|(ac,a),(bc,b)|b.task.updated.cmp(&a.task.updated).then(b.created.cmp(&a.created)).then(ac.cmp(bc)));
        if entries.is_empty(){return Ok("暂无可回复任务".into());}
        let pages=entries.len().div_ceil(5);
        if page>pages{return Err(format!("共 {pages} 页，请发送：/tasks {pages}"));}
        let list=entries.into_iter().skip((page-1)*5).take(5).map(|(code,r)|{
            let title=r.task.title.split_whitespace().collect::<Vec<_>>().join(" ");
            let short=title.chars().take(24).collect::<String>();let suffix=if title.chars().count()>24{"…"}else{""};
            format!("- {short}{suffix} · {code}")
        }).collect::<Vec<_>>().join("\n");
        let next=if page<pages{format!("\n下一页：/tasks {}",page+1)}else{String::new()};
        return Ok(format!("可回复任务 {page}/{pages}\n\n{list}\n\n回复：继续 标识 你的要求{next}"));
    }
    if input.starts_with('/'){return Err("未知命令。发送 /tasks 查看任务，/help 查看帮助".into());}
    let input=input.trim();let rest=input.strip_prefix("继续 ").ok_or("请回复上一条任务通知，或引用指定通知；也可发送 /tasks 选择任务。")?;
    let (code,message)=rest.split_once(char::is_whitespace).ok_or("请在任务标识后填写你的要求")?;let message=message.trim();
    if message.is_empty()||message.len()>16000{return Err("回复不能为空，且不能超过 16000 字节".into());}
    let r=routes.items.get(code).ok_or("任务标识不存在或已过期，请使用最新通知")?;
    if r.profile!=profile||r.robot!=robot||!fresh(r,now){return Err("此通知已处理、已过期或不属于当前账号和机器人，不会重复发送".into());}
    if !tasks.iter().any(|t|same_task(t,&r.task))||turn(&r.task.id).as_deref()!=Some(r.turn.as_str()){return Err("原任务已经更新，请使用最新通知；本次没有发送".into());}
    let id=r.task.id.clone();let title=r.task.title.clone();
    routes.items.get_mut(code).unwrap().used=true;persist(routes)?;
    queue(&id,message)?;
    Ok(format!("已排队发送到原任务「{title}」。\n这不代表已开始执行；原客户端需保持运行，原有审批仍需在原客户端处理。"))
}
fn message_text(v:&Value)->Option<&str>{v["text"].as_str().or_else(||v["content"].as_str())}
fn quoted_message<'a>(v:&'a Value,input:&str)->Result<&'a Value,String>{
    if v["complete"]!=true||v["hasMore"]==true{return Err("引用消息未读取完整，请稍后重试或使用任务标识".into());}
    let rows=v["messages"].as_array().ok_or("未读取到引用消息")?;
    let matches:Vec<_>=rows.iter().filter(|r|message_text(r).is_some_and(|s|s.trim()==input.trim())).collect();
    if matches.len()!=1{return Err("无法唯一确认你刚回复的消息；请引用通知并换一句明确的要求，或使用任务标识".into());}
    Ok(&matches[0]["quotedMessage"])
}
fn direct_command(routes:&Routes,c:&Config,input:&str,tasks:&[Task],turn:impl Fn(&str)->Option<String>,context:Option<(&str,i64)>)->Result<String,String>{
    let input=input.trim();
    if input.is_empty()||input.len()>16000{return Err("回复不能为空或过长".into());}
    if input.starts_with('/'){return Err("命令不能作为任务回复转发".into());}
    let now=chrono::Utc::now().timestamp();
    // Pick the preceding notification first, then validate it. Never fall back to an older task
    // when the latest notification has already been consumed or its task has changed.
    let mut previous=routes.items.iter().filter(|(_,r)|{
        r.profile==c.profile&&r.robot==c.robot&&r.task.provider==Provider::Codex&&r.active&&
        context.is_none_or(|(cid,at)|r.message_id.as_ref().is_none_or(|id|id.conversation==cid)&&r.delivery.as_ref().map_or(r.created,|d|d.confirmed)<=at)
    }).collect::<Vec<_>>();
    let order=|r:&Route|r.delivery.as_ref().map_or((r.created,r.created,r.created),|d|(d.confirmed,d.started,r.created));
    previous.sort_by(|(_,a),(_,b)|order(b).cmp(&order(a)));
    let Some((code,r))=previous.first() else{return Err("暂无可回复通知，请发送 /tasks 查看任务".into());};
    if previous.get(1).is_some_and(|(_,other)|order(other)==order(r)){return Err("上一条通知无法唯一区分，请引用通知回复，或发送 /tasks".into());}
    if !fresh(r,now)||!tasks.iter().any(|t|same_task(t,&r.task))||turn(&r.task.id).as_deref()!=Some(r.turn.as_str()){
        return Err("上一条通知已处理、过期或任务已更新，请引用其他通知或发送 /tasks".into());
    }
    Ok(format!("继续 {code} {input}"))
}
fn visible_text(text:&str)->String{
    // Quote rendering drops Markdown table alignment and changes delimiter spacing.
    // Normalize only delimiter rows; keep column count and every data cell unchanged.
    let table_text=text.lines().map(|line|{
        let trimmed=line.trim();
        if !trimmed.contains('|'){return line.to_owned();}
        let cells=trimmed.strip_prefix('|').unwrap_or(trimmed);
        let cells=cells.strip_suffix('|').unwrap_or(cells).split('|').collect::<Vec<_>>();
        let separator=cells.iter().all(|cell|{
            let cell=cell.trim();let cell=cell.strip_prefix(':').unwrap_or(cell);
            let cell=cell.strip_suffix(':').unwrap_or(cell);
            cell.len()>=3&&cell.bytes().all(|b|b==b'-')
        });
        if separator{format!("|{}|",vec!["---";cells.len()].join("|"))}else{line.to_owned()}
    }).collect::<Vec<_>>().join("\n");
    let links=regex::Regex::new(r"\[([^\]\n]*)\]\([^\n)]*\)").unwrap();
    let text=links.replace_all(&table_text,"$1");
    let lists=regex::Regex::new(r"(?m)^\s*(?:[-+*]\s+|[•●]\s*)").unwrap();
    let text=lists.replace_all(&text,"");
    text.chars().filter(|c|!c.is_whitespace()&&!matches!(c,'*'|'_'|'`')).collect()
}
pub(crate) fn notification_text_matches(saved:&str,rendered:&str)->bool{
    let saved=visible_text(saved);saved==visible_text(rendered)||saved==visible_text(&unescape_quoted_markdown(rendered))
}
fn unescape_quoted_markdown(text:&str)->String{
    static ESCAPES:std::sync::OnceLock<regex::Regex>=std::sync::OnceLock::new();
    // DingTalk escapes punctuation in rendered quotes, including underscores in file names.
    // Decode once on the quoted side only; stored text and literal Windows paths stay intact.
    ESCAPES.get_or_init(||regex::Regex::new(r"\\([[:punct:]])").unwrap()).replace_all(text,"$1").into_owned()
}
enum QuoteLookup{Ready(String),Retry(String),Reject(String)}
#[cfg(test)]
fn resolve_quote(v:&Value,input:&str,routes:&Routes,c:&Config)->Result<String,String>{
    match lookup_quote(v,input,routes,c){QuoteLookup::Ready(v)=>Ok(v),QuoteLookup::Retry(e)|QuoteLookup::Reject(e)=>Err(e)}
}
fn lookup_quote(v:&Value,input:&str,routes:&Routes,c:&Config)->QuoteLookup{
    let quote=match quoted_message(v,input){Ok(v)=>v,Err(e)=>return QuoteLookup::Reject(e)};
    // Only a confirmed non-quoted message may select the preceding notification.
    // A stale/invalid explicit quote must never silently target another task.
    if quote.is_null(){
        let row=v["messages"].as_array().and_then(|rows|rows.iter().find(|row|message_text(row).is_some_and(|text|text.trim()==input.trim())));
        let context=row.and_then(|row|Some((row["conversationId"].as_str()?,crate::dingtalk_binding::message_time(row)?)));
        let Some(context)=context else{return QuoteLookup::Retry("当前消息的会话或时间尚未就绪".into());};
        return match direct_command(routes,c,input,&crate::mobile::background_tasks(&[]),crate::ai_monitor::current_codex_turn,Some(context)){Ok(v)=>QuoteLookup::Ready(v),Err(e)=>QuoteLookup::Reject(e)};
    }
    if !quote.is_object(){return QuoteLookup::Retry("引用消息详情尚未就绪".into());}
    let mut identified=quote.clone();
    if identified["conversationId"].as_str().is_none(){
        if let Some(conversation)=v["messages"].as_array().and_then(|rows|rows.iter().find(|row|message_text(row).is_some_and(|text|text.trim()==input.trim()))).and_then(|row|row["conversationId"].as_str()){identified["conversationId"]=Value::String(conversation.into());}
    }
    // Stream callbacks expose the original proactive receipt directly. It is an
    // independent exact lookup key, never mislabeled as a platform message ID.
    if let Some(receipt)=quote["processQueryKey"].as_str().filter(|s|!s.is_empty()){
        let ids=routes.items.iter().filter(|(_,r)|r.profile==c.profile&&r.robot==c.robot&&r.delivery.as_ref().is_some_and(|d|d.receipt==receipt)&&r.message_id.as_ref().is_none_or(|id|identified["conversationId"].as_str()==Some(id.conversation.as_str()))).collect::<Vec<_>>();
        return if ids.len()==1{QuoteLookup::Ready(format!("继续 {} {}",ids[0].0,input.trim()))}else{QuoteLookup::Reject("引用通知未唯一关联原任务，请发送 /tasks 选择任务".into())};
    }
    if let Some(id)=crate::dingtalk_binding::MessageId::from_row(&identified){
        let ids=routes.items.iter().filter(|(_,r)|r.profile==c.profile&&r.robot==c.robot&&r.message_id.as_ref()==Some(&id)).collect::<Vec<_>>();
        if ids.len()==1{return QuoteLookup::Ready(format!("继续 {} {}",ids[0].0,input.trim()));}
        if ids.len()>1{return QuoteLookup::Reject("消息 ID 关联不唯一，未选择任务".into());}
    }
    QuoteLookup::Retry("此引用尚无对应的消息 ID 绑定；旧版通知不再支持引用继续，请发送“/tasks”选择任务".into())
}

fn receiving_enabled(c:&Config,profile:Option<&str>)->bool{
    c.replies&&!c.robot.trim().is_empty()&&!c.profile.is_empty()&&Some(c.profile.as_str())==profile
}
fn handle_event(event:&Value)->Result<String,String>{
    let at=std::time::Instant::now();
    let trace=crate::dingtalk_diagnostics::correlation(&serde_json::json!([event["conversation"],event["message"]]).to_string());
    let operation=if event["text"].as_str().is_some_and(|s|s.trim().starts_with('/')){"command"}else{"dispatch"};
    crate::dingtalk_diagnostics::local(operation,"routing","start","OK",0,&trace);
    let result=handle_event_inner(event);
    let code=match &result{
        Ok(_)=>"OK",Err(e) if e.starts_with("这条消息已处理")=>"DUPLICATE_MESSAGE",
        Err(e) if e.starts_with("此通知已处理")||e.starts_with("上一条通知已处理")=>"NOTIFICATION_NOT_ACTIONABLE",
        Err(e) if e.contains("暂无可回复")=>"NO_REPLY_TARGET",Err(e) if e.contains("未知命令")=>"UNKNOWN_COMMAND",
        Err(e) if e.contains("未唯一关联")||e.contains("无法唯一区分")=>"QUOTE_NOT_UNIQUE",
        Err(e) if e.contains("消息 ID 绑定")=>"QUOTE_NOT_BOUND",Err(e) if e.contains("过期")=>"EXPIRED",
        Err(e) if e.contains("未开启")||e.contains("已关闭")=>"REPLIES_INACTIVE",
        Err(e) if e.contains("更新")=>"TASK_CHANGED",Err(e) if e.contains("排队结果")=>"QUEUE_UNCONFIRMED",Err(_)=>"ROUTE_REJECTED"
    };
    crate::dingtalk_diagnostics::local(operation,"routing",if result.is_ok(){"success"}else{"rejected"},code,at.elapsed().as_millis() as u64,&trace);
    result
}
fn handle_event_inner(event:&Value)->Result<String,String>{
    let c=crate::dingtalk_bot::read_config()?;
    if !receiving_enabled(&c,crate::dingtalk_bot::current_profile().as_deref()){return Err("机器人回复接收未开启或账号已变化".into());}
    if std::env::var("DAOZAI_DINGTALK_BRIDGE").unwrap_or_default()!=format!("{}|{}",c.profile,c.robot)||crate::dingtalk_bot::current_profile().as_deref()!=Some(c.profile.as_str()){return Err("机器人或账号已变化，本次没有发送".into());}
    let input=event["text"].as_str().ok_or("消息格式无效")?.trim();
    let id=crate::dingtalk_binding::MessageId::from_row(&serde_json::json!({"conversationId":event["conversation"],"messageId":event["message"]})).ok_or("缺少消息标识")?;
    let at=event["at"].as_i64().filter(|at|at.abs_diff(chrono::Utc::now().timestamp())<=180).ok_or("接收消息已过期")?;
    let tasks=crate::mobile::background_tasks(&[]);
    let _guard=lock()?;let mut routes=load()?;
    let explicit=regex::Regex::new(r"^继续 [A-F0-9]{12}\s").unwrap().is_match(input);
    let resolved=if input.starts_with('/')||explicit{input.to_owned()}else{
        match lookup_quote(&event_payload(event),input,&routes,&c){QuoteLookup::Ready(v)=>v,QuoteLookup::Retry(e)|QuoteLookup::Reject(e)=>return Err(e)}
    };
    if !input.starts_with('/'){claim_inbound(&c,&id,at)?;}
    execute(&mut routes,&c.profile,&c.robot,&resolved,&tasks,crate::ai_monitor::current_codex_turn,save,|id,message|{
        let latest=crate::dingtalk_bot::read_config()?;
        if !receiving_enabled(&latest,crate::dingtalk_bot::current_profile().as_deref())||latest.profile!=c.profile||latest.robot!=c.robot{return Err("交互已关闭或账号已变化，本次没有发送".into());}
        let client=crate::ai_assistant::find_client(Provider::Codex).ok_or("未找到本机 Codex")?;
        let args=crate::mobile::queue_arguments(id,message)?;
        let refs:Vec<_>=args.iter().map(String::as_str).collect();
        crate::dingtalk_ui::invoke(&client,&refs,Duration::from_secs(30),&AtomicBool::new(false)).map_err(|_|"无法确认排队结果，请核对原任务，不会自动重发".to_owned())?;Ok(())
    })
}
fn event_payload(event:&Value)->Value{
    let mut quote=Value::Null;
    if event["quote"]["explicit"]==true{quote=serde_json::json!({"conversationId":event["conversation"],"messageId":event["quote"]["message"],"processQueryKey":event["quote"]["receipt"]});}
    serde_json::json!({"complete":true,"messages":[{"conversationId":event["conversation"],"messageId":event["message"],"text":event["text"],"createTime":event["at"].as_i64().and_then(|at|chrono::DateTime::from_timestamp(at,0)).map(|at|at.to_rfc3339()),"quotedMessage":quote}]})
}
// Caller holds the reply lock. Claim the incoming ID before any model dispatch.
fn claim_inbound(c:&Config,id:&crate::dingtalk_binding::MessageId,at:i64)->Result<(),String>{
    let path=file("dingtalk-inbound-journal.json");
    let mut journal:BTreeMap<String,i64>=match std::fs::read(&path){Ok(bytes) if bytes.len()<256*1024=>serde_json::from_slice(&bytes).map_err(|_|"接收记录损坏，已停止转发")?,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>BTreeMap::new(),_=>return Err("无法读取接收记录".into())};
    let key=hint(&serde_json::json!([c.profile,c.robot,id.conversation,id.message]).to_string());
    if journal.contains_key(&key){return Err("这条消息已处理，不会重复发送".into());}
    let now=chrono::Utc::now().timestamp();journal.retain(|_,at|now.abs_diff(*at)<86400);
    if journal.len()>=2048{return Err("接收记录已满，已停止转发".into());}
    journal.insert(key,at);
    crate::model::atomic_write(&path,&serde_json::to_vec(&journal).map_err(|_|"无法编码接收记录")?).map_err(|_|"无法保存接收记录，已停止转发".into())
}
fn hint(text:&str)->String{ring::digest::digest(&ring::digest::SHA256,text.as_bytes()).as_ref().iter().map(|b|format!("{b:02x}")).collect()}
/// Early structured entry; only the owned transport invokes it through stdin.
pub fn cli()->Option<std::process::ExitCode>{
    if std::env::args().nth(1).as_deref()!=Some("--dingtalk-event"){return None;}
    use std::io::Read;
    let mut bytes=Vec::new();let result=std::io::stdin().take(65537).read_to_end(&mut bytes);
    let reply=if result.is_ok()&&bytes.len()<=65536{serde_json::from_slice::<Value>(&bytes).map_err(|_|"消息格式无效".to_owned()).and_then(|v|handle_event(&v))}else{Err("消息过大或读取失败".into())};
    println!("{}",serde_json::json!({"reply":reply.unwrap_or_else(|e|format!("未发送：{e}"))}));
    Some(std::process::ExitCode::SUCCESS)
}

struct Worker{cancel:Arc<AtomicBool>,rx:mpsc::Receiver<String>,identity:String}
#[derive(Default)]pub(crate) struct State{worker:Option<Worker>,note:String,failed:Option<String>}
impl Drop for State{fn drop(&mut self){if let Some(w)=&self.worker{w.cancel.store(true,Ordering::Relaxed);}}}
fn connect(c:&Config,cancel:&AtomicBool,tx:&mpsc::Sender<String>)->Result<(),String>{
    use std::{process::{Command,Stdio},io::{Write,BufRead,BufReader}};
    crate::dingtalk_diagnostics::local("receive","components","start","OK",0,"");
    let (node, bridge) = crate::dingtalk_transport::paths("receive", "")?;
    let mut init=serde_json::to_vec(&crate::dingtalk_transport::init(c,cancel)?).map_err(|_|"无法初始化收发组件")?;init.push(b'\n');
    if cancel.load(Ordering::Relaxed){return Ok(());}
    let mut cmd=Command::new(node);cmd.arg(bridge).env("DAOZAI_DATA_DIR",crate::model::config_dir()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    for name in ["NODE_OPTIONS","NODE_PATH","DWS_AGENT_CMD","DWS_CONNECT_CMD","DAOZAI_CONNECT_MARKDOWN"]{cmd.env_remove(name);}
    #[cfg(windows)]{use std::os::windows::process::CommandExt;cmd.creation_flags(0x08000000);}
    let mut child=cmd.spawn().map_err(|_|"无法启动钉钉收发组件")?;
    #[cfg(windows)]let _job=match crate::dingtalk_ui::child_job(&child){Ok(job)=>job,Err(e)=>{let _=child.kill();let _=child.wait();return Err(e);}};
    let mut stdin=child.stdin.take().unwrap();let stdout=child.stdout.take().unwrap();let out=tx.clone();
    let ready=Arc::new(AtomicBool::new(false));let reader_ready=ready.clone();
    std::thread::spawn(move||{for line in BufReader::new(stdout).lines().map_while(Result::ok){
        if line.len()>4096{continue;}if let Ok(v)=serde_json::from_str::<Value>(&line){
            if v["diagnostic"].is_object(){crate::dingtalk_diagnostics::record(v["diagnostic"].clone());}
            if v["failure"].is_object(){crate::dingtalk_diagnostics::record(serde_json::json!({"operation":"receive","stage":v["failure"]["stage"],"outcome":"failure","code":v["failure"]["code"],"http_status":v["failure"]["http_status"]}));}
            if v["ready"]==true{reader_ready.store(true,Ordering::Relaxed);let _=out.send("钉钉 Stream 已连接，发送 /tasks 查看任务".into());}else if v["error"].is_string(){let _=out.send("钉钉收发失败，请查看本机钉钉诊断日志；不会重放任务".into());}
        }
    }});
    if stdin.write_all(&init).is_err(){let _=child.kill();let _=child.wait();return Err("无法初始化钉钉收发组件".into());}
    let started=std::time::Instant::now();
    let _=tx.send("正在连接钉钉 Stream…".into());
    loop{
        let allowed=crate::dingtalk_bot::read_config().is_ok_and(|v|receiving_enabled(&v,Some(&c.profile))&&v.robot==c.robot);
        if cancel.load(Ordering::Relaxed)||!allowed||crate::dingtalk_bot::current_profile().as_deref()!=Some(c.profile.as_str()){
            crate::dingtalk_diagnostics::local("receive","stream","cancelled","SETTINGS_CHANGED_OR_EXIT",started.elapsed().as_millis() as u64,"");drop(stdin);let _=child.kill();let _=child.wait();return Ok(());
        }
        match child.try_wait(){Ok(Some(_))=>{crate::dingtalk_diagnostics::local("receive","stream","stopped","COMPONENT_EXITED",started.elapsed().as_millis() as u64,"");return Err("收发组件已退出，请检查网络、机器人授权或其他设备连接；不会自动重启抢占连接".into());},Err(_)=>{let _=child.kill();let _=child.wait();return Err("无法检查收发组件".into());},_=>{}}
        if !ready.load(Ordering::Relaxed)&&started.elapsed()>Duration::from_secs(45){crate::dingtalk_diagnostics::local("receive","stream","failure","CONNECT_TIMEOUT",started.elapsed().as_millis() as u64,"");let _=child.kill();let _=child.wait();return Err("Stream 建连超时，请检查网络或机器人连接冲突".into());}
        std::thread::sleep(Duration::from_millis(300));
    }
}
impl State{
    pub(crate) fn tick(&mut self,c:&Config,profile:Option<&str>,ctx:&eframe::egui::Context){
        let identity=format!("{}|{}",c.profile,c.robot);let enabled=receiving_enabled(c,profile);
        if let Some(w)=&self.worker{if !enabled||w.identity!=identity{w.cancel.store(true,Ordering::Relaxed);}}
        let mut stopped=false;
        if let Some(w)=&self.worker{loop{match w.rx.try_recv(){Ok(note)=>self.note=note,Err(mpsc::TryRecvError::Disconnected)=>{stopped=true;break;},_=>break}}}
        if stopped{if self.worker.as_ref().is_some_and(|w|!w.cancel.load(Ordering::Relaxed)){self.failed=Some(identity.clone());}self.worker=None;}
        if !enabled{self.failed=None;}
        if enabled&&self.worker.is_none()&&self.failed.as_deref()!=Some(&identity)&&!cfg!(test){
            let (tx,rx)=mpsc::channel();let cancel=Arc::new(AtomicBool::new(false));let flag=cancel.clone();let c=c.clone();let ctx=ctx.clone();
            match std::thread::Builder::new().name("daozai-bot-reply".into()).spawn(move||{if let Err(e)=connect(&c,&flag,&tx){let _=tx.send(e);}ctx.request_repaint();}){
                Ok(_)=>{self.worker=Some(Worker{cancel,rx,identity});self.note="正在检查应用并启动回复接收…".into();},Err(_)=>{self.failed=Some(identity);self.note="无法启动回复接收".into();}
            }
        }
        if self.worker.is_some(){ctx.request_repaint_after(Duration::from_secs(2));}
    }
    pub(crate) fn ui(&mut self,ui:&mut eframe::egui::Ui){
        if !self.note.is_empty(){if self.failed.is_some(){ui.colored_label(ui.visuals().error_fg_color,&self.note);}else{ui.label(&self.note);}}
        if self.failed.is_some()&&self.worker.is_none()&&ui.button("重新连接").clicked(){
            crate::dingtalk_diagnostics::local("receive","worker","start","USER_RECONNECT",0,"");
            self.failed=None;self.note="准备重新连接…".into();ui.ctx().request_repaint();
        }
    }
    #[cfg(test)]pub(crate) fn preview_failed(&mut self){self.failed=Some("demo".into());self.note="收发连接已中断，请重新连接；详细原因见诊断日志".into();}
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn dingtalk_receiving_stays_enabled_when_notification_sending_is_paused(){
        let mut c=Config{enabled:true,replies:true,profile:"corp:me".into(),robot:"bot".into(),..Default::default()};
        assert!(receiving_enabled(&c,Some("corp:me")));
        c.enabled=false;assert!(receiving_enabled(&c,Some("corp:me")));
        assert!(!receiving_enabled(&c,Some("other:me")));assert!(!receiving_enabled(&c,None));
        c.replies=false;assert!(!receiving_enabled(&c,Some("corp:me")));
        c.enabled=true;assert!(!receiving_enabled(&c,Some("corp:me")));
        c.replies=true;c.robot.clear();assert!(!receiving_enabled(&c,Some("corp:me")));
    }
    fn bound_fixture()->(Routes,Task,Config){
        let(mut routes,task)=fixture();let r=routes.items.get_mut("ABC").unwrap();
        r.message_id=Some(crate::dingtalk_binding::MessageId{conversation:"cid".into(),message:"platform-message".into()});
        (routes,task,Config{profile:"corp:me".into(),robot:"bot".into(),..Default::default()})
    }
    #[test]fn bot_reply_bound_id_works_without_quote_body_or_time_after_restart(){
        let(routes,_,c)=bound_fixture();let routes:Routes=serde_json::from_slice(&serde_json::to_vec(&routes).unwrap()).unwrap();
        let payload=serde_json::json!({"complete":true,"messages":[{"text":"继续","quotedMessage":{"conversationId":"cid","messageId":"platform-message"}}]});
        assert_eq!(resolve_quote(&payload,"继续",&routes,&c).unwrap(),"继续 ABC 继续");
        let changed=serde_json::json!({"complete":true,"messages":[{"text":"继续","conversationId":"cid","quotedMessage":{"messageId":"platform-message","text":"格式和正文都已改变"}}]});
        assert_eq!(resolve_quote(&changed,"继续",&routes,&c).unwrap(),"继续 ABC 继续");
    }
    #[test]fn dingtalk_stream_original_receipt_resolves_without_text_or_message_id(){
        let(mut routes,_,c)=bound_fixture();routes.items.get_mut("ABC").unwrap().delivery=Some(crate::dingtalk_binding::Delivery{receipt:"receipt".into(),started:100,confirmed:101});
        let event=serde_json::json!({"conversation":"cid","message":"incoming","text":"继续","at":chrono::Utc::now().timestamp(),"quote":{"explicit":true,"receipt":"receipt"}});
        assert_eq!(resolve_quote(&event_payload(&event),"继续",&routes,&c).unwrap(),"继续 ABC 继续");
        let mut unknown=event.clone();unknown["quote"]["receipt"]=serde_json::json!("unknown");unknown["quote"]["message"]=serde_json::json!("platform-message");
        assert!(resolve_quote(&event_payload(&unknown),"继续",&routes,&c).is_err());
        let mut empty=event.clone();empty["quote"]=serde_json::json!({"explicit":true});assert!(resolve_quote(&event_payload(&empty),"继续",&routes,&c).is_err());
        let foreign=Config{robot:"other".into(),..c.clone()};assert!(resolve_quote(&event_payload(&event),"继续",&routes,&foreign).is_err());
        routes.items.insert("SECOND".into(),routes.items["ABC"].clone());assert!(resolve_quote(&event_payload(&event),"继续",&routes,&c).is_err());
    }
    #[test]fn dingtalk_stream_incoming_id_is_claimed_before_dispatch_and_survives_restart(){
        let _guard=crate::ui_tests::ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        let root=std::env::temp_dir().join(format!("daozai-stream-journal-{}-{}",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap()));std::env::set_var("DAOZAI_DATA_DIR",&root);
        let _lock=lock().unwrap();let(_,_,c)=bound_fixture();let id=crate::dingtalk_binding::MessageId{conversation:"private-cid".into(),message:"private-mid".into()};let at=chrono::Utc::now().timestamp();
        claim_inbound(&c,&id,at).unwrap();assert!(claim_inbound(&c,&id,at).is_err());
        let raw=std::fs::read_to_string(root.join("dingtalk-inbound-journal.json")).unwrap();assert!(!raw.contains("private-cid"));assert!(!raw.contains("private-mid"));
        let other=Config{profile:"other:me".into(),..c};claim_inbound(&other,&id,at).unwrap();
    }
    #[test]fn bot_reply_bound_id_rejects_other_message_conversation_account_and_robot(){
        let(mut routes,_,c)=bound_fixture();let route=routes.items.get_mut("ABC").unwrap();route.body="相同正文".into();
        let date=chrono::DateTime::from_timestamp(route.created,0).unwrap().to_rfc3339();
        let payload=|cid:&str,id:&str|serde_json::json!({"complete":true,"messages":[{"text":"继续","quotedMessage":{"conversationId":cid,"messageId":id,"text":"相同正文","createTime":date}}]});
        assert!(resolve_quote(&payload("cid","another-message"),"继续",&routes,&c).is_err());
        assert!(resolve_quote(&payload("another-conversation","platform-message"),"继续",&routes,&c).is_err());
        for changed in [Config{profile:"other:me".into(),..c.clone()},Config{robot:"other-bot".into(),..c.clone()}]{assert!(resolve_quote(&payload("cid","platform-message"),"继续",&routes,&changed).is_err());}
        routes.items.insert("SECOND".into(),routes.items["ABC"].clone());assert!(resolve_quote(&payload("cid","platform-message"),"继续",&routes,&c).is_err());
    }
    #[test]fn bot_reply_new_unbound_notifications_do_not_fall_back_to_text_matching(){
        let(mut routes,task,c)=bound_fixture();let route=routes.items.get_mut("ABC").unwrap();route.message_id=None;route.body="尚未绑定的通知".into();
        let date=chrono::DateTime::from_timestamp(route.created,0).unwrap().to_rfc3339();
        let payload=serde_json::json!({"complete":true,"messages":[{"text":"继续","quotedMessage":{"text":"尚未绑定的通知","createTime":date}}]});
        assert!(resolve_quote(&payload,"继续",&routes,&c).is_err());
        assert_eq!(direct_command(&routes,&c,"继续",&[task],|_|Some("turn".into()),None).unwrap(),"继续 ABC 继续");
    }
    #[test]fn bot_reply_old_json_body_time_and_visible_codes_cannot_resolve_quotes(){
        let(routes,_,c)=bound_fixture();let mut json=serde_json::to_value(routes).unwrap();
        let row=json["items"]["ABC"].as_object_mut().unwrap();
        for field in ["message_id","delivery","binding_error"]{row.remove(field);}
        // Unknown fields from the previous preview are harmless, never enable a fallback.
        row.insert("binding_required".into(),Value::Bool(false));
        let body="来源：测试电脑-codex\n回复内容：完成\n继续任务：继续 ABC 你的要求\n引用回复继续 · 关联：ABC";
        row.insert("body".into(),Value::String(body.into()));
        let routes:Routes=serde_json::from_value(json).unwrap();
        let date=chrono::DateTime::from_timestamp(routes.items["ABC"].created,0).unwrap().to_rfc3339();
        for id in [Value::Null,Value::String("unknown-id".into())]{
            let payload=serde_json::json!({"complete":true,"messages":[{"text":"继续","conversationId":"cid","quotedMessage":{"messageId":id,"text":body,"createTime":date}}]});
            assert!(resolve_quote(&payload,"继续",&routes,&c).is_err());
        }
        assert!(!routes.items["ABC"].used);
    }
    #[test]fn bot_reply_multiple_tasks_selects_only_the_quoted_message_id(){
        let(mut routes,a,c)=bound_fixture();let mut second=routes.items["ABC"].clone();
        second.task.id="00000000-0000-0000-0000-000000000002".into();second.message_id.as_mut().unwrap().message="second-id".into();
        let b=second.task.clone();routes.items.insert("SECOND".into(),second);
        let payload=serde_json::json!({"complete":true,"messages":[{"text":"检查","conversationId":"cid","quotedMessage":{"messageId":"second-id"}}]});
        let command=resolve_quote(&payload,"检查",&routes,&c).unwrap();
        execute(&mut routes,&c.profile,&c.robot,&command,&[a,b.clone()],|_|Some("turn".into()),|_|Ok(()),|id,text|{assert_eq!(id,b.id);assert_eq!(text,"检查");Ok(())}).unwrap();
        assert!(!routes.items["ABC"].used);assert!(routes.items["SECOND"].used);
    }
    #[test]fn bot_reply_duplicate_id_is_rejected_without_retry(){
        let(mut routes,_,c)=bound_fixture();routes.items.insert("SECOND".into(),routes.items["ABC"].clone());
        let payload=serde_json::json!({"complete":true,"messages":[{"text":"检查","quotedMessage":{"conversationId":"cid","messageId":"platform-message"}}]});
        assert!(matches!(lookup_quote(&payload,"检查",&routes,&c),QuoteLookup::Reject(_)));
    }
    #[test]fn bot_reply_id_lookup_keeps_original_turn_and_consumption_guards(){
        for mode in 0..4{
            let(mut routes,task,c)=bound_fixture();let r=routes.items.get_mut("ABC").unwrap();
            if mode==0{r.used=true;}if mode==1{r.active=false;}if mode==2{r.created-=86401;}
            let payload=serde_json::json!({"complete":true,"messages":[{"text":"继续","quotedMessage":{"conversationId":"cid","messageId":"platform-message"}}]});
            let command=resolve_quote(&payload,"继续",&routes,&c).unwrap();
            assert!(execute(&mut routes,&c.profile,&c.robot,&command,&[task],|_|Some(if mode==3{"changed"}else{"turn"}.into()),|_|panic!("不得保存"),|_,_|panic!("不得执行")).is_err());
        }
    }
    #[test]fn bot_reply_binding_persists_receipt_and_rejects_duplicate_or_changed_delivery(){
        let _guard=crate::ui_tests::ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        let root=std::env::temp_dir().join(format!("daozai-id-binding-{}-{}",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap()));std::env::set_var("DAOZAI_DATA_DIR",&root);std::fs::create_dir_all(&root).unwrap();
        let(mut routes,_,c)=bound_fixture();let delivery=crate::dingtalk_binding::Delivery{receipt:"receipt-only".into(),started:100,confirmed:102};
        routes.items.get_mut("ABC").unwrap().message_id=None;routes.items.get_mut("ABC").unwrap().delivery=Some(delivery.clone());
        routes.items.insert("SECOND".into(),routes.items["ABC"].clone());save(&routes).unwrap();
        let id=crate::dingtalk_binding::MessageId{conversation:"cid".into(),message:"platform-message".into()};
        bind_message("ABC",&c,&delivery,id.clone()).unwrap();assert_eq!(load().unwrap().items["ABC"].message_id,Some(id.clone()));
        assert!(bind_message("SECOND",&c,&delivery,id.clone()).is_err());
        assert!(bind_message("ABC",&c,&crate::dingtalk_binding::Delivery{receipt:"different".into(),..delivery},id).is_err());
    }
    #[test]
    fn bot_reply_table_normalization_does_not_strip_punctuation_from_data(){
        assert_eq!(visible_text("| ---: | :--- | :---: |"),visible_text("|---|---|---|"));
        assert_ne!(visible_text("| 地址 | host:8080 |"),visible_text("| 地址 | host8080 |"));
        assert_ne!(visible_text("| ---:x | --- |"),visible_text("| --- | --- |"));
    }
    #[test]fn bot_reply_direct_selects_latest_notification_and_never_skips_a_consumed_latest(){
        let(mut routes,t)=fixture();let c=Config{profile:"corp:me".into(),robot:"bot".into(),..Default::default()};
        assert_eq!(direct_command(&routes,&c,"继续检查",&[t.clone()],|_|Some("turn".into()),None).unwrap(),"继续 ABC 继续检查");
        let mut other=routes.items["ABC"].clone();other.task.id="00000000-0000-0000-0000-000000000002".into();let b=other.task.clone();routes.items.insert("DEF".into(),other);
        assert!(direct_command(&routes,&c,"继续检查",&[t.clone(),b.clone()],|_|Some("turn".into()),None).is_err());
        routes.items.get_mut("ABC").unwrap().created-=2;
        assert_eq!(direct_command(&routes,&c,"继续检查",&[t.clone(),b.clone()],|_|Some("turn".into()),None).unwrap(),"继续 DEF 继续检查");
        routes.items.get_mut("DEF").unwrap().used=true;
        assert!(direct_command(&routes,&c,"继续检查",&[t.clone(),b],|_|Some("turn".into()),None).is_err());
        routes.items.remove("DEF");
        assert!(direct_command(&routes,&c,"继续检查",&[t.clone()],|_|Some("new-turn".into()),None).is_err());
        routes.items.get_mut("ABC").unwrap().created-=86401;assert!(direct_command(&routes,&c,"继续检查",&[t],|_|Some("turn".into()),None).is_err());
    }
    #[test]fn bot_reply_commands_never_dispatch_or_consume_a_notification(){
        let(mut routes,t)=fixture();
        for command in ["/tasks","/tasks 1","/help","/unknown","/tasks invalid","/tasks-extra"," /unknown "]{
            let _=execute(&mut routes,"corp:me","bot",command,&[t.clone()],|_|panic!("command must not inspect agent"),|_|panic!("must not save"),|_,_|panic!("must not dispatch"));
            assert!(!routes.items["ABC"].used);
        }
        let c=Config{profile:"corp:me".into(),robot:"bot".into(),..Default::default()};
        assert!(direct_command(&routes,&c," /unknown",&[t],|_|Some("turn".into()),None).is_err());
    }
    #[test]fn bot_reply_direct_respects_incoming_conversation_and_notification_time(){
        let(mut routes,t,c)=bound_fixture();let at=routes.items["ABC"].created;
        let mut future=routes.items["ABC"].clone();future.created=at+10;routes.items.insert("FUTURE".into(),future);
        assert_eq!(direct_command(&routes,&c,"检查",&[t.clone()],|_|Some("turn".into()),Some(("cid",at))).unwrap(),"继续 ABC 检查");
        assert!(direct_command(&routes,&c,"检查",&[t],|_|Some("turn".into()),Some(("other-cid",at))).is_err());
    }
    #[test]fn bot_reply_invalid_quote_never_falls_back_to_the_only_pending_task(){let(routes,_)=fixture();let c=Config{profile:"corp:me".into(),robot:"bot".into(),..Default::default()};let v=serde_json::json!({"complete":true,"messages":[{"text":"继续检查","quotedMessage":{"text":"其他任务的旧消息"}}]});assert!(resolve_quote(&v,"继续检查",&routes,&c).is_err());}
    fn fixture()->(Routes,Task){let t=Task{provider:Provider::Codex,id:"00000000-0000-0000-0000-000000000001".into(),title:"修复连接".into(),status:crate::ai_assistant::Status::Completed,updated:7,owned:false};let r=Route{profile:"corp:me".into(),robot:"bot".into(),task:t.clone(),turn:"turn".into(),created:chrono::Utc::now().timestamp(),active:true,used:false,body:String::new(),delivery:None,message_id:None,binding_error:String::new()};(Routes{items:BTreeMap::from([("ABC".into(),r)])},t)}
    #[test]fn bot_reply_queues_original_once_with_literal_message(){let(mut routes,t)=fixture();let saved=std::cell::Cell::new(false);let result=execute(&mut routes,"corp:me","bot","继续 ABC --yolo & echo no",&[t.clone()],|_|Some("turn".into()),|r|{assert!(r.items["ABC"].used);saved.set(true);Ok(())},|id,msg|{assert!(saved.get());assert_eq!(id,t.id);assert_eq!(msg,"--yolo & echo no");Ok(())});assert!(result.unwrap().contains("已排队"));assert!(execute(&mut routes,"corp:me","bot","继续 ABC again",&[t],|_|Some("turn".into()),|_|Ok(()),|_,_|panic!("duplicate")).is_err());}
    #[test]fn bot_reply_rejects_wrong_account_robot_stale_turn_and_expired_notification(){for mode in 0..5{let(mut routes,mut t)=fixture();if mode==3{t.updated+=1;}if mode==4{routes.items.get_mut("ABC").unwrap().created-=86401;}assert!(execute(&mut routes,if mode==0{"other:me"}else{"corp:me"},if mode==1{"other"}else{"bot"},"继续 ABC go",&[t],|_|Some(if mode==2{"new"}else{"turn"}.into()),|_|panic!("must not save"),|_,_|panic!("must not queue")).is_err());}}
    #[test]fn bot_reply_uncertain_dispatch_remains_consumed_after_restart(){let(mut routes,t)=fixture();assert!(execute(&mut routes,"corp:me","bot","继续 ABC go",&[t.clone()],|_|Some("turn".into()),|_|Ok(()),|_,_|Err("timeout".into())).is_err());let bytes=serde_json::to_vec(&routes).unwrap();let mut routes=serde_json::from_slice(&bytes).unwrap();assert!(execute(&mut routes,"corp:me","bot","继续 ABC go",&[t],|_|Some("turn".into()),|_|Ok(()),|_,_|panic!("replay")).is_err());}
    #[test]fn bot_reply_save_failure_prevents_dispatch(){let(mut routes,t)=fixture();assert!(execute(&mut routes,"corp:me","bot","继续 ABC go",&[t],|_|Some("turn".into()),|_|Err("disk".into()),|_,_|panic!("no send")).is_err());}
    #[test]fn bot_reply_listing_does_not_execute(){let(mut routes,t)=fixture();let result=execute(&mut routes,"corp:me","bot","/tasks",&[t],|_|None,|_|panic!("no save"),|_,_|panic!("no queue")).unwrap();assert!(result.contains("修复连接"));assert!(!routes.items["ABC"].used);}
    #[test]fn bot_reply_compact_list_keeps_older_tasks_accessible_without_dispatch(){
        let(mut routes,t)=fixture();let base=routes.items.remove("ABC").unwrap();let mut tasks=Vec::new();
        for i in 0..7{let mut r=base.clone();r.task.id=format!("task-{i}");r.task.title=format!("任务{i}");r.task.updated=t.updated+i;r.body="不应显示的正文".into();tasks.push(r.task.clone());routes.items.insert(format!("CODE{i}"),r);}
        let mut foreign=base.clone();foreign.profile="other:me".into();foreign.task.updated=999;routes.items.insert("FOREIGN".into(),foreign);
        let mut list=|input:&str|execute(&mut routes,"corp:me","bot",input,&tasks,|_|None,|_|panic!("no save"),|_,_|panic!("no queue"));
        let first=list("/tasks").unwrap();let second=list("/tasks 2").unwrap();
        assert!(first.contains("- 任务6 · CODE6"));assert!(!first.contains("CODE1"));assert!(second.contains("- 任务0 · CODE0"));assert!(second.contains("- 任务1 · CODE1"));
        for text in [&first,&second]{for hidden in ["已完成","未绑定","不应显示的正文","FOREIGN"]{assert!(!text.contains(hidden));}assert_eq!(text.matches("你的要求").count(),1);}
        assert!(list("/tasks 0").is_err());assert!(list("/tasks 999999999999999999999999999").is_err());assert!(list("/tasks 3").is_err());
        assert!(routes.items.values().all(|r|!r.used));
    }
    #[cfg(windows)]#[test]fn bot_reply_owned_process_ends_when_guard_closes(){
        use std::{process::{Command,Stdio},os::windows::process::CommandExt};
        let mut child=Command::new("powershell.exe").args(["-NoProfile","-Command","Start-Sleep -Seconds 30"]).creation_flags(0x08000000).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let job=match crate::dingtalk_ui::child_job(&child){Ok(v)=>v,Err(e)=>{let _=child.kill();let _=child.wait();panic!("{e}");}};drop(job);
        for _ in 0..20{if child.try_wait().unwrap().is_some(){return;}std::thread::sleep(Duration::from_millis(50));}
        let _=child.kill();let _=child.wait();panic!("owned child survived job close");
    }
}
