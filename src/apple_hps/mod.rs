//! Native Apple High Performance transport, ported from iShareScreen (AGPL-3.0-or-later).
mod audio;
mod bplist;
mod control;
mod crypto;
pub mod cursor;
mod offers;
mod rtp;
#[cfg(test)]
mod tests;
mod udp;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::json;
use zeroize::Zeroizing;

use crate::{
    audio::{Settings as AudioSettings, Status as AudioStatus},
    audit::{Level, Scope},
    auth::Credentials,
    error::{Error, Result},
    framebuffer::Framebuffer,
    geometry::Extent,
    input::Command,
    protocol::ServerInfo,
    streaming::{Encoding, StreamSettings},
    telemetry::{Counters, Metered},
};

use control::{Channel, Notice};
use crypto::{Packet, Srtcp, Srtp, SrtpSender};
use offers::Offers;
use rtp::Assembler;
use udp::PeerSocket;

pub const COUNTER_ENCODING: i32 = -26_500;

pub enum Frame {
    Software(Framebuffer),
    #[cfg(windows)]
    Gpu(Arc<crate::render::gpu::nvdec::Frame>),
}

impl Frame {
    pub fn size(&self) -> Extent {
        match self {
            Self::Software(frame) => frame.size,
            #[cfg(windows)]
            Self::Gpu(frame) => frame.size,
        }
    }
}

fn failed(message: impl std::fmt::Display) -> Error {
    Error::Protocol(format!("HEVC screen sharing: {message}"))
}

fn send_udp(socket: &PeerSocket, packet: &[u8], counters: &Counters) -> Result<()> {
    let count = socket.send(packet)?;
    if count != packet.len() {
        return Err(failed("UDP feedback was only partially sent"));
    }
    counters.sent.fetch_add(count as u64, Ordering::Relaxed);
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum VideoAction {
    Wait,
    Recover,
    Reconnect,
}

#[derive(Default)]
struct VideoWatchdog {
    waiting_since: Option<Instant>,
    last_packet: Option<Instant>,
}

impl VideoWatchdog {
    fn packet(&mut self, now: Instant) {
        if self
            .last_packet
            .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(1))
            && self.waiting_since.is_some()
        {
            // A quiet desktop must get a fresh recovery window when video resumes.
            self.waiting_since = Some(now);
        }
        self.last_packet = Some(now);
    }

    fn expect_picture(&mut self, now: Instant) {
        self.waiting_since.get_or_insert(now);
    }

    fn decoded(&mut self) {
        self.waiting_since = None;
    }

    fn action(&self, now: Instant) -> VideoAction {
        if self
            .last_packet
            .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(1))
        {
            return VideoAction::Wait;
        }
        match self
            .waiting_since
            .map(|since| now.saturating_duration_since(since))
        {
            Some(wait) if wait > Duration::from_secs(30) => VideoAction::Reconnect,
            Some(wait) if wait > Duration::from_secs(10) => VideoAction::Recover,
            _ => VideoAction::Wait,
        }
    }
}

pub struct Client {
    pub info: ServerInfo,
    audio: Option<audio::Service>,
    audio_pending: VecDeque<AudioStatus>,
    audio_settings: AudioSettings,
    foreground: Arc<AtomicBool>,
    converter: Option<crate::audio::Codec>,
    channel: Channel,
    control_socket: Arc<PeerSocket>,
    video_socket: Arc<PeerSocket>,
    offers: Offers,
    configuration: Zeroizing<Vec<u8>>,
    receiver: Srtp,
    feedback: Srtcp,
    heartbeat: SrtpSender,
    decoder: native_hevc::Decoder,
    #[cfg(windows)]
    hardware: Option<crate::render::gpu::Decoder>,
    assembler: Assembler,
    counters: Arc<Counters>,
    scope: Scope,
    scaled: Extent,
    backing: Extent,
    encoded: Extent,
    previous_canvas: Option<(Extent, Instant)>,
    early: VecDeque<Vec<u8>>,
    candidates: Vec<Packet>,
    candidate_bytes: usize,
    counts: HashMap<u32, usize>,
    selection_at: Instant,
    selected: Option<u32>,
    highest: u64,
    need_keyframe: bool,
    last_feedback: Instant,
    last_fir: Instant,
    last_nack: Instant,
    last_frame: Instant,
    pacer: crate::render::Pacer<Frame>,
    watchdog: VideoWatchdog,
    started: Instant,
    fps_ceiling: u16,
    hevc_bitrate_bps: u32,
    virtual_display: Option<[u16; 2]>,
    last_recovery: Instant,
    decoded: u64,
    rejected: u64,
    warned: HashSet<i32>,
    alternate_bank: bool,
    video_started: Instant,
    last_stats: Instant,
    authenticated: u64,
    nal_types: HashMap<u8, u64>,
}

impl Client {
    pub fn open(
        stream: Metered<TcpStream>,
        credentials: &Credentials,
        settings: StreamSettings,
        scope: Scope,
        audio_settings: AudioSettings,
        prefer_hardware: bool,
    ) -> Result<Self> {
        Self::open_with_foreground(
            stream,
            credentials,
            settings,
            scope,
            audio_settings,
            prefer_hardware,
            Arc::new(AtomicBool::new(true)),
        )
    }

    pub(crate) fn open_with_foreground(
        mut stream: Metered<TcpStream>,
        credentials: &Credentials,
        settings: StreamSettings,
        scope: Scope,
        audio_settings: AudioSettings,
        prefer_hardware: bool,
        foreground: Arc<AtomicBool>,
    ) -> Result<Self> {
        #[cfg(target_os = "macos")]
        let _ = prefer_hardware;
        settings.validate()?;
        if !settings.encoding.is_hevc() {
            return Err(failed("choose an HEVC streaming preset"));
        }
        let sixty_fps = settings.encoding == Encoding::Hevc60;
        let peer = stream.inner.peer_addr()?;
        let local = stream.inner.local_addr()?;
        let video_port = peer
            .port()
            .checked_add(1)
            .ok_or_else(|| failed("HEVC requires a control port below 65535"))?;
        let mut video_peer = peer;
        video_peer.set_port(video_port);
        let control_socket = PeerSocket::bind(local, peer)?;
        let video_socket = PeerSocket::bind(local, video_peer)?;
        let counters = stream.counters.clone();
        let key = control::authenticate(&mut stream, credentials)?;
        scope.critical("security.authentication.completed",Level::Info,json!({
            "event.outcome":"success","rfb.security.type":33,"rfb.authentication.variant":"rsa1",
            "rfb.protocol.version":"003.889",
        }))?;
        let (mut channel, info) = Channel::bootstrap(stream, &key, settings.virtual_display)?;
        drop(key);
        scope.critical(
            "media.hevc.control.ready",
            Level::Info,
            json!({"network.encrypted":true,"media.physical.width":info.size.width,"media.physical.height":info.size.height}),
        )?;
        audio_settings.validate()?;
        let mut audio_pending = VecDeque::new();
        let converter = match crate::audio::codec_available() {
            Ok(path) => {
                if path.is_none() {
                    audio_pending.push_back(AudioStatus::CodecMissing(
                        "Enable audio to install the optional codec component.".into(),
                    ));
                }
                path
            }
            Err(error) => {
                scope.critical(
                    "media.audio.component.rejected",
                    Level::Error,
                    crate::audit::error_attributes(&error),
                )?;
                audio_pending.push_back(AudioStatus::CodecMissing(error.to_string()));
                None
            }
        };
        let offers = Offers::new(converter.is_some(), settings.hevc_bitrate_bps)?;
        let configuration = offers.configuration(sixty_fps)?;
        scope.critical("media.hevc.offer.requested",Level::Info,json!({"media.codec.name":"hevc","media.tiles_per_frame":1,"media.requested_fps":if sixty_fps{60}else{30},"media.bitrate.requested_bps":settings.hevc_bitrate_bps}))?;
        let mut early = VecDeque::new();
        let mut early_bytes = 0;
        let mut buffer = [0; 65_536];
        let mut last_punch = Instant::now() - Duration::from_secs(1);
        let (canvas, layout) = control::wait_canvas(&mut channel, &configuration, || {
            if last_punch.elapsed() >= Duration::from_millis(100) {
                send_udp(&control_socket, &[0], &counters)?;
                send_udp(&video_socket, &[0], &counters)?;
                last_punch = Instant::now();
            }
            for _ in 0..256 {
                match video_socket.recv(&mut buffer) {
                    Ok(length) => {
                        counters
                            .received
                            .fetch_add(length as u64, Ordering::Relaxed);
                        if early_bytes + length > 4 * 1024 * 1024 {
                            return Err(failed("startup media burst exceeded its buffer limit"));
                        }
                        early.push_back(buffer[..length].to_vec());
                        early_bytes += length;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        })?;
        if canvas.width * canvas.height > 16_777_216 {
            return Err(failed("negotiated HEVC canvas exceeds the pixel limit"));
        }
        // ServerInit describes the physical desktop before a virtual display exists.
        let default_scaled = if settings.virtual_display.is_some() {
            canvas
        } else {
            info.size
        };
        let (scaled, backing) = layout.unwrap_or((default_scaled, canvas));
        channel.send(&control::refresh(backing))?;
        scope.critical(
            "media.hevc.transport.ready",
            Level::Info,
            json!({
                "media.codec.requested":"hevc","media.transport":"srtp","network.encrypted":true,
                "media.security.profile":"AES_CM_256_HMAC_SHA1_80","media.tiles.requested":1,
                "media.width":canvas.width,"media.height":canvas.height,
                "media.requested_fps":if sixty_fps {60} else {30},
                "server.video.port":video_port,"server.control.port":peer.port(),
                "media.decoder":"libde265",
                "media.virtual_display.requested":settings.virtual_display,
            }),
        )?;
        let receiver = Srtp::new(&offers.video_receive);
        let feedback = Srtcp::new(&offers.video_send);
        let heartbeat = SrtpSender::new(&offers.audio_send, offers.audio_ssrc);
        let decoder = native_hevc::Decoder::new().map_err(failed)?;
        #[cfg(windows)]
        let mut hardware = None;
        #[cfg(windows)]
        if prefer_hardware && crate::render::gpu::hardware_enabled()? {
            match crate::render::gpu::candidates() {
                Ok(candidates) => {
                    for candidate in candidates {
                        match crate::render::gpu::Decoder::new(&candidate, canvas) {
                            Ok(selected) => {
                                scope.event("media.hevc.hardware.selected", Level::Info,
                                    json!({"media.decoder":candidate.backend.name(),"gpu.adapter.name":candidate.name}))?;
                                hardware = Some(selected);
                                break;
                            }
                            Err(error) => scope.event("media.hevc.hardware.unavailable", Level::Info,
                                json!({"media.decoder":candidate.backend.name(),"gpu.adapter.name":candidate.name,"error.message":error.to_string()}))?,
                        }
                    }
                }
                Err(error) => scope.event(
                    "media.hevc.hardware.unavailable",
                    Level::Info,
                    json!({"error.message":error.to_string()}),
                )?,
            }
        }
        let now = Instant::now();
        let mut client = Self {
            info,
            audio: None,
            audio_pending,
            audio_settings,
            foreground,
            converter,
            channel,
            control_socket,
            video_socket,
            offers,
            configuration,
            receiver,
            feedback,
            heartbeat,
            decoder,
            #[cfg(windows)]
            hardware,
            assembler: Assembler::new(),
            counters,
            scope,
            scaled,
            backing,
            encoded: canvas,
            previous_canvas: None,
            early,
            candidates: Vec::new(),
            candidate_bytes: 0,
            counts: HashMap::new(),
            selection_at: now + Duration::from_millis(400),
            selected: None,
            highest: 0,
            need_keyframe: true,
            last_feedback: now - Duration::from_secs(1),
            last_fir: now - Duration::from_secs(1),
            last_nack: now,
            last_frame: now,
            pacer: crate::render::Pacer::new(settings.max_fps.min(settings.encoding.fps_ceiling())),
            watchdog: VideoWatchdog::default(),
            started: now,
            fps_ceiling: if sixty_fps { 60 } else { 30 },
            hevc_bitrate_bps: settings.hevc_bitrate_bps,
            virtual_display: settings.virtual_display,
            last_recovery: now - Duration::from_secs(1),
            decoded: 0,
            rejected: 0,
            warned: HashSet::new(),
            alternate_bank: false,
            video_started: now,
            last_stats: now,
            authenticated: 0,
            nal_types: HashMap::new(),
        };
        client.start_audio()?;
        Ok(client)
    }

    fn start_audio(&mut self) -> Result<()> {
        self.audio.take();
        let Some(converter) = &self.converter else {
            return Ok(());
        };
        match audio::Service::start(
            self.control_socket.clone(),
            &self.offers.audio_receive,
            self.audio_settings,
            self.foreground.clone(),
            self.counters.clone(),
            self.scope.clone(),
            converter,
        ) {
            Ok(service) => {
                self.audio = Some(service);
                self.audio_pending.push_back(AudioStatus::Waiting);
            }
            Err(error) => {
                self.scope.critical(
                    "media.audio.failed",
                    Level::Error,
                    crate::audit::error_attributes(&error),
                )?;
                self.audio_pending
                    .push_back(AudioStatus::Unavailable(error.to_string()));
            }
        }
        Ok(())
    }

    pub fn audio_status(&mut self) -> Result<Option<AudioStatus>> {
        if let Some(status) = self.audio_pending.pop_front() {
            return Ok(Some(status));
        }
        match self.audio.as_mut().map(audio::Service::status).transpose() {
            Ok(status) => Ok(status.flatten()),
            Err(error) => {
                self.audio.take();
                self.scope.critical(
                    "media.audio.failed",
                    Level::Error,
                    crate::audit::error_attributes(&error),
                )?;
                Ok(Some(AudioStatus::Unavailable(error.to_string())))
            }
        }
    }

    pub fn cursor_status(&mut self) -> Option<(cursor::Update, f32)> {
        self.channel
            .take_cursor()
            .map(|cursor| (cursor, self.backing.width as f32 / self.scaled.width as f32))
    }

    pub fn input(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Audio(settings) => {
                settings.validate()?;
                self.audio_settings = settings;
                if let Some(service) = &self.audio
                    && let Err(error) = service.configure(settings)
                {
                    self.scope.critical(
                        "media.audio.failed",
                        Level::Error,
                        crate::audit::error_attributes(&error),
                    )?;
                    self.audio_pending
                        .push_back(AudioStatus::Unavailable(error.to_string()));
                }
                Ok(())
            }
            Command::Refresh { .. } => self.request_keyframe(),
            Command::Configure(settings) => {
                settings.validate()?;
                if !settings.encoding.is_hevc()
                    || settings.encoding.fps_ceiling() != self.fps_ceiling
                    || settings.hevc_bitrate_bps != self.hevc_bitrate_bps
                    || settings.virtual_display != self.virtual_display
                {
                    return Err(failed(
                        "reconnect before changing the HEVC preset, Mac display, bitrate ceiling or transport",
                    ));
                }
                self.pacer.set_fps(settings.max_fps.min(self.fps_ceiling));
                Ok(())
            }
            other => self.channel.input(other, self.scaled, self.backing),
        }
    }

    pub fn request_keyframe(&mut self) -> Result<()> {
        let Some(ssrc) = self.selected else {
            return Ok(());
        };
        if self.last_fir.elapsed() < Duration::from_millis(300) {
            return Ok(());
        }
        let mut feedback = rtp::report(self.offers.video_ssrc, Some((ssrc, self.highest)));
        feedback.extend_from_slice(&rtp::fir(ssrc));
        let protected = self.feedback.protect(&feedback)?;
        send_udp(&self.control_socket, &protected, &self.counters)?;
        self.last_fir = Instant::now();
        Ok(())
    }

    fn recovery(&mut self, reason: &str) -> Result<()> {
        self.watchdog.expect_picture(Instant::now());
        self.need_keyframe = true;
        self.pacer.clear();
        if self.last_recovery.elapsed() < Duration::from_millis(300) {
            return Ok(());
        }
        self.last_recovery = Instant::now();
        self.scope.event(
            "media.hevc.recovery.requested",
            Level::Warn,
            json!({"media.recovery.reason":reason}),
        )?;
        self.decoder = native_hevc::Decoder::new().map_err(failed)?;
        #[cfg(windows)]
        if let Some(hardware) = self.hardware.as_mut()
            && let Err(error) = hardware.reset()
        {
            self.scope.event(
                "media.hevc.hardware.fallback",
                Level::Warn,
                json!({"media.decoder":"libde265","error.message":error.to_string()}),
            )?;
            self.hardware = None;
        }
        self.need_keyframe = true;
        self.request_keyframe()
    }

    fn media(&mut self, data: &[u8], now: Instant) -> Result<()> {
        let Some(packet) = self.receiver.receive(data)? else {
            self.rejected = self.rejected.saturating_add(1);
            return Ok(());
        };
        self.authenticated += 1;
        if let Some(first) = packet.payload.first() {
            *self.nal_types.entry((first >> 1) & 63).or_default() += 1;
            let hevc_kind = (first >> 1) & 63;
            let avc_kind = first & 31;
            if *first == 0x92 || (hevc_kind > 49 && matches!(avc_kind, 5 | 7 | 8 | 28)) {
                if self.decoded != 0 || self.alternate_bank {
                    return Err(failed(
                        "the Mac selected H.264 instead of the required HEVC stream",
                    ));
                }
                return self.alternate_proposal(now);
            }
        }
        self.watchdog.packet(now);
        if let Some(selected) = self.selected {
            if packet.ssrc == selected {
                self.highest = self.highest.max(packet.index);
                self.assembler.push(packet, now)?;
            } else if self.last_frame.elapsed() > Duration::from_secs(2) {
                self.selected = None;
                self.selection_at = now + Duration::from_millis(400);
                self.candidates.clear();
                self.counts.clear();
                self.candidate_bytes = 0;
                self.assembler.reset();
                self.recovery("server media stream changed")?;
            }
        } else {
            *self.counts.entry(packet.ssrc).or_default() += packet.payload.len();
            if self.candidate_bytes + packet.payload.len() > 8 * 1024 * 1024
                || self.candidates.len() >= 16_384
            {
                return Err(failed("initial HEVC stream selection exceeded its limit"));
            }
            self.candidate_bytes += packet.payload.len();
            self.candidates.push(packet);
        }
        Ok(())
    }

    fn alternate_proposal(&mut self, now: Instant) -> Result<()> {
        self.scope.critical("media.hevc.offer.renegotiated",Level::Info,json!({
            "media.codec.requested":"hevc","media.codec.observed":"h264","media.proposal.bank":123,
        }))?;
        self.audio.take();
        self.offers = Offers::with_bank(true, self.converter.is_some(), self.hevc_bitrate_bps)?;
        self.configuration = self.offers.configuration(self.fps_ceiling == 60)?;
        self.receiver = Srtp::new(&self.offers.video_receive);
        self.feedback = Srtcp::new(&self.offers.video_send);
        self.heartbeat = SrtpSender::new(&self.offers.audio_send, self.offers.audio_ssrc);
        self.decoder = native_hevc::Decoder::new().map_err(failed)?;
        self.assembler.reset();
        self.selected = None;
        self.candidates.clear();
        self.counts.clear();
        self.candidate_bytes = 0;
        self.selection_at = now + Duration::from_millis(400);
        self.need_keyframe = true;
        self.watchdog = VideoWatchdog::default();
        self.alternate_bank = true;
        self.video_started = now;
        self.channel.send(&self.configuration)?;
        self.start_audio()
    }

    fn decode_video(&mut self, unit: &rtp::Unit) -> Result<Vec<Frame>> {
        #[cfg(windows)]
        if let Some(hardware) = self.hardware.as_mut() {
            match hardware.push(&unit.annex_b, unit.timestamp) {
                Ok(frames) => return Ok(frames.into_iter().map(Frame::Gpu).collect()),
                Err(error) => {
                    self.scope.event(
                        "media.hevc.hardware.fallback",
                        Level::Warn,
                        json!({"media.decoder":"libde265","error.message":error.to_string()}),
                    )?;
                    self.hardware = None;
                    self.decoder = native_hevc::Decoder::new().map_err(failed)?;
                    self.need_keyframe = !unit.random_access;
                    if self.need_keyframe {
                        self.request_keyframe()?;
                        return Ok(Vec::new());
                    }
                }
            }
        }
        self.decoder
            .push(&unit.annex_b, unit.timestamp)
            .map_err(failed)?
            .into_iter()
            .map(|frame| {
                let size = Extent::new(frame.width, frame.height)?;
                if frame.rgba.len() != size.bytes() {
                    return Err(failed("decoder returned an invalid RGBA frame"));
                }
                Ok(Frame::Software(Framebuffer {
                    size,
                    rgba: frame.rgba,
                }))
            })
            .collect()
    }

    fn change_canvas(&mut self, size: Extent, now: Instant) -> Result<()> {
        if size.width * size.height > 16_777_216 {
            return Err(failed("negotiated HEVC canvas exceeds the pixel limit"));
        }
        if self.encoded != size {
            // UDP can deliver pictures from the previous encoder after the
            // new geometry arrives over TCP. Never display those late frames.
            self.previous_canvas = Some((self.encoded, now));
            self.encoded = size;
        }
        Ok(())
    }

    pub fn poll(&mut self) -> Result<Option<Frame>> {
        let now = Instant::now();
        let stats_interval = if self.decoded == 0 { 3 } else { 60 };
        if self.last_stats.elapsed() >= Duration::from_secs(stats_interval) {
            self.scope.event("media.hevc.status",Level::Info,json!({
                "media.packets.authenticated":self.authenticated,"media.packets.ignored":self.rejected,
                "media.nal.packet_types":self.nal_types,"media.frames.decoded":self.decoded,
                "media.pending.groups":self.assembler.pending(),"media.rtp.donl":self.assembler.donl(),"media.keyframe.required":self.need_keyframe,
                "media.selected.ssrc":self.selected,"media.proposal.bank":if self.alternate_bank{123}else{100},
            }))?;
            self.last_stats = now;
        }
        for notice in self.channel.poll()? {
            match notice {
                Notice::Canvas(size) => self.change_canvas(size, now)?,
                Notice::Layout { scaled, backing } => {
                    let changed = self.backing != backing;
                    if changed && self.encoded == self.backing {
                        self.change_canvas(backing, now)?;
                    }
                    self.scaled = scaled;
                    self.backing = backing;
                    if changed {
                        self.channel.send(&self.configuration)?;
                        self.receiver.reset();
                        self.selected = None;
                        self.selection_at = now + Duration::from_millis(400);
                        self.candidates.clear();
                        self.counts.clear();
                        self.candidate_bytes = 0;
                        self.assembler.reset();
                        self.recovery("Mac display geometry changed")?;
                        self.scope.event(
                            "media.hevc.geometry.changed",
                            Level::Info,
                            json!({
                                "media.width":backing.width,"media.height":backing.height,
                                "rfb.input.width":scaled.width,"rfb.input.height":scaled.height,
                            }),
                        )?;
                    }
                    self.channel.send(&control::refresh(backing))?;
                }
                Notice::Unsupported(encoding) if self.warned.insert(encoding) => {
                    self.scope.event(
                        "media.control.ignored",
                        Level::Warn,
                        json!({"rfb.control.encoding":encoding}),
                    )?;
                }
                Notice::Unsupported(_) => (),
            }
        }
        while let Some(packet) = self.early.pop_front() {
            self.media(&packet, now)?;
        }
        let mut units = self.assembler.ready(now)?;
        let mut buffer = [0; 65_536];
        for _ in 0..1024 {
            if !units.is_empty() {
                break;
            }
            match self.video_socket.recv(&mut buffer) {
                Ok(length) => {
                    self.counters
                        .received
                        .fetch_add(length as u64, Ordering::Relaxed);
                    if length >= 2 && !(192..=223).contains(&buffer[1]) {
                        self.media(&buffer[..length], now)?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(error) => return Err(error.into()),
            }
            units.extend(self.assembler.ready(Instant::now())?);
        }
        if self.audio.as_ref().is_none_or(|audio| !audio.receiving()) {
            for _ in 0..128 {
                match self.control_socket.recv(&mut buffer) {
                    Ok(length) => {
                        self.counters
                            .received
                            .fetch_add(length as u64, Ordering::Relaxed);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        if self.selected.is_none() && now >= self.selection_at && !self.counts.is_empty() {
            let mut ids: Vec<_> = self.counts.keys().copied().collect();
            ids.sort_unstable();
            let mut cohorts: Vec<Vec<u32>> = Vec::new();
            for id in ids {
                if let Some(cohort) = cohorts.last_mut()
                    && cohort.len() < 4
                    && cohort
                        .last()
                        .is_some_and(|last| last.checked_add(1) == Some(id))
                {
                    cohort.push(id);
                } else {
                    cohorts.push(vec![id]);
                }
            }
            let cohort = cohorts
                .iter()
                .max_by_key(|group| group.iter().map(|id| self.counts[id]).sum::<usize>())
                .expect("nonempty streams");
            let selected = cohort[0];
            self.scope.event(
                "media.hevc.ssrc.selected",
                Level::Info,
                json!({
                    "media.selected.ssrc":selected,"media.stream.cohort":cohort,
                    "media.stream.packet_bytes":self.counts,
                }),
            )?;
            self.selected = Some(selected);
            self.highest = 0;
            let mut candidates: Vec<_> = self
                .candidates
                .drain(..)
                .filter(|packet| packet.ssrc == selected)
                .collect();
            candidates.sort_by_key(|packet| packet.index);
            for packet in candidates {
                self.highest = self.highest.max(packet.index);
                self.assembler.push(packet, now)?;
            }
            self.candidate_bytes = 0;
            self.request_keyframe()?;
        }
        if self.last_feedback.elapsed() >= Duration::from_secs(1) {
            send_udp(&self.video_socket, &[0], &self.counters)?;
            self.channel.send(&[3, 1, 0, 0, 0, 0, 0, 1, 0, 1])?;
            let mut report = rtp::sender_report(self.offers.video_ssrc)?;
            report.extend_from_slice(&rtp::report(
                self.offers.video_ssrc,
                self.selected.map(|ssrc| (ssrc, self.highest)),
            ));
            let protected = self.feedback.protect(&report)?;
            send_udp(&self.control_socket, &protected, &self.counters)?;
            let heartbeat = self
                .heartbeat
                .heartbeat((self.started.elapsed().as_millis() as u32).wrapping_mul(90))?;
            send_udp(&self.control_socket, &heartbeat, &self.counters)?;
            self.last_feedback = now;
        }
        let missing = self.assembler.missing(now);
        if !missing.is_empty() && self.last_nack.elapsed() > Duration::from_millis(20) {
            if let Some(selected) = self.selected {
                let mut packet =
                    rtp::report(self.offers.video_ssrc, Some((selected, self.highest)));
                packet.extend_from_slice(&rtp::nack(self.offers.video_ssrc, selected, &missing));
                let protected = self.feedback.protect(&packet)?;
                send_udp(&self.control_socket, &protected, &self.counters)?;
            }
            self.last_nack = now;
        }
        units.extend(self.assembler.ready(Instant::now())?);
        if let Some(loss) = self.assembler.take_loss() {
            self.scope.event(
                "media.hevc.rtp.loss",
                Level::Warn,
                json!({"media.rtp.loss":&loss}),
            )?;
            self.recovery(&loss.reason)?;
        }
        for unit in units {
            self.watchdog.expect_picture(now);
            if self.need_keyframe && !unit.random_access {
                continue;
            }
            if unit.random_access {
                self.need_keyframe = false;
            }
            match self.decode_video(&unit) {
                Ok(frames) => {
                    for frame in frames {
                        let size = frame.size();
                        if size != self.encoded {
                            if self.previous_canvas.is_some_and(|(previous, changed)| {
                                size == previous
                                    && now.duration_since(changed) < Duration::from_secs(3)
                            }) {
                                continue;
                            }
                            return Err(failed(format!(
                                "decoded picture {}x{} does not match the negotiated {}x{} canvas",
                                size.width, size.height, self.encoded.width, self.encoded.height
                            )));
                        }
                        self.backing = size;
                        self.decoded += 1;
                        self.last_frame = now;
                        self.watchdog.decoded();
                        self.counters
                            .encoding
                            .store(COUNTER_ENCODING, Ordering::Relaxed);
                        #[cfg(windows)]
                        self.counters.decoder.store(
                            if matches!(&frame, Frame::Gpu(_)) {
                                self.hardware
                                    .as_ref()
                                    .map_or(0, |decoder| decoder.counter())
                            } else {
                                0
                            },
                            Ordering::Relaxed,
                        );
                        self.pacer.submit(frame);
                    }
                }
                Err(error) => {
                    self.scope.event(
                        "media.hevc.decode.failed",
                        Level::Warn,
                        json!({"error.type":"hevc_decode"}),
                    )?;
                    self.recovery("HEVC decoder requested a new reference picture")?;
                    if self.decoded == 0 && self.started.elapsed() > Duration::from_secs(12) {
                        return Err(failed(format!(
                            "could not decode the Mac's HEVC stream: {error}"
                        )));
                    }
                }
            }
        }
        if self.need_keyframe {
            self.request_keyframe()?;
        }
        if self.decoded == 0 && self.video_started.elapsed() > Duration::from_secs(20) {
            return Err(Error::ConnectionLost(
                "no decodable HEVC video arrived. Check UDP ports 5900/5901, the Mac's High Performance support, and its active desktop.".into(),
            ));
        }
        if self.decoded != 0 {
            match self.watchdog.action(now) {
                VideoAction::Wait => (),
                VideoAction::Recover => {
                    self.recovery("HEVC video is arriving without a complete picture")?;
                }
                VideoAction::Reconnect => {
                    return Err(Error::ConnectionLost(
                        "HEVC video could not recover a complete picture".into(),
                    ));
                }
            }
        }
        Ok(self.pacer.take(Instant::now()))
    }
}
