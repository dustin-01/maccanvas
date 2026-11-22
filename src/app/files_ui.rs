use std::{cell::RefCell, rc::Rc, sync::atomic::Ordering};

use maccanvas::{
    audit::{Level, Scope},
    auth::Credentials,
    clipboard::{self, native},
    credentials::CredentialSource,
    error::{Error, Result},
    files::{self, Client, Command, Entry, Event, Request},
    profiles::{AuthMode, Profile, SshHostKey},
    telemetry,
};
use serde_json::json;
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::sync::oneshot;

use super::{State, window_handle};
use crate::{App, FileRow};

#[derive(Clone, Copy)]
enum ClipboardAction {
    Copy,
    Send,
}

// The file worker can finish a transfer while its desktop is in the background.
#[derive(Default)]
pub(super) struct Panel {
    host: slint::SharedString,
    port: slint::SharedString,
    has_saved: bool,
    use_saved: bool,
    connected: bool,
    directory_ready: bool,
    connecting: bool,
    busy: bool,
    key_pending: bool,
    fingerprint: slint::SharedString,
    key_algorithm: slint::SharedString,
    key_command: slint::SharedString,
    path: slint::SharedString,
    upload_name: slint::SharedString,
    status: slint::SharedString,
    error: slint::SharedString,
    progress_label: slint::SharedString,
    progress: f32,
}

impl Panel {
    pub(super) fn capture(app: &App) -> Self {
        Self {
            host: app.get_files_host(),
            port: app.get_files_port(),
            has_saved: app.get_files_has_saved(),
            use_saved: app.get_files_use_saved(),
            connected: app.get_files_connected(),
            directory_ready: app.get_files_directory_ready(),
            connecting: app.get_files_connecting(),
            busy: app.get_files_busy(),
            key_pending: app.get_files_key_pending(),
            fingerprint: app.get_files_fingerprint(),
            key_algorithm: app.get_files_key_algorithm(),
            key_command: app.get_files_key_command(),
            path: app.get_files_path(),
            upload_name: app.get_files_upload_name(),
            status: app.get_files_status(),
            error: app.get_files_error(),
            progress_label: app.get_files_progress_label(),
            progress: app.get_files_progress(),
        }
    }

    pub(super) fn show(&self, app: &App, files: &Files) {
        app.set_files_username("".into());
        app.set_files_password("".into());
        app.set_files_host(self.host.clone());
        app.set_files_port(self.port.clone());
        app.set_files_has_saved(self.has_saved);
        app.set_files_use_saved(self.use_saved);
        app.set_files_connected(self.connected);
        app.set_files_directory_ready(self.directory_ready);
        app.set_files_connecting(self.connecting);
        app.set_files_busy(self.busy);
        app.set_files_key_pending(self.key_pending);
        app.set_files_fingerprint(self.fingerprint.clone());
        app.set_files_key_algorithm(self.key_algorithm.clone());
        app.set_files_key_command(self.key_command.clone());
        app.set_files_path(self.path.clone());
        app.set_files_upload_name(self.upload_name.clone());
        app.set_files_status(self.status.clone());
        app.set_files_error(self.error.clone());
        app.set_files_progress_label(self.progress_label.clone());
        app.set_files_progress(self.progress);
        rows(app, files);
    }
}

#[derive(Default)]
pub(super) struct Files {
    client: Option<Client>,
    retiring: Vec<Client>,
    profile: Option<Profile>,
    scope: Option<Scope>,
    directory: String,
    entries: Vec<Entry>,
    approval: Option<(SshHostKey, oneshot::Sender<bool>)>,
    last_outcome: Option<String>,
    received: u64,
    sent: u64,
    baseline: (u64, u64),
    pending_clipboard: Option<ClipboardAction>,
    // The SSH worker accepts one UI operation at a time. Only a copy started
    // since this desktop last gained focus may write its response to Windows.
    apply_clipboard_read: bool,
}

impl Files {
    pub fn clear_pending_clipboard(&mut self) {
        self.pending_clipboard = None;
        self.apply_clipboard_read = false;
    }

    pub fn retire(&mut self) {
        self.approval = None;
        self.clear_pending_clipboard();
        if let Some(client) = self.client.take() {
            client.close();
            self.retiring.push(client);
        }
    }

    pub fn stop(&mut self, app: &App) {
        self.retire();
        app.set_files_connected(false);
        app.set_files_connecting(false);
        app.set_files_busy(false);
        app.set_files_key_pending(false);
        app.set_files_password("".into());
        app.set_files_username("".into());
        self.directory.clear();
        self.entries.clear();
        self.last_outcome = None;
        app.set_files_path(".".into());
        app.set_files_directory_ready(false);
        rows(app, self);
    }

    pub fn finished(&self) -> bool {
        self.retiring.iter().all(Client::is_finished)
    }

    pub fn finish(&mut self) -> Result<()> {
        self.retire();
        for client in std::mem::take(&mut self.retiring) {
            client.finish()?;
        }
        Ok(())
    }

    pub fn active(&self) -> bool {
        self.client.is_some() || !self.retiring.is_empty()
    }

    fn total(&self) -> (u64, u64) {
        self.client.iter().chain(self.retiring.iter()).fold(
            (self.received, self.sent),
            |(received, sent), client| {
                (
                    received.saturating_add(client.counters.received.load(Ordering::Relaxed)),
                    sent.saturating_add(client.counters.sent.load(Ordering::Relaxed)),
                )
            },
        )
    }

    pub fn reset_network(&mut self) {
        self.baseline = self.total();
    }

    pub fn network(&self) -> (u64, u64) {
        let (received, sent) = self.total();
        (
            received.saturating_sub(self.baseline.0),
            sent.saturating_sub(self.baseline.1),
        )
    }

    fn command(&self, app: &App, command: Command) -> Result<()> {
        if app.get_files_busy() {
            return Err(Error::Transfer(
                "Wait for the current operation or cancel it first".into(),
            ));
        }
        self.client
            .as_ref()
            .ok_or_else(|| Error::Transfer("Connect SSH first".into()))?
            .command(command)?;
        app.set_files_busy(true);
        app.set_files_error("".into());
        app.set_files_progress(0.0);
        app.set_files_progress_label("Starting...".into());
        Ok(())
    }
}

fn fail(app: &App, error: impl std::fmt::Display) {
    app.set_files_error(error.to_string().into());
    if !app.get_files_open() && !app.get_clipboard_open() {
        super::notify(app, error);
    }
}

fn rows(app: &App, files: &Files) {
    app.set_file_entries(ModelRc::from(Rc::new(VecModel::from(
        files
            .entries
            .iter()
            .map(|entry| FileRow {
                name: entry.name.clone().into(),
                directory: entry.directory,
                regular: entry.regular,
                detail: if entry.directory {
                    "Folder".into()
                } else if entry.regular {
                    entry
                        .size
                        .map_or_else(|| "File (size unavailable)".into(), size)
                } else {
                    "Link or special file - not followed".into()
                }
                .into(),
            })
            .collect::<Vec<_>>(),
    ))));
}

fn size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn scope(files: &Files) -> Result<&Scope> {
    files
        .scope
        .as_ref()
        .ok_or_else(|| Error::Transfer("SSH operation context is unavailable".into()))
}

fn prepare_ssh(app: &App, state: &mut State) -> Result<()> {
    if !app.get_connected() {
        return Err(Error::Transfer("Connect to your Mac's screen first".into()));
    }
    state.release()?;
    if state.active.files.client.is_none() {
        let profile = state
            .active
            .connection_profile
            .clone()
            .ok_or_else(|| Error::Transfer("The active Mac is unavailable".into()))?;
        let saved = profile.authentication == AuthMode::MacLogin && profile.has_saved_credentials;
        app.set_files_host(profile.host.clone().into());
        app.set_files_has_saved(saved);
        app.set_files_use_saved(saved);
        app.set_files_password("".into());
        app.set_files_username("".into());
        app.set_files_port("22".into());
        app.set_files_error("".into());
        app.set_files_status("SSH is shared by file transfer and clipboard.".into());
        state.active.files.profile = Some(profile);
    }
    Ok(())
}

fn clipboard_command(app: &App, state: &mut State, action: ClipboardAction) -> Result<()> {
    let command = match action {
        ClipboardAction::Copy => Command::CopyClipboard,
        ClipboardAction::Send => {
            let (sequence, text) =
                native::read_snapshot(window_handle(app)?)?.ok_or_else(|| {
                    Error::Clipboard("Windows has no plain text on the clipboard".into())
                })?;
            Command::SendClipboard {
                text: zeroize::Zeroizing::new(clipboard::normalize(&text)),
                sequence,
            }
        }
    };
    state.active.files.command(app, command)?;
    state.active.files.apply_clipboard_read = matches!(action, ClipboardAction::Copy);
    Ok(())
}

fn request_clipboard(app: &App, state: &mut State, action: ClipboardAction) -> Result<()> {
    state.release()?;
    if app.get_files_connected() {
        clipboard_command(app, state, action)?;
        if !app.get_modal_open() {
            app.invoke_focus_canvas();
        }
    } else {
        prepare_ssh(app, state)?;
        state.active.files.pending_clipboard = Some(action);
        app.set_files_open(false);
        app.set_clipboard_open(true);
    }
    Ok(())
}

pub(super) fn wire(app: &App, state: Rc<RefCell<State>>) {
    let weak = app.as_weak();
    let open = state.clone();
    app.on_open_files(move || {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let result: Result<()> = (|| {
            let mut state = open.borrow_mut();
            prepare_ssh(&app, &mut state)?;
            app.set_clipboard_open(false);
            app.set_files_open(true);
            Ok(())
        })();
        if let Err(error) = result {
            fail(&app, error);
            app.set_files_open(true);
        }
    });

    let weak = app.as_weak();
    let clipboard = state.clone();
    app.on_open_clipboard(move || {
        let Some(app) = weak.upgrade() else {
            return;
        };
        if let Err(error) = prepare_ssh(&app, &mut clipboard.borrow_mut()) {
            fail(&app, error);
        }
        app.set_files_open(false);
        app.set_clipboard_open(true);
    });
    let weak = app.as_weak();
    let clipboard_closing = state.clone();
    app.on_close_clipboard(move || {
        if let Some(app) = weak.upgrade() {
            clipboard_closing
                .borrow_mut()
                .active
                .files
                .pending_clipboard = None;
            if app.get_files_connecting() || app.get_files_key_pending() {
                clipboard_closing.borrow_mut().active.files.stop(&app);
            }
            app.set_clipboard_open(false);
            if app.get_connected() {
                app.invoke_focus_canvas();
            }
        }
    });

    let weak = app.as_weak();
    let connecting = state.clone();
    app.on_files_connect(move || {
        let Some(app) = weak.upgrade() else { return; };
        let result = (|| {
            let mut state = connecting.borrow_mut();
            if !app.get_connected() || state.updater.installing { return Err(Error::Transfer("Keep the Mac connection open while using SSH".into())); }
            if state.active.files.client.is_some() { return Err(Error::Transfer("SSH is already connected or connecting".into())); }
            if state.active.files.retiring.len() >= 4 { return Err(Error::Transfer("Wait for the previous SSH connection to close".into())); }
            let profile = state.active.files.profile.clone().ok_or_else(|| Error::Transfer("Open Files or Clipboard from an active Mac connection".into()))?;
            let port = app.get_files_port().parse::<u16>().ok().filter(|&port| port != 0)
                .ok_or_else(|| Error::Transfer("SSH port must be 1-65535".into()))?;
            let credentials = if app.get_files_use_saved() { CredentialSource::Saved } else {
                let credentials = Credentials::new(app.get_files_username().to_string(),app.get_files_password().to_string());
                credentials.validate(AuthMode::MacLogin)?;
                CredentialSource::Manual(credentials)
            };
            let scope = state.audit.child(json!({"profile.id":profile.id,"server.address":profile.host,"server.port":port,"network.protocol.name":"ssh"}));
            let pinned = state.settings.ssh_host_keys.iter().find(|key| key.endpoint(&profile.host,port)).cloned();
            let client = Client::start(Request { profile,port,credentials,pinned,window:window_handle(&app)?,audit:scope.clone() })?;
            state.active.files.scope = Some(scope); state.active.files.client = Some(client);
            app.set_files_connecting(true); app.set_files_error("".into()); app.set_files_status("Verifying the Mac's SSH identity before using credentials...".into());
            Ok(())
        })();
        app.set_files_password("".into()); app.set_files_username("".into());
        if let Err(error) = result { fail(&app,error); }
    });

    let weak = app.as_weak();
    let approving = state.clone();
    app.on_files_approve(move |approved| {
        let Some(app) = weak.upgrade() else { return; };
        let result = (|| {
            let mut state = approving.borrow_mut();
            let (key,reply) = state.active.files.approval.take().ok_or_else(|| Error::Transfer("No host-key approval is pending".into()))?;
            if approved {
                key.validate()?;
                if let Some(existing) = state.settings.ssh_host_keys.iter().find(|old| old.endpoint(&key.host,key.port)) {
                    if existing.algorithm != key.algorithm || existing.sha256 != key.sha256 {
                        return Err(Error::Transfer("The existing host key cannot be replaced from an approval prompt".into()));
                    }
                } else {
                    let old = state.settings.ssh_host_keys.clone();
                    state.settings.ssh_host_keys.push(key.clone());
                    if let Err(error) = state.persist() { state.settings.ssh_host_keys = old; return Err(error); }
                }
                scope(&state.active.files)?.critical("ssh.host_key.pinned",Level::Info,json!({"ssh.host_key.fingerprint.sha256":key.sha256,"server.address":key.host,"server.port":key.port}))?;
            }
            reply.send(approved).map_err(|_| Error::Transfer("The SSH approval request is no longer active".into()))?;
            Ok(())
        })();
        app.set_files_key_pending(false);
        if let Err(error) = result { fail(&app,error); }
    });

    let weak = app.as_weak();
    let disconnecting = state.clone();
    app.on_files_disconnect(move || {
        if let Some(app) = weak.upgrade() {
            disconnecting.borrow_mut().active.files.stop(&app);
            app.set_files_status("SSH disconnected.".into());
        }
    });
    let weak = app.as_weak();
    let closing = state.clone();
    app.on_files_close(move || {
        if let Some(app) = weak.upgrade() {
            if app.get_files_connecting() || app.get_files_key_pending() {
                closing.borrow_mut().active.files.stop(&app);
            }
            app.set_files_open(false);
            if app.get_connected() {
                app.invoke_focus_canvas();
            }
        }
    });
    let weak = app.as_weak();
    let browsing = state.clone();
    app.on_files_browse(move || {
        if let Some(app) = weak.upgrade()
            && let Err(error) = browsing
                .borrow()
                .active
                .files
                .command(&app, Command::List(app.get_files_path().to_string()))
        {
            fail(&app, error);
        }
    });
    let weak = app.as_weak();
    let up = state.clone();
    app.on_files_up(move || {
        if let Some(app) = weak.upgrade() {
            let state = up.borrow();
            if let Err(error) = state.active.files.command(
                &app,
                Command::List(files::parent(&state.active.files.directory)),
            ) {
                fail(&app, error);
            }
        }
    });
    let weak = app.as_weak();
    let entering = state.clone();
    app.on_files_entry(move |index| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let result = (|| {
            let (entry, remote, audit) = {
                let state = entering.borrow();
                let entry = usize::try_from(index)
                    .ok()
                    .and_then(|index| state.active.files.entries.get(index))
                    .ok_or_else(|| {
                        Error::Transfer("The selected file is no longer in this folder".into())
                    })?
                    .clone();
                let remote = files::join(&state.active.files.directory, &entry.name)?;
                if entry.directory {
                    return state.active.files.command(&app, Command::List(remote));
                }
                if !entry.regular {
                    return Err(Error::Transfer(
                        "Links and special files cannot be received".into(),
                    ));
                }
                (entry, remote, scope(&state.active.files)?.clone())
            };
            let local = files::dialog::save(window_handle(&app)?, &safe_name(&entry.name))?;
            if let Some(local) = local {
                entering
                    .borrow()
                    .active
                    .files
                    .command(&app, Command::Download { remote, local })?;
            } else {
                audit.event(
                    "file.dialog.cancelled",
                    Level::Info,
                    json!({"file.transfer.direction":"download"}),
                )?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            fail(&app, error);
        }
    });
    let weak = app.as_weak();
    let uploading = state.clone();
    app.on_files_upload(move || {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let result = (|| {
            let (directory, audit) = {
                let state = uploading.borrow();
                if state.active.files.directory.is_empty() {
                    return Err(Error::Transfer(
                        "Open an accessible Mac folder before sending a file".into(),
                    ));
                }
                (
                    state.active.files.directory.clone(),
                    scope(&state.active.files)?.clone(),
                )
            };
            let local = files::dialog::pick(window_handle(&app)?)?;
            let Some(local) = local else {
                audit.event(
                    "file.dialog.cancelled",
                    Level::Info,
                    json!({"file.transfer.direction":"upload"}),
                )?;
                return Ok(());
            };
            let name = if app.get_files_upload_name().is_empty() {
                local
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        Error::Transfer(
                            "The Windows file name cannot be represented as UTF-8".into(),
                        )
                    })?
                    .to_owned()
            } else {
                app.get_files_upload_name().to_string()
            };
            files::valid_name(&name)?;
            uploading.borrow().active.files.command(
                &app,
                Command::Upload {
                    local,
                    directory,
                    name,
                },
            )
        })();
        if let Err(error) = result {
            fail(&app, error);
        }
    });
    let weak = app.as_weak();
    let cancelling = state.clone();
    app.on_files_cancel(move || {
        if let Some(app) = weak.upgrade() {
            let state = cancelling.borrow();
            if let Some(client) = &state.active.files.client {
                client.cancel();
                app.set_files_status("Cancelling and cleaning up temporary files...".into());
            }
        }
    });
    let weak = app.as_weak();
    let copying = state.clone();
    app.on_files_copy_text(move || {
        if let Some(app) = weak.upgrade()
            && let Err(error) =
                request_clipboard(&app, &mut copying.borrow_mut(), ClipboardAction::Copy)
        {
            fail(&app, error);
        }
    });
    let weak = app.as_weak();
    let sending = state;
    app.on_files_send_text(move || {
        let Some(app) = weak.upgrade() else {
            return;
        };
        if let Err(error) =
            request_clipboard(&app, &mut sending.borrow_mut(), ClipboardAction::Send)
        {
            fail(&app, error);
        }
    });
}

pub(super) fn tick(app: &App, state: &mut State) -> Result<()> {
    while let Some(index) = state
        .active
        .files
        .retiring
        .iter()
        .position(Client::is_finished)
    {
        let client = state.active.files.retiring.remove(index);
        state.active.files.received = state
            .active
            .files
            .received
            .saturating_add(client.counters.received.load(Ordering::Relaxed));
        state.active.files.sent = state
            .active
            .files
            .sent
            .saturating_add(client.counters.sent.load(Ordering::Relaxed));
        client.finish()?;
    }
    while let Some(event) = state
        .active
        .files
        .client
        .as_ref()
        .map(Client::event)
        .transpose()?
        .flatten()
    {
        match event {
            Event::HostKey { key, reply } => {
                app.set_files_fingerprint(key.sha256.clone().into());
                app.set_files_key_algorithm(key.algorithm.clone().into());
                let name = if key.algorithm == "ssh-ed25519" {
                    "ed25519"
                } else {
                    "ecdsa"
                };
                app.set_files_key_command(
                    format!("ssh-keygen -lf /etc/ssh/ssh_host_{name}_key.pub -E sha256").into(),
                );
                state.active.files.approval = Some((key, reply));
                app.set_files_key_pending(true);
                if !app.get_clipboard_open() {
                    app.set_files_open(true);
                }
            }
            Event::Connected => {
                state.active.files.directory.clear();
                state.active.files.entries.clear();
                rows(app, &state.active.files);
                app.set_files_path(".".into());
                app.set_files_directory_ready(false);
                app.set_files_connected(true);
                app.set_files_connecting(false);
                app.set_files_busy(true);
                app.set_files_status("SSH host verified. Reading your Mac folder...".into());
                if app.get_clipboard_open() {
                    app.set_clipboard_open(false);
                    app.invoke_focus_canvas();
                }
            }
            Event::Directory { path, entries } => {
                state.active.files.directory = path.clone();
                state.active.files.entries = entries;
                app.set_files_path(path.into());
                rows(app, &state.active.files);
                app.set_files_directory_ready(true);
                app.set_files_error("".into());
                app.set_files_busy(false);
                app.set_files_status(
                    state
                        .active
                        .files
                        .last_outcome
                        .take()
                        .unwrap_or_else(|| {
                            "Encrypted SFTP. No files are automatically opened or replaced.".into()
                        })
                        .into(),
                );
            }
            Event::Completed { description } => {
                state.active.files.last_outcome = Some(description.clone());
                app.set_files_busy(false);
                app.set_files_status(description.into());
                state
                    .active
                    .files
                    .command(app, Command::List(state.active.files.directory.clone()))?;
            }
            Event::Clipboard(text) => {
                app.set_files_busy(false);
                if !std::mem::take(&mut state.active.files.apply_clipboard_read) {
                    app.set_files_status("Clipboard copy cancelled after switching Macs.".into());
                    continue;
                }
                let result = state.clipboard.receive(window_handle(app)?,&text).and_then(|_| {
                    scope(&state.active.files)?.critical("clipboard.applied",Level::Info,json!({"clipboard.direction":"mac_to_windows","clipboard.text_bytes":text.len(),"network.protocol.name":"ssh"}))
                });
                match result {
                    Ok(()) => app
                        .set_files_status("Mac plain text is now on the Windows clipboard.".into()),
                    Err(error) => {
                        scope(&state.active.files)?.event(
                            "clipboard.apply_failed",
                            Level::Warn,
                            json!({"error.type":maccanvas::audit::error_type(&error)}),
                        )?;
                        fail(app, error);
                    }
                }
            }
            Event::ClipboardSent(sequence) => {
                state.clipboard.sent(sequence);
                app.set_files_busy(false);
                app.set_files_status(
                    "Windows plain text is now on the Mac clipboard. Paste it in your Mac app."
                        .into(),
                );
            }
            Event::Failed(error) => {
                app.set_files_busy(false);
                state.active.files.apply_clipboard_read = false;
                fail(app, error);
            }
            Event::DirectoryFailed { path, error } => {
                app.set_files_busy(false);
                state.active.files.last_outcome = None;
                let ready = !state.active.files.directory.is_empty();
                app.set_files_directory_ready(ready);
                app.set_files_path(
                    if ready {
                        state.active.files.directory.clone()
                    } else {
                        path.clone()
                    }
                    .into(),
                );
                app.set_files_status(
                    if ready {
                        format!("SSH connected. Showing {}.", state.active.files.directory)
                    } else {
                        "SSH connected. Enter a folder this account can access.".into()
                    }
                    .into(),
                );
                fail(
                    app,
                    format!("Cannot open the Mac folder \"{path}\". {error}"),
                );
            }
            Event::Closed => {
                state.active.files.stop(app);
            }
        }
    }
    if app.get_files_connected()
        && !app.get_files_busy()
        && let Some(action) = state.active.files.pending_clipboard.take()
        && let Err(error) = clipboard_command(app, state, action)
    {
        fail(app, error);
    }
    if let Some(client) = &state.active.files.client {
        if client.is_finished() {
            state.active.files.stop(app);
            return Ok(());
        }
        if app.get_files_busy() {
            let progress = client.progress()?;
            app.set_files_progress(if progress.total == 0 {
                0.0
            } else {
                (progress.bytes as f64 / progress.total as f64).min(1.0) as f32
            });
            app.set_files_progress_label(if progress.phase.is_empty() {
                "Waiting for the Mac...".into()
            } else {
                format!(
                    "{}: {} / {}  -  {}",
                    progress.phase,
                    size(progress.bytes),
                    size(progress.total),
                    telemetry::rate(progress.bits_per_second)
                )
                .into()
            });
        }
    }
    Ok(())
}

fn safe_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|character| {
            if character.is_control() || "<>:\"/\\|?*".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect();
    let name = name.trim_end_matches(['.', ' ']);
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if name.is_empty() {
        "download".into()
    } else if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
    {
        format!("_{name}")
    } else {
        name.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_file_names_cannot_prefill_windows_device_names_or_streams() {
        assert_eq!(safe_name("CON.txt"), "_CON.txt");
        assert_eq!(safe_name("hello:secret.txt"), "hello_secret.txt");
        assert_eq!(safe_name("LPT1"), "_LPT1");
        assert_eq!(safe_name(".."), "download");
        assert_eq!(safe_name("hello\u{1f34e}.txt"), "hello\u{1f34e}.txt");
    }
}
