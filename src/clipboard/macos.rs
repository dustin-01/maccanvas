use super::{MAX_TEXT, normalize, validate};
use crate::{
    error::{Error, Result},
    macos::*,
};
use zeroize::Zeroizing;

pub fn sequence() -> u32 {
    unsafe { mc_clip_sequence() }
}
pub fn foreground(_: isize) -> bool {
    unsafe { mc_foreground() }
}
pub fn read_snapshot(_: isize) -> Result<Option<(u32, Zeroizing<String>)>> {
    let mut bytes = Zeroizing::new(vec![0u8; MAX_TEXT]);
    let mut count = 0;
    let len = unsafe { mc_clip_read(bytes.as_mut_ptr(), bytes.len(), &mut count) };
    if len == -1 {
        return Ok(None);
    }
    let len = usize::try_from(len)
        .map_err(|_| Error::Clipboard("Clipboard text exceeds the size limit".into()))?;
    let text =
        std::str::from_utf8(&bytes[..len]).map_err(|_| Error::Clipboard("Invalid UTF-8".into()))?;
    let text = Zeroizing::new(normalize(text));
    validate(&text, true)?;
    Ok(Some((count, text)))
}
pub fn read(owner: isize) -> Result<Option<Zeroizing<String>>> {
    Ok(read_snapshot(owner)?.map(|(_, text)| text))
}
pub fn write(owner: isize, text: &str) -> Result<()> {
    write_if_current(owner, text, None).map(|_| ())
}
pub(super) fn write_if_current(_: isize, text: &str, expected: Option<u32>) -> Result<Option<u32>> {
    validate(text, true)?;
    let mut count = 0;
    match unsafe {
        mc_clip_write(
            text.as_ptr(),
            text.len(),
            expected.map_or(-1, i64::from),
            &mut count,
        )
    } {
        0 => Ok(None),
        1 => Ok(Some(count)),
        _ => Err(Error::Clipboard(
            "macOS could not write the clipboard".into(),
        )),
    }
}
pub use super::watcher::Watcher;
