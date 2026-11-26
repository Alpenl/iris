//! Explicit command receipts for resources that must outlive native work.
use std::ffi::{CString, c_char};
use std::ptr;

use super::{Error, Mpv, Result, check, cstr, sys};
impl Mpv {
    pub fn command_with_reply(&self, id: u64, args: &[&str]) -> Result<()> {
        if id == 0 {
            return Err(Error {
                code: sys::MPV_ERROR_INVALID_PARAMETER,
                context: "zero command receipt".into(),
            });
        }
        let owned = args
            .iter()
            .map(|arg| cstr(arg))
            .collect::<Result<Vec<CString>>>()?;
        let mut pointers: Vec<*const c_char> = owned.iter().map(|arg| arg.as_ptr()).collect();
        pointers.push(ptr::null());
        // SAFETY: a live handle and NULL-terminated borrowed strings; libmpv
        // copies the command before returning. The receipt contains no pointers.
        check(
            unsafe { sys::mpv_command_async(self.handle, id, pointers.as_mut_ptr()) },
            || "command_with_reply".into(),
        )
    }
}
