//! Read only native Codex receipts. Missing/locked/changed schemas never acknowledge tasks.
use std::{collections::HashMap,ffi::{c_char,c_int,c_void,CStr,CString},path::Path,ptr};
type Callback=unsafe extern "C" fn(*mut c_void,c_int,*mut *mut c_char,*mut *mut c_char)->c_int;
#[link(name="winsqlite3")]
extern "C" {
    fn sqlite3_open_v2(name:*const c_char,db:*mut *mut c_void,flags:c_int,vfs:*const c_char)->c_int;
    fn sqlite3_exec(db:*mut c_void,sql:*const c_char,callback:Option<Callback>,data:*mut c_void,error:*mut *mut c_char)->c_int;
    fn sqlite3_close(db:*mut c_void)->c_int;
}
pub(super) fn receipts(home:&Path)->HashMap<String,String> {
    let mut result=HashMap::new();
    let path=home.join("state_5.sqlite");
    if !path.is_file(){return result;}
    let Ok(name)=CString::new(path.to_string_lossy().as_bytes()) else{return result;};
    let sql=CString::new("SELECT thread_id,last_published_turn FROM thread_read_receipts WHERE first_unread_turn IS NULL AND last_published_turn IS NOT NULL LIMIT 10000").unwrap();
    unsafe extern "C" fn row(data:*mut c_void,n:c_int,values:*mut *mut c_char,_:*mut *mut c_char)->c_int {
        unsafe {
            if n==2 && !(*values).is_null() && !(*values.add(1)).is_null() {
                if let (Ok(id),Ok(turn))=(CStr::from_ptr(*values).to_str(),CStr::from_ptr(*values.add(1)).to_str()) {
                    (*data.cast::<HashMap<String,String>>()).insert(id.to_owned(),turn.to_owned());
                }
            }
        }
        0
    }
    let mut db=ptr::null_mut();
    unsafe {
        let opened=sqlite3_open_v2(name.as_ptr(),&mut db,1,ptr::null())==0;
        let ok=opened && sqlite3_exec(db,sql.as_ptr(),Some(row),(&mut result as *mut HashMap<String,String>).cast(),ptr::null_mut())==0;
        if !db.is_null(){sqlite3_close(db);}
        if !ok {result.clear();}
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ai_native_receipts_only_return_explicitly_read_published_turns() {
        let home=std::env::temp_dir().join(format!("daozai-receipts-{}",std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        assert!(receipts(&home).is_empty());
        let name=CString::new(home.join("state_5.sqlite").to_string_lossy().as_bytes()).unwrap();
        let mut db=ptr::null_mut();
        unsafe {
            assert_eq!(sqlite3_open_v2(name.as_ptr(),&mut db,6,ptr::null()),0);
            let sql=CString::new("CREATE TABLE thread_read_receipts(thread_id TEXT,first_unread_turn TEXT,last_published_turn TEXT); INSERT INTO thread_read_receipts VALUES('read',NULL,'turn-1'),('unread','turn-2','turn-2'),('unknown',NULL,NULL);").unwrap();
            assert_eq!(sqlite3_exec(db,sql.as_ptr(),None,ptr::null_mut(),ptr::null_mut()),0);
            sqlite3_close(db);
        }
        assert_eq!(receipts(&home),HashMap::from([("read".into(),"turn-1".into())]));
        std::fs::remove_file(home.join("state_5.sqlite")).unwrap();
        std::fs::remove_dir(home).unwrap();
    }
}
