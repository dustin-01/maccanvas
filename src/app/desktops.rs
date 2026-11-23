use super::*;
use maccanvas::apple_hps::cursor;

pub(super) struct Desktop {
    pub session: Option<Session>,
    pub connection_profile: Option<Profile>,
    pub connection_credentials: Option<CredentialSource>,
    pub reconnect: Option<Reconnect>,
    pub reconnect_attempt: u32,
    pub connected: bool,
    pub generation: u64,
    pub streaming: StreamSettings,
    pub files: files_ui::Files,
    panel: files_ui::Panel,
    pub audio_restart: bool,
    pub audio_status: String,
    pub audio_missing: bool,
    pub status: String,
    pub error: String,
    pub cursor: Option<(cursor::Update, f32)>,
    pub frames: Frames,
    pub frame: Option<Arc<Framebuffer>>,
    pub gpu_frame: Option<Arc<maccanvas::render::gpu::nvdec::Frame>>,
    pub source_size: Option<Extent>,
    pub viewport: Option<Viewport>,
    pub thumbnail: slint::Image,
    last_preview: Option<Instant>,
    pub monitor: Monitor,
    pub network_logged: Instant,
    pub frame_number: u64,
    pub last_displayed_frame: u64,
}

impl Desktop {
    pub fn keep_files_from(&mut self, previous: &mut Desktop) {
        self.files = std::mem::take(&mut previous.files);
        self.files.reset_network();
        self.panel = std::mem::take(&mut previous.panel);
    }

    pub fn new(streaming: StreamSettings) -> Self {
        let now = Instant::now();
        Self {
            session: None,
            connection_profile: None,
            connection_credentials: None,
            reconnect: None,
            reconnect_attempt: 0,
            connected: false,
            generation: 0,
            streaming,
            files: Default::default(),
            panel: Default::default(),
            audio_restart: false,
            audio_status: "Audio is available with HEVC.".into(),
            audio_missing: false,
            status: "Disconnected".into(),
            error: String::new(),
            cursor: None,
            frames: Default::default(),
            frame: None,
            gpu_frame: None,
            source_size: None,
            viewport: None,
            thumbnail: Default::default(),
            last_preview: None,
            monitor: Monitor::new(now),
            network_logged: now,
            frame_number: 0,
            last_displayed_frame: 0,
        }
    }

    pub fn id(&self) -> Option<Uuid> {
        self.connection_profile.as_ref().map(|profile| profile.id)
    }

    /// Drain only the latest frame from each single-slot mailbox. The foreground
    /// does this on presentation, while previews sample at a much lower rate.
    pub fn receive_frame(&mut self, software: bool) -> Result<bool> {
        let gpu = self
            .session
            .as_ref()
            .map(Session::take_gpu_frame)
            .transpose()?
            .flatten();
        let cpu = self
            .frames
            .lock()
            .map_err(|_| Error::Display("Frame queue is unavailable".into()))?
            .take();
        if let Some(cpu) = cpu {
            self.source_size = Some(cpu.size);
            self.frame = Some(cpu);
            self.gpu_frame = None;
        } else if let Some(gpu) = gpu {
            let size = gpu.size;
            if software {
                self.frame = Some(Arc::new(Framebuffer {
                    size,
                    rgba: gpu.read_rgba()?,
                }));
                self.gpu_frame = None;
            } else {
                self.frame = None;
                self.gpu_frame = Some(gpu);
            }
            self.source_size = Some(size);
        } else {
            return Ok(false);
        }
        self.frame_number += 1;
        Ok(true)
    }

    fn make_thumbnail(&mut self) -> Result<()> {
        let Some(size) = self.source_size else {
            return Ok(());
        };
        let readback;
        let rgba = if let Some(frame) = &self.frame {
            &frame.rgba[..]
        } else if let Some(gpu) = &self.gpu_frame {
            readback = gpu.read_rgba()?;
            &readback[..]
        } else {
            return Ok(());
        };
        let scale = (224.0 / f64::from(size.width))
            .min(144.0 / f64::from(size.height))
            .min(1.0);
        let width = (f64::from(size.width) * scale).round().max(1.0) as u32;
        let height = (f64::from(size.height) * scale).round().max(1.0) as u32;
        let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
        let out = pixels.make_mut_bytes();
        for y in 0..height {
            let row = (y * size.height / height) * size.width;
            for x in 0..width {
                let src = ((row + x * size.width / width) * 4) as usize;
                let dst = ((y * width + x) * 4) as usize;
                out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
            }
        }
        self.thumbnail = slint::Image::from_rgba8(pixels);
        Ok(())
    }

    fn update_preview(&mut self, now: Instant, software: bool) -> bool {
        self.last_preview = Some(now);
        let result = self.receive_frame(software).and_then(|updated| {
            if updated {
                self.make_thumbnail()?;
            }
            Ok(updated)
        });
        match result {
            Ok(updated) => updated,
            Err(error) => {
                self.error = error.to_string();
                false
            }
        }
    }
}

impl State {
    pub fn desktop(&self, id: Uuid) -> Option<&Desktop> {
        std::iter::once(&self.active)
            .chain(&self.background)
            .find(|desk| desk.id() == Some(id))
    }

    pub fn desktop_mut(&mut self, generation: u64) -> Option<&mut Desktop> {
        std::iter::once(&mut self.active)
            .chain(&mut self.background)
            .find(|desk| desk.session.is_some() && desk.generation == generation)
    }

    pub fn connected_profiles(&self) -> impl Iterator<Item = &Profile> {
        self.settings
            .profiles
            .iter()
            .filter(|profile| self.desktop(profile.id).is_some_and(|desk| desk.connected))
    }

    pub fn editing_id(&self) -> Uuid {
        self.selected
            .and_then(|i| self.settings.profiles.get(i))
            .map_or(self.draft_id, |profile| profile.id)
    }
}

pub fn show_cursor(app: &App, desktop: &Desktop) {
    app.set_remote_cursor_ready(false);
    app.set_remote_cursor_hidden(false);
    if let Some((update, point_scale)) = &desktop.cursor {
        app.set_remote_cursor_scale(*point_scale);
        match update {
            cursor::Update::Image(cursor) => {
                let mut pixels =
                    SharedPixelBuffer::<Rgba8Pixel>::new(cursor.width.into(), cursor.height.into());
                pixels.make_mut_bytes().copy_from_slice(&cursor.rgba);
                app.set_remote_cursor_image(slint::Image::from_rgba8(pixels));
                app.set_remote_cursor_width(cursor.width.into());
                app.set_remote_cursor_height(cursor.height.into());
                app.set_remote_cursor_hotspot_x(cursor.hotspot_x.into());
                app.set_remote_cursor_hotspot_y(cursor.hotspot_y.into());
                app.set_remote_cursor_ready(true);
            }
            cursor::Update::Hidden => app.set_remote_cursor_hidden(true),
        }
    }
}

pub fn show(app: &App, state: &State) {
    let desktop = &state.active;
    let hevc = desktop.session.as_ref().is_some_and(Session::uses_hevc);
    app.set_connected(desktop.connected);
    app.set_connecting(desktop.id().is_some() && !desktop.connected);
    app.set_hevc_active(hevc);
    app.set_clipboard_shared(!hevc && state.settings.clipboard_enabled);
    app.set_audio_status(desktop.audio_status.as_str().into());
    app.set_audio_codec_missing(desktop.audio_missing);
    app.set_status(desktop.status.as_str().into());
    app.set_error_text(desktop.error.as_str().into());
    show_cursor(app, desktop);
}

pub fn activate(app: &App, state: &Rc<RefCell<State>>, id: Uuid) -> Result<()> {
    let (outgoing, animate);
    {
        let mut state = state.borrow_mut();
        state.wanted = None;
        if state.active.id() == Some(id) {
            state.select_profile(id);
            set_profile_form(app, state.active.connection_profile.as_ref());
            model(app, &state);
            app.set_editor_open(false);
            app.invoke_focus_canvas();
            return Ok(());
        }
        let Some(index) = state.background.iter().position(|d| d.id() == Some(id)) else {
            return Ok(());
        };
        let software = !state.gpu_renderer;
        // Prepare the destination before changing which session receives input.
        state.background[index].receive_frame(software)?;
        state.release()?;
        if let Some(session) = &state.active.session {
            if !session.uses_hevc() && !session.is_closed() {
                session.share_clipboard(false)?;
            }
            session.set_foreground(false);
        }
        state.pending_clipboard = None;
        state.clipboard = Default::default();
        state.active.files.clear_pending_clipboard();
        state.active.panel = files_ui::Panel::capture(app);
        if let Err(error) = state.active.make_thumbnail() {
            state.active.error = error.to_string();
        }
        // Direct GPU textures are owned by the outgoing rendering context.
        outgoing = if state.active.gpu_frame.is_some()
            && maccanvas::render::gpu::direct_sharing_enabled()
        {
            state.active.thumbnail.clone()
        } else {
            app.get_frame_image()
        };
        animate =
            state.active.source_size.is_some() && state.background[index].source_size.is_some();
        app.set_previous_screen_width(app.get_draw_width());
        app.set_previous_screen_height(app.get_draw_height());
        let next = state.background.remove(index);
        let old = std::mem::replace(&mut state.active, next);
        if old.id().is_some() {
            state.background.push(old);
        }
        state.active.viewport = None;
        if let Some(session) = &state.active.session {
            session.set_foreground(true);
            if !session.is_closed() {
                let result = if !session.uses_hevc() {
                    session.share_clipboard(state.settings.clipboard_enabled)
                } else {
                    session.send(Command::Audio(state.settings.audio))
                };
                if let Err(error) = result {
                    state.active.error = error.to_string();
                }
            }
        }
        state.select_profile(id);
        set_profile_form(app, state.active.connection_profile.as_ref());
        state.active.panel.show(app, &state.active.files);
        app.set_files_open(false);
        app.set_clipboard_open(false);
        app.set_audio_open(false);
        app.set_editor_open(false);
        app.set_username("".into());
        app.set_password("".into());
        app.set_frame_image(state.active.thumbnail.clone());
        app.set_remote_cursor_frame_width(
            state
                .active
                .source_size
                .map_or(1., |size| size.width as f32),
        );
        app.set_desktop_description("".into());
        app.set_active_encoding("Waiting for pixels".into());
        app.set_fps_text("0 FPS".into());
        app.set_download_rate("0.0 kbps".into());
        app.set_upload_rate("0.0 kbps".into());
        app.set_traffic(ModelRc::new(VecModel::from(vec![
            TrafficBar::default();
            telemetry::HISTORY
        ])));
        stream_form(app, state.active.streaming);
        stream_summary(app, state.active.streaming);
        guide(app, state.active.source_size);
        show(app, &state);
        if state.active.connected && state.settings.record_mac_use(id) {
            state.persist()?;
        }
        model(app, &state);
    }
    refresh_layout(app, state)?;
    app.set_previous_screen(if animate && app.get_motion_enabled() {
        outgoing
    } else {
        slint::Image::default()
    });
    app.invoke_animate_mac_switch();
    app.window().request_redraw();
    app.invoke_focus_canvas();
    Ok(())
}

pub fn restore_survivor(app: &App, state: &Rc<RefCell<State>>) -> Result<()> {
    let next = {
        let state = state.borrow();
        state
            .background
            .iter()
            .rev()
            .find(|desk| desk.connected)
            .or_else(|| state.background.last())
            .and_then(Desktop::id)
    };
    if let Some(id) = next {
        activate(app, state, id)?;
    } else {
        model(app, &state.borrow());
    }
    Ok(())
}

pub fn disconnect_machine(app: &App, shared: &Rc<RefCell<State>>, id: Uuid) -> Result<()> {
    let mut state = shared.borrow_mut();
    if state.active.id() == Some(id) {
        drop(state);
        return disconnect(app, shared);
    }
    if let Some(i) = state
        .background
        .iter()
        .position(|d| d.id() == Some(id))
    {
        let mut desktop = state.background.remove(i);
        let closed = if let Some(session) = desktop.session.take() {
            let record =
                session
                    .scope()
                    .critical("session.disconnect.requested", Level::Info, json!({}));
            let close = session.close();
            state.retired.push(session);
            record.and(close)
        } else {
            Ok(())
        };
        desktop.files.retire();
        if desktop.files.active() {
            state.retired_files.push(desktop.files);
        }
        if state.wanted == Some(id) {
            state.wanted = None;
        }
        model(app, &state);
        closed?;
    }
    Ok(())
}

pub fn previews(app: &App, shared: &Rc<RefCell<State>>) -> Result<()> {
    let mut state = shared.borrow_mut();
    if state.background.is_empty() {
        return Ok(());
    }
    let now = Instant::now();
    let software = !state.gpu_renderer;

    // The first picture of a newly requested connection should switch the
    // canvas promptly, even if the sidebar is hidden or its preview isn't due.
    if let Some(wanted) = state.wanted
        && let Some(desktop) = state
            .background
            .iter_mut()
            .find(|desktop| desktop.connected && desktop.id() == Some(wanted))
    {
        desktop.update_preview(now, software);
        if desktop.source_size.is_some() {
            drop(state);
            return activate(app, shared, wanted);
        }
    }

    if !app.get_sidebar_open()
        || app.get_immersive()
        || app.get_editor_open()
        || now < state.preview_at
    {
        return Ok(());
    }
    // A GPU preview reads the full surface. Sample at most four cards per
    // second across all desktops and at most twice a second for any one card.
    state.preview_at = now + Duration::from_millis(250);
    let visible = filtered_profiles(app, &state);
    let next = state
        .background
        .iter()
        .enumerate()
        .filter(|(_, desktop)| {
            desktop.connected
                && desktop
                    .last_preview
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(500))
        })
        .filter_map(|(index, desktop)| {
            let id = desktop.id()?;
            let row = visible.iter().position(|profile| profile.id == id)?;
            Some((desktop.last_preview, index, row))
        })
        .min_by_key(|(last, _, _)| *last);
    if let Some((_, index, row)) = next {
        let desktop = &mut state.background[index];
        if desktop.update_preview(now, software) {
            let rows = app.get_machines();
            if let Some(mut machine) = rows.row_data(row) {
                machine.thumbnail = desktop.thumbnail.clone();
                rows.set_row_data(row, machine);
            }
        }
    }
    Ok(())
}
