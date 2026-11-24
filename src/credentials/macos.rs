use super::*;
use crate::macos::*;
use std::ffi::CString;
use zeroize::Zeroizing;

pub(super) struct Keychain;
fn account(id: Uuid) -> CString {
    CString::new(id.to_string()).expect("UUID is ASCII")
}
fn check(status: i32) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error::Credential(format!(
            "Keychain returned OSStatus {status}"
        )))
    }
}

impl CredentialStore for Keychain {
    fn load(&self, id: Uuid) -> Result<Credentials> {
        let mut data = Zeroizing::new([0u8; 1024]);
        let mut size = 0;
        check(unsafe {
            mc_vault_load(
                account(id).as_ptr(),
                data.as_mut_ptr(),
                data.len(),
                &mut size,
            )
        })?;
        let text = std::str::from_utf8(&data[..size])
            .map_err(|_| Error::Credential("Invalid Keychain entry".into()))?;
        let (user, password) = text
            .split_once('\0')
            .ok_or_else(|| Error::Credential("Incomplete Keychain entry".into()))?;
        Ok(Credentials::new(user.into(), password.into()))
    }
    fn save(&self, id: Uuid, credentials: &Credentials) -> Result<()> {
        let data = Zeroizing::new(format!(
            "{}\0{}",
            credentials.username(),
            credentials.password()
        ));
        if data.len() > 1024 {
            return Err(Error::Credential(
                "Login exceeds the Keychain entry limit".into(),
            ));
        }
        check(unsafe { mc_vault_save(account(id).as_ptr(), data.as_ptr(), data.len()) })
    }
    fn delete(&self, id: Uuid) -> Result<()> {
        check(unsafe { mc_vault_delete(account(id).as_ptr()) })
    }
}

pub(super) fn verify() -> Result<()> {
    if unsafe { mc_consent() } {
        Ok(())
    } else {
        Err(Error::Consent(
            "Device owner verification was unavailable or cancelled".into(),
        ))
    }
}
