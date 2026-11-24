use uuid::Uuid;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use WindowsVault as Vault;
#[cfg(windows)]
pub use windows::{WindowsHello, WindowsVault};
#[cfg(target_os = "macos")]
mod macos;
use crate::{
    audit::{self, Level, Scope},
    auth::Credentials,
    error::{Error, Result},
};
#[cfg(target_os = "macos")]
use macos::Keychain as Vault;
use serde_json::json;

pub trait CredentialStore {
    fn load(&self, profile: Uuid) -> Result<Credentials>;
    fn save(&self, profile: Uuid, credentials: &Credentials) -> Result<()>;
    fn delete(&self, profile: Uuid) -> Result<()>;
}

pub trait ConsentVerifier {
    fn verify(&self) -> Result<()>;
}

#[derive(Clone)]
pub enum CredentialSource {
    Manual(Credentials),
    Saved,
}

pub fn resolve(
    profile: Uuid,
    source: CredentialSource,
    require_windows_hello: bool,
    consent: &impl ConsentVerifier,
    store: &impl CredentialStore,
) -> Result<Credentials> {
    match source {
        CredentialSource::Saved => {
            if require_windows_hello {
                consent.verify()?;
            }
            store.load(profile)
        }

        CredentialSource::Manual(credentials) => Ok(credentials),
    }
}

pub struct AuditedConsent<'a> {
    pub scope: &'a Scope,
    pub window: isize,
}

impl ConsentVerifier for AuditedConsent<'_> {
    fn verify(&self) -> Result<()> {
        self.scope.critical(
            "security.consent.requested",
            Level::Info,
            json!({"security.mechanism":if cfg!(windows) { "windows_hello" } else { "local_authentication" }}),
        )?;
        #[cfg(target_os = "macos")]
        let result = macos::verify();
        #[cfg(windows)]
        let result = WindowsHello {
            window: self.window,
        }
        .verify();
        outcome(self.scope, "security.consent.completed", result)
    }
}

pub struct AuditedVault<'a> {
    pub scope: &'a Scope,
}

fn outcome<T>(scope: &Scope, name: &'static str, result: Result<T>) -> Result<T> {
    match &result {
        Ok(_) => scope.critical(name, Level::Info, json!({"event.outcome": "success"}))?,
        Err(error) => {
            let mut attributes = audit::error_attributes(error);
            attributes["event.outcome"] = json!("failure");
            scope.critical(name, Level::Warn, attributes)?;
        }
    }
    result
}

impl CredentialStore for AuditedVault<'_> {
    fn load(&self, profile: Uuid) -> Result<Credentials> {
        self.scope.critical(
            "credential.read.requested",
            Level::Info,
            json!({"profile.id": profile}),
        )?;
        outcome(self.scope, "credential.read.completed", Vault.load(profile))
    }

    fn save(&self, profile: Uuid, credentials: &Credentials) -> Result<()> {
        self.scope.critical(
            "credential.save.requested",
            Level::Info,
            json!({"profile.id": profile}),
        )?;
        outcome(
            self.scope,
            "credential.save.completed",
            Vault.save(profile, credentials),
        )
    }

    fn delete(&self, profile: Uuid) -> Result<()> {
        self.scope.critical(
            "credential.delete.requested",
            Level::Info,
            json!({"profile.id": profile}),
        )?;
        outcome(
            self.scope,
            "credential.delete.completed",
            Vault.delete(profile),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Store<'a>(&'a RefCell<Vec<&'static str>>);
    impl CredentialStore for Store<'_> {
        fn load(&self, _: Uuid) -> Result<Credentials> {
            self.0.borrow_mut().push("read");
            Ok(Credentials::new("fixture".into(), "fixture".into()))
        }
        fn save(&self, _: Uuid, _: &Credentials) -> Result<()> {
            Ok(())
        }
        fn delete(&self, _: Uuid) -> Result<()> {
            Ok(())
        }
    }
    struct Consent<'a>(&'a RefCell<Vec<&'static str>>, bool);
    impl ConsentVerifier for Consent<'_> {
        fn verify(&self) -> Result<()> {
            self.0.borrow_mut().push("hello");
            if self.1 {
                Ok(())
            } else {
                Err(Error::Consent("cancelled fixture".into()))
            }
        }
    }

    #[test]
    fn saved_secrets_are_read_only_after_approval() {
        let calls = RefCell::new(Vec::new());
        resolve(
            Uuid::new_v4(),
            CredentialSource::Saved,
            true,
            &Consent(&calls, true),
            &Store(&calls),
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["hello", "read"]);
    }

    #[test]
    fn saved_login_without_hello_reads_the_vault_directly() {
        let calls = RefCell::new(Vec::new());
        resolve(
            Uuid::new_v4(),
            CredentialSource::Saved,
            false,
            &Consent(&calls, false),
            &Store(&calls),
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["read"]);
    }

    #[test]
    fn cancelled_hello_never_reads_the_vault() {
        let calls = RefCell::new(Vec::new());
        assert!(
            resolve(
                Uuid::new_v4(),
                CredentialSource::Saved,
                true,
                &Consent(&calls, false),
                &Store(&calls)
            )
            .is_err()
        );
        assert_eq!(*calls.borrow(), ["hello"]);
    }

    #[test]
    fn manual_credentials_do_not_read_the_vault() {
        let calls = RefCell::new(Vec::new());
        resolve(
            Uuid::new_v4(),
            CredentialSource::Manual(Credentials::new("fixture".into(), "fixture".into())),
            true,
            &Consent(&calls, false),
            &Store(&calls),
        )
        .unwrap();
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn every_saved_login_requires_a_fresh_hello_approval() {
        let calls = RefCell::new(Vec::new());
        let profile = Uuid::new_v4();
        resolve(
            profile,
            CredentialSource::Saved,
            true,
            &Consent(&calls, true),
            &Store(&calls),
        )
        .unwrap();
        assert!(
            resolve(
                profile,
                CredentialSource::Saved,
                true,
                &Consent(&calls, false),
                &Store(&calls)
            )
            .is_err()
        );
        assert_eq!(*calls.borrow(), ["hello", "read", "hello"]);
    }

    #[test]
    fn cloning_a_reconnect_source_cannot_cache_saved_secrets_or_hello_consent() {
        let calls = RefCell::new(Vec::new());
        let source = CredentialSource::Saved;
        let profile = Uuid::new_v4();
        resolve(
            profile,
            source.clone(),
            true,
            &Consent(&calls, true),
            &Store(&calls),
        )
        .unwrap();
        assert!(
            resolve(
                profile,
                source,
                true,
                &Consent(&calls, false),
                &Store(&calls)
            )
            .is_err()
        );
        assert_eq!(*calls.borrow(), ["hello", "read", "hello"]);
    }

    #[test]
    fn unsaved_reconnect_credentials_never_use_the_vault() {
        let calls = RefCell::new(Vec::new());
        let source = CredentialSource::Manual(Credentials::new("fixture".into(), "fixture".into()));
        for reconnect in [source.clone(), source] {
            let credentials = resolve(
                Uuid::new_v4(),
                reconnect,
                true,
                &Consent(&calls, false),
                &Store(&calls),
            )
            .unwrap();
            assert_eq!(credentials.username(), "fixture");
            assert_eq!(credentials.password(), "fixture");
        }
        assert!(calls.borrow().is_empty());
    }

    #[test]
    #[cfg(windows)]
    fn windows_vault_round_trip_uses_only_a_disposable_test_target() {
        let profile = Uuid::new_v4();
        let credentials = Credentials::new("fixture-user".into(), "fixture-password".into());
        Vault.save(profile, &credentials).unwrap();
        let restored = Vault.load(profile);
        Vault.delete(profile).unwrap();
        let restored = restored.unwrap();
        assert_eq!(restored.username(), credentials.username());
        assert_eq!(restored.password(), credentials.password());
    }
}
