//! Minimal standalone adapter of soimy's DingTalk transport. No OpenClaw host.
use crate::dingtalk_bot::Config;
use serde_json::{Value,json};
use std::{path::PathBuf,sync::atomic::AtomicBool,time::{Duration,Instant}};
use crate::dingtalk_diagnostics as log;

pub(crate) fn paths(operation: &str, trace: &str) -> Result<(PathBuf, PathBuf), String> {
    let exe=std::env::current_exe().map_err(|_|"无法找到岛仔程序")?;
    let dir=exe.parent().ok_or("无法定位收发组件")?.join("components/dingtalk");
    let check = |name: &str, label: &str| {
        let path = dir.join(name);
        match component_problem(std::fs::metadata(&path)) {
            None => Ok(path),
            Some((code, reason)) => {
                log::local(operation, "components", "failure", &format!("{label}_{code}"), 0, trace);
                Err(format!("钉钉收发组件 {name} {reason}，请检查程序旁的 components 文件夹"))
            }
        }
    };
    Ok((check("node.exe", "NODE_EXE")?, check("bridge.cjs", "BRIDGE_CJS")?))
}

fn component_problem(metadata: std::io::Result<std::fs::Metadata>) -> Option<(&'static str, &'static str)> {
    use std::io::ErrorKind;
    match metadata {
        Ok(metadata) if metadata.is_file() => None,
        Ok(_) => Some(("NOT_FILE", "不是普通文件")),
        Err(error) => Some(match error.kind() {
            ErrorKind::NotFound => ("MISSING", "不存在"),
            ErrorKind::PermissionDenied => ("ACCESS_DENIED", "无权访问"),
            _ => ("IO_ERROR", "无法读取文件状态"),
        }),
    }
}
fn credentials_value(v:&Value,robot:&str)->Result<Value,String>{
    let mut current=v;
    for _ in 0..8{
        let id=["clientId","appKey","clientID"].iter().find_map(|k|current[k].as_str());
        let secret=["clientSecret","appSecret"].iter().find_map(|k|current[k].as_str()).filter(|v|!v.trim().is_empty()&&v.len()<=4096);
        if let (Some(id),Some(secret))=(id,secret){
            if id!=robot{return Err("返回的凭证不属于所选机器人".into());}
            return Ok(json!({"clientId":id,"clientSecret":secret}));
        }
        current=["data","content","result"].iter().find_map(|k|current.get(k).filter(|v|v.is_object())).ok_or("未能读取所选机器人的凭证，请检查个人授权和应用管理权限")?;
    }
    Err("机器人凭证格式无效".into())
}
pub(crate) fn init(c:&Config,cancel:&AtomicBool)->Result<Value,String>{
    let trace=log::correlation(&format!("auth:{}:{}",c.robot,chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()));
    init_traced(c,cancel,&trace)
}
fn init_traced(c:&Config,cancel:&AtomicBool,trace:&str)->Result<Value,String>{
    let cli=crate::dingtalk_ui::client().ok_or("未找到钉钉官方授权组件")?;
    let call=|stage:&str,args:&[&str]|retry_read_only(|attempt|{
        let at=Instant::now();log::record(json!({"operation":"worker","stage":stage,"outcome":"start","code":"OK","attempt":attempt,"correlation":trace}));
        let result=crate::dingtalk_ui::run(&cli,args,Duration::from_secs(40),cancel);
        log::record(json!({"operation":"worker","stage":stage,"outcome":if result.is_ok(){"success"}else{"failure"},"code":if result.is_ok(){"OK"}else{"DWS_COMMAND_FAILED"},"elapsed_ms":at.elapsed().as_millis() as u64,"attempt":attempt,"correlation":trace}));result
    },|delay|{
        let until=Instant::now()+delay;
        while Instant::now()<until{if cancel.load(std::sync::atomic::Ordering::Relaxed){return false;}std::thread::sleep(Duration::from_millis(50));}true
    });
    let v=call("app_lookup",&["dev","app","get","--app-key",&c.robot,"--profile",&c.profile,"--format","json"])?;
    let app=v["data"]["unifiedAppId"].as_str().filter(|_|v["data"]["appKey"]==c.robot).ok_or("无法确认所选机器人的应用")?;
    let v=call("robot_lookup",&["dev","app","robot","get","--unified-app-id",app,"--profile",&c.profile,"--format","json"])?;
    if v["data"]["robotCode"]!=c.robot||v["data"]["mode"]!="STREAM"{return Err("所选机器人尚未发布 Stream 接收配置".into());}
    let credentials=call("credentials",&["dev","app","credentials","get","--unified-app-id",app,"--profile",&c.profile,"--format","json"])?;
    let mut init=credentials_value(&credentials,&c.robot).map_err(|e|{log::local("worker","credentials","failure","INVALID_CREDENTIAL_RESPONSE",0,&trace);e})?;
    let user=c.profile.split_once(':').map(|(_,user)|user).filter(|s|!s.is_empty()).ok_or("账号标识无效")?;
    init["robot"]=json!(c.robot);init["user"]=json!(user);init["profile"]=json!(c.profile);
    init["correlation"]=json!(trace);
    init["exe"]=json!(std::env::current_exe().map_err(|_|"无法定位岛仔")?);
    Ok(init)
}
// Only read-only app/robot/credential lookups use this retry policy. Never wrap
// the notification delivery call: a timeout there may mean it was already sent.
fn retry_read_only<T>(mut call:impl FnMut(u64)->Result<T,String>,mut pause:impl FnMut(Duration)->bool)->Result<T,String>{
    for attempt in 1..=3{
        match call(attempt){Ok(value)=>return Ok(value),Err(error)=>{
            if attempt==3||error.contains("取消"){return Err(error);}
            if !pause(Duration::from_secs(attempt)){return Err("已取消授权查询重试".into());}
        }}
    }
    unreachable!()
}
pub(crate) fn send(c:&Config,body:&str,cancel:&AtomicBool)->Result<crate::dingtalk_binding::Delivery,String>{
    let at=Instant::now();let trace=log::correlation(&format!("notify:{}:{}",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()));
    log::local("notify","components","start","OK",0,&trace);
    let (node, bridge) = paths("notify", &trace)?;
    let mut init=init_traced(c,cancel,&trace).map_err(|e|{log::local("notify","credentials","failure","INITIALIZATION_FAILED",at.elapsed().as_millis() as u64,&trace);e})?;init["body"]=json!(body);
    let mut input=serde_json::to_vec(&init).map_err(|_|"无法初始化发送组件")?;input.push(b'\n');
    let started=chrono::Utc::now().timestamp();
    let bytes=crate::dingtalk_ui::invoke_input(&node,&[&bridge.to_string_lossy(),"--send"],Duration::from_secs(50),cancel,input).map_err(|e|{log::local("notify","components","failure","COMPONENT_INVOKE_FAILED",at.elapsed().as_millis() as u64,&trace);e})?;
    let receipt=decode_send(&bytes,&trace).map_err(|e|{log::local("notify","components","failure","SEND_UNCONFIRMED",at.elapsed().as_millis() as u64,&trace);e})?;
    log::local("notify","components","success","DELIVERY_CONFIRMED",at.elapsed().as_millis() as u64,&trace);
    Ok(crate::dingtalk_binding::Delivery{receipt,started,confirmed:chrono::Utc::now().timestamp()})
}
fn decode_send(bytes:&[u8],trace:&str)->Result<String,String>{
    let text=std::str::from_utf8(bytes).map_err(|_|"发送组件返回格式错误，请查看钉钉诊断日志")?;
    let mut terminal=None;let mut count=0;
    for line in text.lines().filter(|s|!s.trim().is_empty()){
        count+=1;if count>64{return Err("发送组件返回记录过多".into());}
        let v:Value=serde_json::from_str(line).map_err(|_|"发送组件返回记录不完整，请查看钉钉诊断日志")?;
        if v["diagnostic"].is_object(){let mut row=v["diagnostic"].clone();row["correlation"]=json!(trace);log::record(row);}
        if v["receipt"].is_string()||v["failure"].is_object(){if terminal.is_some(){return Err("发送结果不唯一，不自动重发".into());}terminal=Some(v);}
    }
    let v=terminal.ok_or("通知发送结果未确认，请查看钉钉诊断日志")?;
    if let Some(receipt)=v["receipt"].as_str().filter(|s|!s.is_empty()){return Ok(receipt.into());}
    let failure=&v["failure"];let stage=match failure["stage"].as_str(){Some("token")=>"应用鉴权",Some("notification")=>"通知投递",_=>"发送组件"};
    // Reuse the logger's strict projection; never display free-form platform error messages.
    log::record(json!({"operation":"notify","stage":failure["stage"],"outcome":"failure","code":failure["code"],"http_status":failure["http_status"],"correlation":trace}));
    let code=failure["code"].as_str().filter(|s|!s.is_empty()&&s.len()<=80&&s.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'_'|b'.'|b':'|b'-'))).unwrap_or("UNKNOWN");
    Err(format!("{stage}失败（{code}），详见钉钉诊断日志；不会自动重发"))
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]
    fn dingtalk_component_check_distinguishes_wrong_type_and_io_errors() {
        let meta = std::fs::metadata(std::env::temp_dir());
        assert_eq!(component_problem(meta).map(|problem| problem.0), Some("NOT_FILE"));
        for (error, expected) in [(std::io::ErrorKind::NotFound, "MISSING"), (std::io::ErrorKind::PermissionDenied, "ACCESS_DENIED")] {
            assert_eq!(component_problem(Err(error.into())).map(|problem| problem.0), Some(expected));
        }
        assert!(component_problem(std::fs::metadata(std::env::current_exe().unwrap())).is_none());
    }
    #[test]fn dingtalk_read_only_queries_retry_with_backoff_but_stop_on_cancel_or_limit(){
        let mut attempts=Vec::new();let mut waits=Vec::new();
        let result=retry_read_only(|attempt|{attempts.push(attempt);if attempt<3{Err("temporary lookup error".into())}else{Ok(42)}},|delay|{waits.push(delay);true});
        assert_eq!(result.unwrap(),42);assert_eq!(attempts,vec![1,2,3]);assert_eq!(waits,vec![Duration::from_secs(1),Duration::from_secs(2)]);
        let mut calls=0;assert!(retry_read_only::<()>(|_|{calls+=1;Err("temporary".into())},|_|true).is_err());assert_eq!(calls,3);
        let mut calls=0;assert!(retry_read_only::<()>(|_|{calls+=1;Err("已取消本次操作".into())},|_|panic!("must not retry cancellation")).is_err());assert_eq!(calls,1);
        let mut calls=0;assert!(retry_read_only::<()>(|_|{calls+=1;Err("temporary".into())},|_|false).is_err());assert_eq!(calls,1);
    }
    #[test]#[ignore="手动本机 Node 管道/作业对象测试，使用假凭证且不联网"]
    fn dingtalk_transport_node_pipe_read_only(){
        let node=PathBuf::from(std::env::var_os("DAOZAI_BRIDGE_PROBE_NODE").unwrap());
        let bridge=std::env::var("DAOZAI_BRIDGE_PROBE_FILE").unwrap();
        let bytes=crate::dingtalk_ui::invoke_input(&node,&[&bridge,"--send"],Duration::from_secs(10),&AtomicBool::new(false),b"{\"robot\":\"synthetic\",\"clientId\":\"synthetic\",\"user\":\"me\"}\n".to_vec()).unwrap();
        let value:Value=serde_json::from_slice(&bytes).unwrap();assert_eq!(value["failure"]["code"],"INVALID_CREDENTIALS");
        println!("node_pipe_and_job_passed=true; credentials=false; network_send=false");
    }
    #[test]fn dingtalk_transport_credentials_are_scoped_and_reject_missing_or_wrong_robot(){
        let v=json!({"data":{"result":{"clientId":"selected","clientSecret":"synthetic-secret"}}});
        assert_eq!(credentials_value(&v,"selected").unwrap()["clientId"],"selected");
        assert!(credentials_value(&v,"other").is_err());assert!(credentials_value(&json!({"data":{"clientId":"selected"}}),"selected").is_err());
    }
    #[test]fn dingtalk_diagnostics_preserve_send_failure_envelopes_and_keep_receipts_private(){
        let _guard=crate::ui_tests::ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        let root=std::env::temp_dir().join(format!("daozai-send-log-{}-{}",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap()));std::env::set_var("DAOZAI_DATA_DIR",&root);
        let trace=log::correlation("synthetic-private-event");
        let failed=b"{\"diagnostic\":{\"operation\":\"notify\",\"stage\":\"notification\",\"outcome\":\"failure\",\"code\":\"Forbidden\",\"http_status\":403,\"elapsed_ms\":123,\"message\":\"private-token\"}}\n{\"failure\":{\"stage\":\"notification\",\"code\":\"Forbidden\",\"http_status\":403}}\n";
        let error=decode_send(failed,&trace).unwrap_err();assert!(error.contains("Forbidden"));assert!(error.contains("通知投递"));
        assert_eq!(decode_send(b"{\"receipt\":\"private-receipt\"}\n",&trace).unwrap(),"private-receipt");
        assert!(decode_send(b"{\"receipt\":\"one\"}\n{\"receipt\":\"two\"}",&trace).is_err());
        assert!(decode_send(b"{\"error\":\"private-secret\"}",&trace).is_err());
        let raw=std::fs::read_to_string(root.join("dingtalk-diagnostics.json")).unwrap();assert!(raw.contains("403"));assert!(raw.contains("123"));
        for value in ["private-token","private-secret","private-receipt","synthetic-private-event"]{assert!(!raw.contains(value));}
    }
    #[test]#[ignore="手动只读检查所选机器人授权，不连接 Stream、不发消息、不输出凭证"]
    fn dingtalk_transport_current_authorization_read_only(){
        let c=crate::dingtalk_bot::read_config().unwrap();let value=init(&c,&AtomicBool::new(false)).unwrap();
        assert_eq!(value["clientId"],c.robot);assert!(value["clientSecret"].as_str().is_some_and(|s|!s.is_empty()));
        println!("selected_robot_authorization_confirmed=true; stream_connected=false; message_sent=false");
    }
}
