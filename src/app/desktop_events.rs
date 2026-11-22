use super::*;

pub(super) fn route(
    app: &App,
    shared: &Rc<RefCell<State>>,
    generation: u64,
    event: Event,
) -> Result<()> {
    let mut state = shared.borrow_mut();
    let foreground = state.active.generation == generation;
    let Some(desktop) = state.desktop_mut(generation) else {
        return Ok(()); // A closed or replaced session may still have queued events.
    };
    match event {
        Event::Connected {
            profile,
            server_name,
            credentials_saved,
        } => {
            let streaming = desktop.streaming;
            let restart_audio =
                std::mem::take(&mut desktop.audio_restart) && streaming.encoding.is_hevc();
            let initial_connection = desktop.reconnect_attempt == 0;
            desktop.connected = true;
            desktop.reconnect_attempt = 0;
            desktop.status = format!("Connected to {} ({server_name})", profile.name);
            desktop.connection_profile = Some(profile.clone());
            if credentials_saved {
                desktop.connection_credentials = Some(CredentialSource::Saved);
            }
            let editing = state.editing_id() == profile.id;
            let index = if let Some(index) = state
                .settings
                .profiles
                .iter()
                .position(|p| p.id == profile.id)
            {
                state.settings.profiles[index] = profile.clone();
                index
            } else {
                state.settings.profiles.push(profile.clone());
                state.settings.profiles.len() - 1
            };
            let close_editor =
                editing && initial_connection && (foreground || state.wanted == Some(profile.id));
            if editing {
                state.selected = Some(index);
                app.set_has_saved(profile.has_saved_credentials);
                app.set_use_saved(profile.has_saved_credentials);
            }
            if foreground {
                desktops::show(app, &state);
            }
            model(app, &state);
            state.persist()?;
            drop(state);
            if close_editor {
                app.set_editor_open(false);
                if foreground && !app.get_modal_open() {
                    app.invoke_focus_canvas();
                }
            }
            if restart_audio {
                if foreground {
                    apply_streaming(app, shared, streaming, true)?;
                } else {
                    restart_background_audio(app, shared, profile.id)?;
                }
            }
        }
        Event::Warning(message) | Event::Error(message) => {
            desktop.error = message;
            if foreground {
                app.set_error_text(desktop.error.as_str().into());
            }
        }
        Event::Cursor {
            update,
            point_scale,
        } => {
            desktop.cursor = Some((update, point_scale));
            if foreground {
                desktops::show_cursor(app, desktop);
            }
        }
        Event::Audio(status) => {
            use maccanvas::audio::Status;
            desktop.audio_missing = matches!(status, Status::CodecMissing(_));
            desktop.audio_status = match status {
                Status::Waiting => "Waiting for Mac audio...".into(),
                Status::Playing => "Mac system audio - 48 kHz stereo".into(),
                Status::CodecMissing(message) => message,
                Status::Unavailable(message) => {
                    desktop.error = message.clone();
                    if foreground {
                        notify(app, &message);
                    }
                    message
                }
            };
            if foreground {
                app.set_audio_codec_missing(desktop.audio_missing);
                app.set_audio_status(desktop.audio_status.as_str().into());
            }
        }
        Event::Disconnected { reconnect } => {
            drop(state);
            connection_ended(app, shared, generation, reconnect)?;
        }
    }
    Ok(())
}

fn reconnect_request(desktop: &Desktop, delay: Duration) -> Result<Reconnect> {
    Ok(Reconnect {
        profile: desktop.connection_profile.clone().ok_or_else(|| {
            Error::Protocol("The interrupted connection profile is unavailable".into())
        })?,
        credentials: desktop.connection_credentials.clone().ok_or_else(|| {
            Error::Credential("The interrupted connection cannot be restarted".into())
        })?,
        retry_at: Instant::now() + delay,
        previous: desktop
            .session
            .as_ref()
            .expect("live session event")
            .counters
            .clone(),
    })
}

fn retire_session(state: &mut State, desktop: &mut Desktop) -> Result<()> {
    if let Some(session) = desktop.session.take() {
        let closed = session.close();
        state.retired.push(session);
        closed?;
    }
    Ok(())
}

// Move the file workers off the UI thread only when their Mac is actually gone.
fn retire_files(state: &mut State, desktop: &mut Desktop) {
    desktop.files.retire();
    if desktop.files.active() {
        state.retired_files.push(std::mem::take(&mut desktop.files));
    }
}

pub(super) fn restart_background_audio(
    app: &App,
    shared: &Rc<RefCell<State>>,
    id: Uuid,
) -> Result<()> {
    let mut state = shared.borrow_mut();
    let Some(index) = state.background.iter().position(|d| d.id() == Some(id)) else {
        return Ok(());
    };
    let pending = reconnect_request(&state.background[index], Duration::ZERO)?;
    state.audit.critical(
        "session.reconnect.requested",
        Level::Info,
        json!({
            "profile.id":id, "rfb.streaming":state.background[index].streaming,
            "session.reconnect.reason":"audio",
        }),
    )?;
    let mut desktop = state.background.remove(index);
    let closed = retire_session(&mut state, &mut desktop);
    desktop.reconnect = Some(pending);
    desktop.connected = false;
    desktop.reconnect_attempt = 1;
    desktop.status = "Restarting Mac audio...".into();
    state.background.insert(index, desktop);
    model(app, &state);
    closed
}

fn connection_ended(
    app: &App,
    shared: &Rc<RefCell<State>>,
    generation: u64,
    retryable: bool,
) -> Result<()> {
    let mut state = shared.borrow_mut();
    let foreground = state.active.generation == generation;
    let audit = state.audit.clone();
    let Some(desktop) = state.desktop_mut(generation) else {
        return Ok(());
    };
    let profile = desktop
        .connection_profile
        .clone()
        .expect("live connection profile");
    let message = desktop.error.clone();
    let streaming = desktop.streaming;
    let next = if retryable
        && (desktop.connected || desktop.reconnect_attempt != 0)
        && desktop.streaming.encoding.is_hevc()
        && audit.audit().failure().is_none()
    {
        let attempt = desktop.reconnect_attempt.saturating_add(1);
        let delay = reconnect_delay(attempt);
        let request = reconnect_request(desktop, delay)?;
        audit.critical("session.reconnect.scheduled", Level::Warn, json!({
            "profile.id":profile.id, "session.reconnect.reason":"connection_interrupted",
            "session.reconnect.attempt":attempt, "session.reconnect.delay_ms":delay.as_millis() as u64,
        }))?;
        Some((
            request,
            attempt,
            format!(
                "Connection interrupted. Reconnecting in {}s (attempt {attempt}). Cancel to stop.",
                delay.as_secs()
            ),
        ))
    } else {
        None
    };

    if foreground {
        drop(state);
        if let Some((request, attempt, status)) = next {
            let closed = close_stream(app, shared);
            let mut state = shared.borrow_mut();
            let desktop = &mut state.active;
            desktop.files.stop(app);
            desktop.connected = false;
            desktop.streaming = streaming;
            desktop.connection_profile = Some(profile);
            desktop.reconnect = Some(request);
            desktop.reconnect_attempt = attempt;
            desktop.status = status;
            desktop.error = message;
            desktops::show(app, &state);
            model(app, &state);
            closed?;
        } else {
            disconnect(app, shared)?;
            if !message.is_empty() {
                let mut state = shared.borrow_mut();
                if state.active.id().is_none() {
                    state.active.error.clone_from(&message);
                    notify(app, message);
                } else {
                    notify(app, format!("{}: {message}", profile.name));
                }
            }
        }
    } else {
        let index = state
            .background
            .iter()
            .position(|d| d.generation == generation)
            .expect("generation belongs to a background session");
        let mut desktop = state.background.remove(index);
        let closed = retire_session(&mut state, &mut desktop);
        retire_files(&mut state, &mut desktop);
        if let Some((request, attempt, status)) = next {
            let mut waiting = Desktop::new(streaming);
            waiting.generation = generation;
            waiting.connection_profile = Some(profile);
            waiting.reconnect = Some(request);
            waiting.reconnect_attempt = attempt;
            waiting.status = status;
            waiting.error = message;
            state.background.insert(index, waiting);
        } else {
            if state.wanted == Some(profile.id) {
                state.wanted = None;
            }
            if !message.is_empty() {
                notify(app, format!("{}: {message}", profile.name));
            }
        }
        model(app, &state);
        closed?;
    }
    Ok(())
}

pub(super) fn tick_reconnect(
    app: &App,
    shared: &Rc<RefCell<State>>,
    events: &mpsc::Sender<(u64, Event)>,
) -> Result<()> {
    let mut state = shared.borrow_mut();
    let now = Instant::now();
    let Some(id) = std::iter::once(&state.active)
        .chain(&state.background)
        .filter_map(|desktop| desktop.reconnect.as_ref())
        .find(|request| {
            now >= request.retry_at
                && !state
                    .retired
                    .iter()
                    .any(|s| Arc::ptr_eq(&s.counters, &request.previous))
        })
        .map(|request| request.profile.id)
    else {
        return Ok(());
    };

    let index = state.background.iter().position(|d| d.id() == Some(id));
    let foreground = index.is_none();
    let desktop = match index {
        Some(index) => &mut state.background[index],
        None => &mut state.active,
    };
    let pending = desktop.reconnect.take().expect("due reconnect");
    let attempt = desktop.reconnect_attempt;
    let streaming = desktop.streaming;
    let name = pending.profile.name.clone();
    match open_desktop(
        app,
        &mut state,
        pending.profile,
        pending.credentials,
        false,
        streaming,
        foreground,
        events,
    ) {
        Ok(mut opened) => {
            opened.reconnect_attempt = attempt;
            let old = match index {
                Some(index) => &mut state.background[index],
                None => &mut state.active,
            };
            opened.keep_files_from(old);
            *old = opened;
            if foreground {
                desktops::show(app, &state);
            }
            model(app, &state);
        }
        Err(error) => {
            if let Some(index) = index {
                let mut old = state.background.remove(index);
                retire_files(&mut state, &mut old);
                if state.wanted == Some(id) {
                    state.wanted = None;
                }
                model(app, &state);
            } else {
                drop(state);
                disconnect(app, shared)?;
            }
            notify(app, format!("{name}: {error}"));
        }
    }
    Ok(())
}
