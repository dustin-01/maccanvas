use std::ptr;

use super::*;
use ::windows::{
    Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    },
    Win32::{
        Foundation::{ERROR_NOT_FOUND, HWND},
        Security::Credentials::{
            CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
            CredReadW, CredWriteW,
        },
        System::WinRT::{
            IUserConsentVerifierInterop, RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
        },
        UI::WindowsAndMessaging::IsWindow,
    },
    core::{HSTRING, PCWSTR, PWSTR, factory},
};
use windows_future::IAsyncOperation;
use zeroize::{Zeroize, Zeroizing};

pub struct WindowsVault;

fn target(profile: Uuid) -> Vec<u16> {
    format!("MacCanvas/profile/{profile}")
        .encode_utf16()
        .chain(Some(0))
        .collect()
}

struct CredentialAllocation(*mut CREDENTIALW);

impl Drop for CredentialAllocation {
    fn drop(&mut self) {
        unsafe {
            let credential = &mut *self.0;
            if !credential.CredentialBlob.is_null() && credential.CredentialBlobSize > 0 {
                std::slice::from_raw_parts_mut(
                    credential.CredentialBlob,
                    credential.CredentialBlobSize as usize,
                )
                .zeroize();
            }
            CredFree(self.0.cast());
        }
    }
}

impl CredentialStore for WindowsVault {
    fn load(&self, profile: Uuid) -> Result<Credentials> {
        let name = target(profile);
        let mut pointer = ptr::null_mut();
        unsafe { CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None, &mut pointer) }
            .map_err(|error| {
                if error.code() == ERROR_NOT_FOUND.to_hresult() {
                    Error::Credential("Saved credentials were removed. Enter them manually.".into())
                } else {
                    Error::Credential(error.to_string())
                }
            })?;
        if pointer.is_null() {
            return Err(Error::Credential(
                "Windows returned an invalid credential".into(),
            ));
        }
        let allocation = CredentialAllocation(pointer);
        let raw = unsafe { &*allocation.0 };
        if raw.CredentialBlobSize == 0
            || raw.CredentialBlobSize > 256
            || raw.CredentialBlob.is_null()
        {
            return Err(Error::Credential(
                "Saved password has an invalid size".into(),
            ));
        }
        let mut password = Zeroizing::new(
            unsafe {
                std::slice::from_raw_parts(raw.CredentialBlob, raw.CredentialBlobSize as usize)
            }
            .to_vec(),
        );
        let password_string = std::str::from_utf8(&password)
            .map_err(|_| Error::Credential("Saved password is not UTF-8".into()))?
            .to_owned();
        password.zeroize();
        let username = if raw.UserName.is_null() {
            String::new()
        } else {
            unsafe { raw.UserName.to_string() }
                .map_err(|_| Error::Credential("Saved username is invalid".into()))?
        };
        Ok(Credentials::new(username, password_string))
    }

    fn save(&self, profile: Uuid, credentials: &Credentials) -> Result<()> {
        if credentials.password().is_empty() || credentials.password().len() > 256 {
            return Err(Error::Credential("Password has an invalid size".into()));
        }
        let mut name = target(profile);
        let mut username: Vec<u16> = credentials
            .username()
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut blob = Zeroizing::new(credentials.password().as_bytes().to_vec());
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(name.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(username.as_mut_ptr()),
            ..CREDENTIALW::default()
        };
        let result = unsafe { CredWriteW(&credential, 0) }
            .map_err(|error| Error::Credential(error.to_string()));
        username.zeroize();
        result
    }

    fn delete(&self, profile: Uuid) -> Result<()> {
        let name = target(profile);
        match unsafe { CredDeleteW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(()),
            Err(error) => Err(Error::Credential(error.to_string())),
        }
    }
}

pub struct WindowsHello {
    pub window: isize,
}

struct Runtime;

impl Runtime {
    fn initialize() -> Result<Self> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .map_err(|error| Error::Consent(error.to_string()))?;
        Ok(Self)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

impl ConsentVerifier for WindowsHello {
    fn verify(&self) -> Result<()> {
        let _runtime = Runtime::initialize()?;
        let window = HWND(self.window as *mut _);
        if self.window == 0 || !unsafe { IsWindow(Some(window)) }.as_bool() {
            return Err(Error::Consent(
                "The connection window is no longer available".into(),
            ));
        }
        let availability = UserConsentVerifier::CheckAvailabilityAsync()
            .and_then(|operation| operation.join())
            .map_err(|error| Error::Consent(error.to_string()))?;
        if availability != UserConsentVerifierAvailability::Available {
            return Err(Error::Consent("Set up Windows Hello in Windows Settings, or enter your Mac credentials manually instead of using the saved password.".into()));
        }
        let verifier = factory::<UserConsentVerifier, IUserConsentVerifierInterop>()
            .map_err(|error| Error::Consent(error.to_string()))?;
        let operation: IAsyncOperation<UserConsentVerificationResult> = unsafe {
            verifier.RequestVerificationForWindowAsync(
                window,
                &HSTRING::from("Verify to connect with MacCanvas credentials"),
            )
        }
        .map_err(|error| Error::Consent(error.to_string()))?;
        let result = operation
            .join()
            .map_err(|error| Error::Consent(error.to_string()))?;
        if result != UserConsentVerificationResult::Verified {
            return Err(Error::Consent("Verification was not approved. No saved password was read and no connection was opened.".into()));
        }
        Ok(())
    }
}
