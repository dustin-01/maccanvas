use crate::{
    error::{Error, Result},
    macos::mc_file_dialog,
};
use std::{
    ffi::{CStr, CString, OsStr},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
};

pub fn pick(_: isize) -> Result<Option<PathBuf>> {
    show(None)
}
pub fn save(_: isize, name: &str) -> Result<Option<PathBuf>> {
    let name = CString::new(name).map_err(|_| Error::Transfer("Invalid file name".into()))?;
    show(Some(&name))
}
fn show(name: Option<&CStr>) -> Result<Option<PathBuf>> {
    let mut buffer = [0; libc::PATH_MAX as usize];
    match unsafe {
        mc_file_dialog(
            name.map_or(std::ptr::null(), CStr::as_ptr),
            buffer.as_mut_ptr(),
            buffer.len(),
        )
    } {
        0 => Ok(None),
        1 => Ok(Some(PathBuf::from(OsStr::from_bytes(
            unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_bytes(),
        )))),
        _ => Err(Error::Transfer("Could not read the selected path".into())),
    }
}
