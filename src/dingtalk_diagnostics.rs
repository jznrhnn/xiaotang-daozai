//! Bounded, best-effort diagnostics. Never accept free-form error/body text.
use serde::{Deserialize,Serialize};
use serde_json::Value;
use std::{fs::OpenOptions,io::Read,time::Duration};
#[derive(Serialize,Deserialize)]
struct Entry{
    at:i64,operation:String,stage:String,outcome:String,code:String,
    http_status:Option<u16>,elapsed_ms:u64,attempt:u64,retry_count:u64,correlation:String,
}
fn member(value:&str,allowed:&[&str])->String{if allowed.contains(&value){value.into()}else{"unknown".into()}}
fn code(value:&str)->String{
    if !value.is_empty()&&value.len()<=80&&value.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'_'|b'.'|b':'|b'-')){value.into()}else{"unknown".into()}
}
pub(crate) fn correlation(value:&str)->String{ring::digest::digest(&ring::digest::SHA256,value.as_bytes()).as_ref()[..12].iter().map(|b|format!("{b:02x}")).collect()}
fn entry(value:&Value)->Entry{
    let s=|key:&str|value[key].as_str().unwrap_or("");
    let attempt=value["attempt"].as_u64().unwrap_or(1).clamp(1,3);
    Entry{at:chrono::Utc::now().timestamp(),
        operation:member(s("operation"),&["notify","receive","reply","dispatch","command","worker","log"]),
        stage:member(s("stage"),&["components","app_lookup","robot_lookup","credentials","token","notification","binding","stream","callback","helper","reply","routing","queue","worker","journal"]),
        outcome:member(s("outcome"),&["start","success","failure","accepted","rejected","duplicate","queued","stopped","cancelled","auto_disabled","paused"]),
        code:code(s("code")),http_status:value["http_status"].as_u64().filter(|v|(100..=599).contains(v)).map(|v|v as u16),
        elapsed_ms:value["elapsed_ms"].as_u64().unwrap_or(0).min(86400000),attempt,retry_count:attempt-1,
        correlation:{let v=s("correlation");if v.len()==24&&v.bytes().all(|b|b.is_ascii_hexdigit()){v.into()}else{String::new()}},
    }
}
pub(crate) fn record(value:Value){
    #[cfg(test)]if !std::env::var_os("DAOZAI_DATA_DIR").map(std::path::PathBuf::from).is_some_and(|p|p.starts_with(std::env::temp_dir())){return;}
    let _=append(entry(&value));
}
fn append(row:Entry)->Result<(),()>{
    let dir=crate::model::config_dir();std::fs::create_dir_all(&dir).map_err(|_|())?;
    let mut opts=OpenOptions::new();opts.read(true).write(true).create(true);
    #[cfg(windows)]{use std::os::windows::fs::OpenOptionsExt;opts.share_mode(0);}
    let mut guard=None;
    for _ in 0..8{if let Ok(v)=opts.open(dir.join("dingtalk-diagnostics.lock")){guard=Some(v);break;}std::thread::sleep(Duration::from_millis(10));}
    let _guard=guard.ok_or(())?;let path=dir.join("dingtalk-diagnostics.json");
    let mut bytes=Vec::new();if let Ok(file)=std::fs::File::open(&path){let _=file.take(512*1024+1).read_to_end(&mut bytes);}
    // Deserialize only the known schema; re-sanitize even existing rows before rewriting.
    let mut rows=if bytes.len()<=512*1024{serde_json::from_slice::<Vec<Entry>>(&bytes).unwrap_or_default()}else{Vec::new()};
    let now=chrono::Utc::now().timestamp();rows.retain(|v|now.abs_diff(v.at)<=7*86400);
    for old in &mut rows{let at=old.at;*old=entry(&serde_json::to_value(&*old).map_err(|_|())?);old.at=at;}
    rows.push(row);if rows.len()>400{rows.drain(..rows.len()-400);}
    let bytes=serde_json::to_vec(&rows).map_err(|_|())?;
    crate::model::atomic_write(&path,&bytes).map_err(|_|())
}
pub(crate) fn local(operation:&str,stage:&str,outcome:&str,code:&str,elapsed_ms:u64,correlation:&str){
    record(serde_json::json!({"operation":operation,"stage":stage,"outcome":outcome,"code":code,"elapsed_ms":elapsed_ms,"correlation":correlation}));
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn dingtalk_diagnostics_drop_unknown_fields_and_free_form_errors_and_bound_history(){
        let _guard=crate::ui_tests::ENV_LOCK.lock().unwrap_or_else(|e|e.into_inner());
        let root=std::env::temp_dir().join(format!("daozai-log-{}-{}",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap()));std::env::set_var("DAOZAI_DATA_DIR",&root);
        let correlation=correlation("private-message-id");
        for _ in 0..410{record(serde_json::json!({"operation":"notify","stage":"notification","outcome":"failure","code":"Forbidden","http_status":403,"elapsed_ms":123,"correlation":correlation,"clientSecret":"private-secret","text":"private-body","error":"https://secret?token=private-token"}));}
        record(serde_json::json!({"operation":"private-body","stage":"https://secret","outcome":"private-secret","code":"Bearer private-token","correlation":"private-message-id"}));
        let raw=std::fs::read_to_string(root.join("dingtalk-diagnostics.json")).unwrap();
        for forbidden in ["private-secret","private-body","private-token","private-message-id","https://secret"]{assert!(!raw.contains(forbidden));}
        let rows:Vec<Value>=serde_json::from_str(&raw).unwrap();assert_eq!(rows.len(),400);assert_eq!(rows[0]["http_status"],403);assert_eq!(rows[0]["elapsed_ms"],123);assert_eq!(rows[0]["retry_count"],0);
        assert_eq!(rows.last().unwrap()["stage"],"unknown");
        let mut old=entry(&serde_json::json!({}));old.at=chrono::Utc::now().timestamp()-8*86400;std::fs::write(root.join("dingtalk-diagnostics.json"),serde_json::to_vec(&vec![old]).unwrap()).unwrap();
        local("worker","worker","start","OK",0,"");let rows:Vec<Value>=serde_json::from_slice(&std::fs::read(root.join("dingtalk-diagnostics.json")).unwrap()).unwrap();assert_eq!(rows.len(),1);
        record(serde_json::json!({"operation":"worker","stage":"app_lookup","outcome":"success","code":"OK","attempt":3}));
        local("worker","worker","start","OK",0,"");
        let rows:Vec<Value>=serde_json::from_slice(&std::fs::read(root.join("dingtalk-diagnostics.json")).unwrap()).unwrap();assert_eq!(rows[1]["attempt"],3);assert_eq!(rows[1]["retry_count"],2);
    }
}
