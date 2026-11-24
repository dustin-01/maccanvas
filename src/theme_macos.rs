use crate::{error::Result, macos::*};
pub fn system_dark() -> Result<bool> {
    Ok(unsafe { mc_dark() })
}
pub fn motion_enabled() -> Result<bool> {
    Ok(unsafe { mc_motion() })
}
pub fn glass_enabled() -> Result<bool> {
    Ok(unsafe { mc_glass() })
}
