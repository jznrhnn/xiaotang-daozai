//! Personal DingTalk OAuth through the official DWS client. No token is copied into Daozai.
use eframe::egui;
use serde_json::Value;
use std::{io::Read,path::PathBuf,process::{Command,Stdio},sync::{Arc,atomic::{AtomicBool,Ordering},mpsc::{self,Receiver}},time::{Duration,Instant}};

const LIMIT:u64=256*1024;
#[derive(Clone,Debug)]
struct Account {profile:String,organization:String,name:String,current:bool}
struct Checked {accounts:Vec<Account>,selected:Option<String>,authenticated:bool}
struct Pending {rx:Receiver<Result<Checked,String>>,cancel:Arc<AtomicBool>}
struct Installing {rx:Receiver<Result<(),String>>,cancel:Arc<AtomicBool>}
pub(crate) struct State {
    accounts:Vec<Account>, selected:Option<String>, authenticated:bool,
    checked:bool, pending:Option<Pending>, note:String, error:bool,
    installing:Option<Installing>,
    #[cfg(test)] missing_preview:bool,
}
fn pin_path()->PathBuf{crate::model::config_dir().join("dingtalk-link.json")}
fn valid_id(value:&str)->bool{!value.is_empty() && value.len()<=256 && value.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'-'|b'_'))}
fn valid_profile(value:&str)->bool{value.split_once(':').is_some_and(|(corp,user)|valid_id(corp)&&valid_id(user))}
impl Default for State {
    fn default()->Self {
        let selected=std::fs::read(pin_path()).ok().filter(|v|v.len()<4096)
            .and_then(|v|serde_json::from_slice::<Value>(&v).ok())
            .and_then(|v|v["profile"].as_str().filter(|p|valid_profile(p)).map(str::to_owned));
        Self{accounts:vec![],selected,authenticated:false,checked:false,pending:None,note:String::new(),error:false,installing:None,#[cfg(test)]missing_preview:false}
    }
}
impl Drop for State {fn drop(&mut self){if let Some(p)=&self.pending{p.cancel.store(true,Ordering::Relaxed);}if let Some(p)=&self.installing{p.cancel.store(true,Ordering::Relaxed);}}}

pub(crate) fn client()->Option<PathBuf>{
    let mut paths=vec![crate::model::config_dir().join("tools/dws/dws.exe")];
    if let Some(home)=std::env::var_os("USERPROFILE"){paths.push(PathBuf::from(home).join(".local/bin/dws.exe"));}
    if let Some(path)=std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path){paths.push(dir.join(if cfg!(windows){"dws.exe"}else{"dws"}));}
    }
    if let Some(roaming)=std::env::var_os("APPDATA") {
        paths.push(PathBuf::from(roaming).join("npm/node_modules/dingtalk-workspace-cli/vendor/dws.exe"));
    }
    paths.into_iter().find(|p|p.is_file())
}
pub(crate) fn invoke(path:&std::path::Path,args:&[&str],limit:Duration,cancel:&AtomicBool)->Result<Vec<u8>,String>{
    invoke_env(path,args,limit,cancel,&[])
}
#[cfg(windows)]
pub(crate) struct ChildJob(windows::Win32::Foundation::HANDLE);
#[cfg(windows)]impl Drop for ChildJob{fn drop(&mut self){unsafe{let _=windows::Win32::Foundation::CloseHandle(self.0);}}}
#[cfg(windows)]pub(crate) fn child_job(child:&std::process::Child)->Result<ChildJob,String>{
    use windows::Win32::{Foundation::HANDLE,System::JobObjects::*};
    use std::os::windows::io::AsRawHandle;
    unsafe{
        let job=ChildJob(CreateJobObjectW(None,None).map_err(|_|"无法管理机器人接收进程")?);
        let mut info=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();info.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(job.0,JobObjectExtendedLimitInformation,&info as *const _ as _,std::mem::size_of_val(&info) as u32).map_err(|_|"无法设置机器人进程退出保护")?;
        AssignProcessToJobObject(job.0,HANDLE(child.as_raw_handle())).map_err(|_|"无法托管机器人接收进程")?;Ok(job)
    }
}
fn stop_child(child:&mut std::process::Child){
    let _=child.kill();
    let deadline=Instant::now()+Duration::from_secs(1);
    while Instant::now()<deadline {
        if !matches!(child.try_wait(),Ok(None)){break;}
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn invoke_env(path:&std::path::Path,args:&[&str],limit:Duration,cancel:&AtomicBool,env:&[(&str,&str)])->Result<Vec<u8>,String>{
    invoke_input_env(path,args,limit,cancel,env,None)
}
pub(crate) fn invoke_input(path:&std::path::Path,args:&[&str],limit:Duration,cancel:&AtomicBool,input:Vec<u8>)->Result<Vec<u8>,String>{
    invoke_input_env(path,args,limit,cancel,&[],Some(input))
}
fn invoke_input_env(path:&std::path::Path,args:&[&str],limit:Duration,cancel:&AtomicBool,env:&[(&str,&str)],input:Option<Vec<u8>>)->Result<Vec<u8>,String>{
    if cancel.load(Ordering::Relaxed){return Err("已取消本次操作".into());}
    let mut command=Command::new(path);
    command.args(args).envs(env.iter().copied()).stdin(if input.is_some(){Stdio::piped()}else{Stdio::null()}).stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some(){command.env_remove("NODE_OPTIONS").env_remove("NODE_PATH");}
    #[cfg(windows)]{use std::os::windows::process::CommandExt;command.creation_flags(0x08000000);}
    let mut child=command.spawn().map_err(|_|"无法启动钉钉组件，请检查安装")?;
    // Own descendants before starting pipe readers; closing the job also closes their inherited pipes.
    #[cfg(windows)]let mut job=match child_job(&child){Ok(job)=>Some(job),Err(_)=>{stop_child(&mut child);return Err("无法管理钉钉组件进程，已停止操作".into());}};
    if let Some(input)=input{let mut stdin=child.stdin.take().unwrap();std::thread::spawn(move||{use std::io::Write;let _=stdin.write_all(&input);});}
    let stdout=child.stdout.take().unwrap();let stderr=child.stderr.take().unwrap();
    let (out_tx,out_rx)=mpsc::sync_channel(1);
    std::thread::spawn(move||{let mut bytes=Vec::new();let result=stdout.take(LIMIT+1).read_to_end(&mut bytes).map(|_|bytes);let _=out_tx.send(result);});
    // Drain stderr without retaining OAuth URLs or credentials, and never block joining a pipe reader.
    std::thread::spawn(move||{let _=std::io::copy(&mut std::io::BufReader::new(stderr),&mut std::io::sink());});
    let started=Instant::now();
    let status=loop{
        if cancel.load(Ordering::Relaxed)||started.elapsed()>limit {
            #[cfg(windows)]drop(job.take());
            stop_child(&mut child);
            return Err(if cancel.load(Ordering::Relaxed){"已取消本次操作"}else{"钉钉操作等待超时，请重试"}.into());
        }
        match child.try_wait(){
            Ok(Some(status))=>break status,
            Ok(None)=>{},
            Err(_)=>{
                #[cfg(windows)]drop(job.take());
                stop_child(&mut child);return Err("钉钉组件已退出".into());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // A command may exit while a helper still owns stdout; stop owned helpers before waiting for EOF.
    #[cfg(windows)]drop(job.take());
    if !status.success(){let code=status.code().unwrap_or(-1);crate::dingtalk_diagnostics::local("worker","components","failure",&format!("PROCESS_EXIT_{code}"),started.elapsed().as_millis() as u64,"");return Err(format!("钉钉组件退出（代码 {code}），请查看诊断日志"));}
    let bytes=out_rx.recv_timeout(Duration::from_secs(1)).map_err(|_|"钉钉返回结果未能完整读取，请重试")?
        .map_err(|_|"钉钉返回结果读取失败")?;
    if bytes.len() as u64>LIMIT{return Err("钉钉返回的数据过大".into());}
    Ok(bytes)
}
pub(crate) fn run(path:&std::path::Path,args:&[&str],limit:Duration,cancel:&AtomicBool)->Result<Value,String>{
    let bytes=invoke(path,args,limit,cancel)?;
    parse_output(&bytes).ok_or_else(||"无法确认钉钉返回结果，请刷新状态".into())
}
fn parse_output(bytes:&[u8])->Option<Value>{
    serde_json::from_slice::<Value>(bytes).ok().filter(Value::is_object).or_else(||{
        bytes.iter().enumerate().filter(|(_,b)|**b==b'{')
            .find_map(|(i,_)|serde_json::from_slice::<Value>(&bytes[i..]).ok().filter(Value::is_object))
    })
}
fn accounts(value:&Value)->Vec<Account>{
    value["profiles"].as_array().into_iter().flatten().filter_map(|v|{
        let corp=v["corpId"].as_str()?;let user=v["userId"].as_str()?;
        if !valid_id(corp)||!valid_id(user){return None;}
        let text=|key:&str|v[key].as_str().unwrap_or("").chars().filter(|c|!c.is_control()).take(80).collect();
        Some(Account{profile:format!("{corp}:{user}"),organization:text("corpName"),name:text("userName"),current:v["isOrgCurrent"]==true&&v["isCurrent"]==true})
    }).collect()
}
fn select(accounts:&[Account],saved:Option<&str>)->Option<String>{
    if let Some(saved)=saved {return accounts.iter().find(|a|a.profile==saved).map(|a|a.profile.clone());}
    let current:Vec<_>=accounts.iter().filter(|a|a.current).collect();
    (current.len()==1).then(||current[0].profile.clone())
}
fn authenticated(value:&Value,profile:&str)->bool{
    value["authenticated"]==true && value["token_valid"]==true
        && value["corp_id"].as_str().zip(value["user_id"].as_str())
            .is_some_and(|(corp,user)|format!("{corp}:{user}")==profile)
}
fn check(login:bool,selected:Option<String>,cancel:&AtomicBool)->Result<Checked,String>{
    let path=client().ok_or("尚未安装钉钉官方登录组件，请先查看安装说明")?;
    if login {
        // Browser/loopback OAuth; no embedded app secret, token argument, broad recommended grants or auto-approval.
        run(&path,&["auth","login","--format","json"],Duration::from_secs(240),cancel)?;
    }
    let list=run(&path,&["profile","list","--format","json"],Duration::from_secs(40),cancel)?;
    let accounts=accounts(&list);
    let selected=select(&accounts,if login{None}else{selected.as_deref()});
    let mut connected=false;
    if let Some(profile)=&selected {
        let result=run(&path,&["auth","status","--profile",profile,"--format","json"],Duration::from_secs(40),cancel)?;
        connected=authenticated(&result,profile);
    }
    Ok(Checked{accounts,selected,authenticated:connected})
}
const INSTALL_URL:&str="https://raw.githubusercontent.com/DingTalk-Real-AI/dingtalk-workspace-cli/main/scripts/install.ps1";
fn install(cancel:&AtomicBool)->Result<(),String>{
    if client().is_some(){return Ok(());}
    if !cfg!(windows){return Err("一键安装目前仅支持 Windows，请查看官方安装说明".into());}
    let response=ureq::AgentBuilder::new().timeout(Duration::from_secs(40)).build().get(INSTALL_URL).call().map_err(|_|"无法下载官方安装器，请检查网络后重试，或查看官方安装说明")?;
    let mut bytes=Vec::new();response.into_reader().take(512*1024+1).read_to_end(&mut bytes).map_err(|_|"官方安装器下载未完成")?;
    if cancel.load(Ordering::Relaxed){return Err("已取消安装".into());}
    let script=std::str::from_utf8(&bytes).map_err(|_|"安装器内容无效，请稍后重试")?;
    if bytes.len()>512*1024||!script.contains("DWS_NO_SKILLS")||!script.contains("DWS_INSTALL_DIR"){return Err("官方安装器格式已变化，请查看官方安装说明".into());}
    let dir=crate::model::config_dir().join("tools/dws");std::fs::create_dir_all(&dir).map_err(|_|"无法创建组件安装目录")?;
    let script_path=dir.join("install.ps1");crate::model::atomic_write(&script_path,&bytes).map_err(|_|"无法保存官方安装器")?;
    let shell=PathBuf::from(std::env::var_os("SystemRoot").ok_or("无法找到 Windows PowerShell")?).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let location=dir.to_string_lossy();let script_file=script_path.to_string_lossy();
    invoke_env(&shell,&["-NoProfile","-NonInteractive","-ExecutionPolicy","Bypass","-File",&script_file],Duration::from_secs(600),cancel,&[("DWS_INSTALL_DIR",&location),("DWS_NO_SKILLS","1"),("DWS_SKILLS_ONLY","0"),("DWS_VERSION","latest"),("DWS_GITEE_REPO",""),("DWS_GITEE_FALLBACK_REPO","DingTalk-Real-AI/dingtalk-workspace-cli")])
        .map_err(|_|if cancel.load(Ordering::Relaxed){"已取消安装；可重新尝试"}else{"组件安装未完成或超时，请检查网络后重试，或查看官方安装说明"}.to_owned())?;
    let path=client().ok_or("安装器已结束，但未找到组件，请重试或查看安装说明")?;
    let version=invoke(&path,&["--version"],Duration::from_secs(15),cancel)?;
    if !String::from_utf8_lossy(&version).contains("dws"){return Err("组件启动验证未通过，请重试安装".into());}Ok(())
}
impl State {
    pub(crate) fn component_missing(&self)->bool{
        #[cfg(test)]if self.missing_preview{return true;}
        client().is_none()
    }
    fn start_install(&mut self,ctx:&egui::Context){
        if self.installing.is_some()||self.pending.is_some(){return;}
        let (tx,rx)=mpsc::channel();let cancel=Arc::new(AtomicBool::new(false));let flag=cancel.clone();let ctx=ctx.clone();
        match std::thread::Builder::new().name("daozai-dws-install".into()).spawn(move||{let result=install(&flag);let _=tx.send(result);ctx.request_repaint();}){
            Ok(_)=>{self.installing=Some(Installing{rx,cancel});self.note="正在下载并安装手机交互组件，请稍候…".into();self.error=false;},
            Err(_)=>{self.note="无法启动安装，请稍后重试".into();self.error=true;}
        }
    }
    pub(crate) fn account_profile(&self)->Option<&str>{self.authenticated.then_some(self.selected.as_deref()).flatten()}
    pub(crate) fn ensure_checked(&mut self,ctx:&egui::Context){if !self.checked && self.pending.is_none(){self.start(ctx,false);}}
    fn start(&mut self,ctx:&egui::Context,login:bool){
        if self.pending.is_some()||self.installing.is_some(){return;}
        let selected=self.selected.clone();let cancel=Arc::new(AtomicBool::new(false));let flag=cancel.clone();
        let (tx,rx)=mpsc::channel();let ctx=ctx.clone();
        match std::thread::Builder::new().name("daozai-dingtalk-login".into()).spawn(move||{let result=check(login,selected,&flag);let _=tx.send(result);ctx.request_repaint();}) {
            Ok(_)=>{self.pending=Some(Pending{rx,cancel});self.note=if login{"请在打开的浏览器中完成钉钉登录和授权"}else{"正在检查钉钉账号…"}.into();self.error=false;},
            Err(_)=>{self.note="无法启动钉钉登录，请稍后重试".into();self.error=true;},
        }
    }
    pub(crate) fn poll(&mut self,ctx:&egui::Context){
        if let Some(p)=&self.installing{
            match p.rx.try_recv(){
                Ok(result)=>{self.installing=None;match result{Ok(())=>{self.note="组件已安装，正在检查登录状态…".into();self.checked=false;self.error=false;self.start(ctx,false);},Err(e)=>{self.note=e;self.error=true;self.checked=true;}}},
                Err(mpsc::TryRecvError::Empty)=>{ctx.request_repaint_after(Duration::from_millis(250));return;},
                Err(_)=>{self.installing=None;self.note="安装已中断，请重试".into();self.error=true;self.checked=true;}
            }
        }
        let Some(pending)=&self.pending else{return;};
        let result=match pending.rx.try_recv(){Ok(v)=>v,Err(mpsc::TryRecvError::Empty)=>{ctx.request_repaint_after(Duration::from_millis(250));return;},Err(_)=>Err("钉钉登录检查已中断".into())};
        self.pending=None;self.checked=true;
        match result {
            Err(note)=>{self.note=note;self.error=true;self.authenticated=false;},
            Ok(result)=>{
                self.accounts=result.accounts;self.selected=result.selected;self.authenticated=result.authenticated;self.error=false;
                self.note=if self.authenticated{"个人登录已连接；机器人权限以实际发送结果为准"}
                    else if self.selected.is_none()&&!self.accounts.is_empty(){"请选择要用于岛仔的组织和账号"}
                    else{"尚未登录或登录已失效，请登录钉钉"}.into();
                if let Some(profile)=&self.selected {
                    let path=pin_path();
                    let result=std::fs::create_dir_all(path.parent().unwrap()).and_then(|_|crate::model::atomic_write(&path,&serde_json::to_vec(&serde_json::json!({"profile":profile})).unwrap()));
                    if result.is_err(){self.note="账号状态已获取，但本机账号绑定保存失败".into();self.error=true;}
                }
            }
        }
    }
    pub(crate) fn ui(&mut self,ui:&mut egui::Ui){
        use crate::extras::{settings_card,settings_label,settings_hint};
        let available=!self.component_missing();
        if !available||self.installing.is_some(){
            settings_label(ui,"手机交互组件");
            settings_card(ui,|ui|{
                ui.label("使用手机交互需要钉钉官方组件 DWS，当前设备尚未准备好。");
                settings_hint(ui,"点击后从官方来源下载并安装到岛仔的本机工具目录，不安装 AI 技能包。官方安装器可能将组件目录加入当前用户 PATH；无需手动安装 Node.js 或打开终端。");
                if self.installing.is_some(){ui.horizontal(|ui|{ui.spinner();ui.label("正在安装…");});}
                if !self.note.is_empty(){ui.colored_label(if self.error{ui.visuals().error_fg_color}else{ui.visuals().text_color()},&self.note);}
                ui.horizontal_wrapped(|ui|{
                    if ui.add_enabled(self.installing.is_none()&&self.pending.is_none(),egui::Button::new(if self.error{"重试安装"}else{"一键安装组件"})).clicked(){self.start_install(ui.ctx());}
                    if self.installing.is_none()&&ui.button("重新检测").clicked(){self.start(ui.ctx(),false);}
                    ui.hyperlink_to("官方安装说明","https://github.com/DingTalk-Real-AI/dingtalk-workspace-cli#installation");
                });
                if let Some(p)=&self.installing{if ui.button("取消安装").clicked(){p.cancel.store(true,Ordering::Relaxed);}}
            });return;
        }
        if !self.checked && self.pending.is_none()&&!cfg!(test){self.start(ui.ctx(),false);}
        ui.label("使用自己的钉钉账号，通过应用机器人接收任务通知。");
        settings_label(ui,"个人登录授权");
        settings_card(ui,|ui|{
            if self.pending.is_some(){ui.horizontal(|ui|{ui.spinner();ui.label(&self.note);});}
            else {ui.colored_label(if self.error{ui.visuals().error_fg_color}else if self.authenticated{crate::theme::GREEN}else{ui.visuals().text_color()},if self.note.is_empty(){"尚未检查钉钉登录状态"}else{&self.note});}
            let before=self.selected.clone();
            ui.add_enabled_ui(self.pending.is_none(),|ui|{
                if !self.accounts.is_empty(){
                    let current=self.accounts.iter().find(|a|Some(&a.profile)==self.selected.as_ref()).map(|a|format!("{} · {}",a.organization,a.name)).unwrap_or_else(||"选择组织和账号".into());
                    egui::ComboBox::from_id_salt("dingtalk-account").selected_text(current).width(ui.available_width()-20.0).show_ui(ui,|ui|{
                        for account in &self.accounts{ui.selectable_value(&mut self.selected,Some(account.profile.clone()),format!("{} · {}",account.organization,account.name));}
                    });
                }
                ui.horizontal_wrapped(|ui|{
                    if ui.button("登录钉钉").clicked(){self.start(ui.ctx(),true);}
                    if ui.button("刷新状态").clicked(){self.start(ui.ctx(),false);}
                    if self.selected.is_some()&&ui.button("取消绑定").clicked(){
                        match std::fs::remove_file(pin_path()){
                            Ok(())=>self.unbind(),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>self.unbind(),
                            Err(_)=>{self.note="无法移除本机绑定，请稍后重试".into();self.error=true;},
                        }
                    }
                });
            });
            if self.pending.is_none()&&self.selected!=before&&self.selected.is_some(){self.authenticated=false;self.start(ui.ctx(),false);}
            if let Some(p)=&self.pending {if ui.button("取消等待").clicked(){p.cancel.store(true,Ordering::Relaxed);}}
            settings_hint(ui,"登录会打开钉钉官方授权页面，不需要填写账号密码或应用密钥。可复用本机已登录账号；取消绑定不会退出其他程序中的钉钉登录。");
        });
    }
    fn unbind(&mut self){self.selected=None;self.authenticated=false;self.note="已取消岛仔账号绑定，官方组件登录状态保留".into();self.error=false;}
    #[cfg(test)]
    pub(crate) fn preview_missing(&mut self){self.missing_preview=true;self.checked=true;self.accounts.clear();self.selected=None;self.authenticated=false;self.note=String::new();}
    #[cfg(test)]
    pub(crate) fn preview(&mut self){
        self.checked=true;self.accounts=vec![Account{profile:"demo:user".into(),organization:"示例组织".into(),name:"示例用户".into(),current:true}];
        self.selected=Some("demo:user".into());self.authenticated=true;self.note="个人登录已连接；机器人权限以实际发送结果为准".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dingtalk_login_progress_requires_a_final_structured_result(){
        assert_eq!(parse_output(b"Waiting for browser...\n{\"success\":true}\n").unwrap()["success"],true);
        assert!(parse_output(b"Waiting for browser...").is_none());
        assert!(parse_output(b"{\"success\":true} extra").is_none());
    }
    #[test]
    fn dingtalk_accounts_require_explicit_identity_and_do_not_choose_first(){
        let value=serde_json::json!({"profiles":[{"corpId":"corpA","userId":"one","corpName":"A","userName":"甲","isOrgCurrent":true,"isCurrent":false},{"corpId":"corpB","userId":"two","corpName":"B","userName":"乙","isOrgCurrent":true,"isCurrent":true}]});
        let mut all=accounts(&value);
        assert_eq!(select(&all,None).as_deref(),Some("corpB:two"));
        assert_eq!(select(&all,Some("corpA:one")).as_deref(),Some("corpA:one"));
        assert!(select(&all,Some("missing:user")).is_none());
        all[0].current=true;assert!(select(&all,None).is_none());
        assert!(!valid_profile("corp:user,other:user"));
    }
    #[test]
    fn dingtalk_status_requires_valid_token_and_matching_account(){
        let mut value=serde_json::json!({"authenticated":true,"token_valid":true,"corp_id":"corp","user_id":"user","access_token":"never-persist-this"});
        assert!(authenticated(&value,"corp:user"));assert!(!authenticated(&value,"other:user"));
        value["token_valid"]=false.into();assert!(!authenticated(&value,"corp:user"));
    }
    #[test]
    #[ignore="手动只读验证本机官方钉钉登录，不输出账号与令牌"]
    fn dingtalk_local_login_status(){
        let value=check(false,None,&AtomicBool::new(false)).expect("无法检查本机钉钉登录");
        assert!(value.authenticated);assert!(value.selected.is_some());
    }
}


#[cfg(all(test,windows))]
mod process_tests {
    use super::*;
    const FIXTURE:&str="dingtalk_ui::process_tests::dingtalk_process_fixture";
    struct Files(PathBuf);
    impl Files {
        fn new()->Self{let path=std::env::temp_dir().join(format!("daozai-process-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));std::fs::create_dir_all(&path).unwrap();Self(path)}
        fn ready(&self)->PathBuf{self.0.join("child.pid")}
    }
    impl Drop for Files{fn drop(&mut self){let _=std::fs::remove_file(self.ready());let _=std::fs::remove_dir(&self.0);}}
    #[test]
    #[ignore="仅供进程回归测试启动的受控子进程，无外部服务"]
    fn dingtalk_process_fixture(){
        let Ok(mode)=std::env::var("DAOZAI_PROCESS_FIXTURE") else{return;};
        if mode=="leaf"{std::thread::sleep(Duration::from_secs(30));return;}
        let mut command=Command::new(std::env::current_exe().unwrap());
        command.args(["--exact",FIXTURE,"--ignored","--nocapture","--test-threads=1"]).env("DAOZAI_PROCESS_FIXTURE","leaf")
            .stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
        {use std::os::windows::process::CommandExt;command.creation_flags(0x08000000);}
        let child=command.spawn().unwrap();
        std::fs::write(std::env::var_os("DAOZAI_PROCESS_READY").unwrap(),child.id().to_string()).unwrap();
        if mode!="parent-exit"{std::thread::sleep(Duration::from_secs(30));}
    }
    fn assert_child_exited(files:&Files){
        use windows::Win32::{Foundation::CloseHandle,System::Threading::{OpenProcess,GetExitCodeProcess,PROCESS_QUERY_LIMITED_INFORMATION}};
        let pid=std::fs::read_to_string(files.ready()).expect("受控子进程必须先启动").parse::<u32>().unwrap();
        let Ok(handle)=(unsafe{OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION,false,pid)}) else{return;};
        let deadline=Instant::now()+Duration::from_secs(1);let mut code=259;
        while Instant::now()<deadline {
            unsafe{GetExitCodeProcess(handle,&mut code).unwrap();}
            if code!=259{break;}
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe{let _=CloseHandle(handle);}
        assert_ne!(code,259,"操作结束后子进程不能继续运行");
    }
    fn fixture(mode:&str,files:&Files,limit:Duration,cancel:&AtomicBool)->Result<Vec<u8>,String>{
        let ready=files.ready();let ready=ready.to_str().unwrap();
        invoke_env(&std::env::current_exe().unwrap(),&["--exact",FIXTURE,"--ignored","--nocapture","--test-threads=1"],limit,cancel,&[("DAOZAI_PROCESS_FIXTURE",mode),("DAOZAI_PROCESS_READY",ready)])
    }
    #[test]
    fn dingtalk_timeout_ends_descendants_without_waiting_for_their_pipes(){
        let files=Files::new();let started=Instant::now();
        assert!(fixture("parent-wait",&files,Duration::from_secs(1),&AtomicBool::new(false)).unwrap_err().contains("超时"));
        assert!(started.elapsed()<Duration::from_secs(3));assert_child_exited(&files);
    }
    #[test]
    fn dingtalk_cancel_ends_descendants_without_waiting_for_their_pipes(){
        let files=Files::new();let cancel=Arc::new(AtomicBool::new(false));let flag=cancel.clone();let ready=files.ready();
        let setter=std::thread::spawn(move||{let deadline=Instant::now()+Duration::from_secs(3);while !ready.exists()&&Instant::now()<deadline{std::thread::sleep(Duration::from_millis(10));}flag.store(true,Ordering::Relaxed);});
        let started=Instant::now();assert!(fixture("parent-wait",&files,Duration::from_secs(5),&cancel).unwrap_err().contains("取消"));
        setter.join().unwrap();assert!(started.elapsed()<Duration::from_secs(4));assert_child_exited(&files);
    }
    #[test]
    fn dingtalk_parent_exit_closes_descendant_pipes_before_reading_results(){
        let files=Files::new();let started=Instant::now();
        assert!(fixture("parent-exit",&files,Duration::from_secs(5),&AtomicBool::new(false)).is_ok());
        assert!(started.elapsed()<Duration::from_secs(3));assert_child_exited(&files);
    }
}
