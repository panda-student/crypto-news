//! SQLite authorizer：把一次操作限制在允许的表内。

use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr;

use libsqlite3_sys as ffi;

struct Ctx {
    /// 小写表名
    tables: Vec<String>,
}

/// 设置在某个连接上的 authorizer，析构时清除并释放上下文。
pub(crate) struct AuthGuard {
    db: *mut ffi::sqlite3,
    ctx: *mut Ctx,
}

// 守卫只在持有该连接的任务里使用
unsafe impl Send for AuthGuard {}

impl AuthGuard {
    /// # Safety
    /// `db` 必须是有效的连接句柄，且在守卫析构前保持有效、不被其他线程同时使用。
    pub(crate) unsafe fn install(db: *mut ffi::sqlite3, tables: &[String]) -> AuthGuard {
        let ctx = Box::into_raw(Box::new(Ctx {
            tables: tables.iter().map(|t| t.to_ascii_lowercase()).collect(),
        }));
        ffi::sqlite3_set_authorizer(db, Some(callback), ctx.cast());
        AuthGuard { db, ctx }
    }
}

impl Drop for AuthGuard {
    fn drop(&mut self) {
        // SAFETY：见 install
        unsafe {
            ffi::sqlite3_set_authorizer(self.db, None, ptr::null_mut());
            drop(Box::from_raw(self.ctx));
        }
    }
}

unsafe fn text<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        CStr::from_ptr(p).to_str().ok()
    }
}

fn is_internal(name: &str) -> bool {
    matches!(
        name,
        "sqlite_master"
            | "sqlite_schema"
            | "sqlite_temp_master"
            | "sqlite_temp_schema"
            | "sqlite_sequence"
    )
}

unsafe extern "C" fn callback(
    user: *mut c_void,
    action: c_int,
    a1: *const c_char,
    a2: *const c_char,
    _db: *const c_char,
    _trigger: *const c_char,
) -> c_int {
    let ctx = &*(user as *const Ctx);
    let allowed = |name: Option<&str>| match name {
        Some(n) => {
            let n = n.to_ascii_lowercase();
            is_internal(&n) || ctx.tables.contains(&n)
        }
        None => false,
    };
    let ok = match action {
        // 表级读写：arg1 是表名
        ffi::SQLITE_READ
        | ffi::SQLITE_INSERT
        | ffi::SQLITE_UPDATE
        | ffi::SQLITE_DELETE
        | ffi::SQLITE_CREATE_TABLE
        | ffi::SQLITE_DROP_TABLE
        | ffi::SQLITE_ANALYZE => allowed(text(a1)),
        // arg2 是表名
        ffi::SQLITE_CREATE_INDEX
        | ffi::SQLITE_DROP_INDEX
        | ffi::SQLITE_CREATE_TRIGGER
        | ffi::SQLITE_DROP_TRIGGER
        | ffi::SQLITE_ALTER_TABLE => allowed(text(a2)),
        ffi::SQLITE_SELECT
        | ffi::SQLITE_FUNCTION
        | ffi::SQLITE_RECURSIVE
        | ffi::SQLITE_TRANSACTION
        | ffi::SQLITE_SAVEPOINT
        // 建索引时 SQLite 会内部触发 REINDEX，只重建索引，不涉及数据
        | ffi::SQLITE_REINDEX => true,
        // ATTACH、DETACH、PRAGMA、虚表、视图、临时对象等一律拒绝
        _ => false,
    };
    if ok {
        ffi::SQLITE_OK
    } else {
        ffi::SQLITE_DENY
    }
}
