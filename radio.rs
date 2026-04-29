use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct Channel {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub background: Color,
    pub foreground: Color,
    pub accent: Color,
    pub secondary: Color,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecordingFormat {
    Wav,
    Mp3,
    Flac,
    Ogg,
}

impl RecordingFormat {
    fn next(self) -> Self {
        match self {
            Self::Wav => Self::Mp3,
            Self::Mp3 => Self::Flac,
            Self::Flac => Self::Ogg,
            Self::Ogg => Self::Wav,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedConfig {
    theme_name: String,
    recording_format: RecordingFormat,
}

impl Default for PersistedConfig {
    fn default() -> Self {
        Self {
            theme_name: "tokyo_night".to_string(),
            recording_format: RecordingFormat::Mp3,
        }
    }
}

pub struct AudioEngine {
    shared: Arc<Mutex<AudioShared>>,
    _stream: cpal::Stream,
    worker: Option<Worker>,
    music_dir: PathBuf,
}

struct Worker {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

struct RecorderProcess {
    child: Child,
    stdin: ChildStdin,
    path: String,
}

struct AudioShared {
    buffer: VecDeque<f32>,
    start_index: u64,
    live_index: u64,
    play_index: u64,
    max_samples: usize,
    sample_rate: u32,
    channels: u16,
    volume: f32,
    paused: bool,
    bars: Vec<u64>,
    status: String,
    now_playing: String,
    recording_requested: bool,
    recording_active: bool,
    recording_path: String,
    recording_format: RecordingFormat,
    cava_active: bool,
}

pub struct App {
    channels: Vec<Channel>,
    list_state: ListState,
    engine: AudioEngine,
    themes: Vec<Theme>,
    theme_index: usize,
    config: PersistedConfig,
}

impl AudioShared {
    fn new(sample_rate: u32, channels: u16, recording_format: RecordingFormat) -> Self {
        let max_samples = sample_rate as usize * channels as usize * 180;
        Self {
            buffer: VecDeque::with_capacity(max_samples),
            start_index: 0,
            live_index: 0,
            play_index: 0,
            max_samples,
            sample_rate,
            channels,
            volume: 0.8,
            paused: false,
            bars: vec![0; 64],
            status: "Idle".to_string(),
            now_playing: "Nothing playing".to_string(),
            recording_requested: false,
            recording_active: false,
            recording_path: String::new(),
            recording_format,
            cava_active: false,
        }
    }

    fn append_samples(&mut self, samples: &[f32]) {
        let was_empty = self.live_index == self.start_index;
        for sample in samples {
            if self.buffer.len() == self.max_samples {
                self.buffer.pop_front();
                self.start_index += 1;
            }
            self.buffer.push_back(*sample);
            self.live_index += 1;
        }
        if was_empty {
            self.play_index = self.start_index;
        }
        if self.play_index < self.start_index {
            self.play_index = self.start_index;
        }
        if !self.cava_active {
            self.update_bars(samples);
        }
    }

    fn update_bars(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        let mono = samples
            .chunks(self.channels as usize)
            .map(|frame| frame.iter().map(|v| v.abs()).sum::<f32>() / frame.len() as f32)
            .collect::<Vec<_>>();
        let band_len = (mono.len() / self.bars.len().max(1)).max(1);
        for (index, bar) in self.bars.iter_mut().enumerate() {
            let start = index * band_len;
            let end = ((index + 1) * band_len).min(mono.len());
            let value = if start >= end {
                0.0
            } else {
                mono[start..end].iter().sum::<f32>() / (end - start) as f32
            };
            let target = (value * 180.0).clamp(0.0, 100.0) as f64;
            let current = *bar as f64;
            *bar = (current * 0.72 + target * 0.28).round() as u64;
        }
    }

    fn rewind_seconds(&mut self, seconds: u64) {
        let delta = seconds * self.sample_rate as u64 * self.channels as u64;
        self.play_index = self.live_index.saturating_sub(delta).max(self.start_index);
        self.paused = false;
    }

    fn go_live(&mut self) {
        self.play_index = self.live_index;
        self.paused = false;
    }

    fn buffered_seconds(&self) -> u64 {
        if self.sample_rate == 0 || self.channels == 0 {
            return 0;
        }
        (self.live_index.saturating_sub(self.start_index))
            / self.sample_rate as u64
            / self.channels as u64
    }

    fn delay_seconds(&self) -> u64 {
        if self.sample_rate == 0 || self.channels == 0 {
            return 0;
        }
        (self.live_index.saturating_sub(self.play_index))
            / self.sample_rate as u64
            / self.channels as u64
    }
}

impl AudioEngine {
    pub fn new(music_dir: PathBuf, recording_format: RecordingFormat) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device"))?;
        let supported = device.default_output_config()?;
        let sample_rate = supported.sample_rate().0;
        let channels = supported.channels();
        let shared = Arc::new(Mutex::new(AudioShared::new(
            sample_rate,
            channels,
            recording_format,
        )));
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => {
                build_stream::<f32>(&device, &supported.into(), shared.clone())?
            }
            cpal::SampleFormat::I16 => {
                build_stream::<i16>(&device, &supported.into(), shared.clone())?
            }
            cpal::SampleFormat::U16 => {
                build_stream::<u16>(&device, &supported.into(), shared.clone())?
            }
            other => return Err(anyhow!("unsupported sample format: {other:?}")),
        };
        stream.play()?;
        spawn_cava_monitor(shared.clone());
        Ok(Self {
            shared,
            _stream: stream,
            worker: None,
            music_dir,
        })
    }

    pub fn play(&mut self, channel: Channel) {
        self.stop_worker();
        if let Ok(mut shared) = self.shared.lock() {
            shared.buffer.clear();
            shared.start_index = 0;
            shared.live_index = 0;
            shared.play_index = 0;
            shared.paused = false;
            shared.status = format!("Connecting to {}", channel.name);
            shared.now_playing = channel.name.clone();
            shared.recording_active = false;
            shared.recording_path.clear();
        }
        let shared = self.shared.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = stop.clone();
        let music_dir = self.music_dir.clone();
        let handle = thread::spawn(move || {
            run_stream_worker(channel, shared, stop_worker, music_dir);
        });
        self.worker = Some(Worker { stop, handle });
    }

    pub fn toggle_pause(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.paused = !shared.paused;
            shared.status = if shared.paused {
                "Paused".to_string()
            } else {
                "Playing".to_string()
            };
        }
    }

    pub fn set_volume_delta(&self, delta: f32) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.volume = (shared.volume + delta).clamp(0.0, 2.0);
        }
    }

    pub fn rewind_minutes(&self, minutes: u64) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.rewind_seconds(minutes * 60);
            shared.status = format!("Rewound {minutes} minute(s)");
        }
    }

    pub fn go_live(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.go_live();
            shared.status = "Live".to_string();
        }
    }

    pub fn toggle_recording(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.recording_requested = !shared.recording_requested;
            if !shared.recording_requested {
                shared.recording_active = false;
                shared.recording_path.clear();
            }
        }
    }

    pub fn cycle_recording_format(&self) -> RecordingFormat {
        let mut format = RecordingFormat::Wav;
        if let Ok(mut shared) = self.shared.lock() {
            shared.recording_format = shared.recording_format.next();
            format = shared.recording_format;
            shared.status = format!("Recording format: {}", format.extension());
        }
        format
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        let shared = self.shared.lock().expect("audio shared");
        AudioSnapshot {
            volume: shared.volume,
            paused: shared.paused,
            bars: shared.bars.clone(),
            status: shared.status.clone(),
            now_playing: shared.now_playing.clone(),
            buffered_seconds: shared.buffered_seconds(),
            delay_seconds: shared.delay_seconds(),
            recording_requested: shared.recording_requested,
            recording_active: shared.recording_active,
            recording_path: shared.recording_path.clone(),
            recording_format: shared.recording_format,
            cava_active: shared.cava_active,
        }
    }

    fn stop_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop.store(true, Ordering::SeqCst);
            let _ = worker.handle.join();
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

pub struct AudioSnapshot {
    pub volume: f32,
    pub paused: bool,
    pub bars: Vec<u64>,
    pub status: String,
    pub now_playing: String,
    pub buffered_seconds: u64,
    pub delay_seconds: u64,
    pub recording_requested: bool,
    pub recording_active: bool,
    pub recording_path: String,
    pub recording_format: RecordingFormat,
    pub cava_active: bool,
}

pub fn run(music_dir: PathBuf) -> Result<()> {
    let channels = fetch_channels()?;
    let themes = load_alacritty_themes()?;
    let config = load_persisted_config();
    let theme_index = themes
        .iter()
        .position(|theme| theme.name.eq_ignore_ascii_case(&config.theme_name))
        .unwrap_or(0);
    let mut terminal = setup_terminal()?;
    let engine = AudioEngine::new(music_dir, config.recording_format)?;
    let mut app = App::new(channels, engine, themes, theme_index, config);
    app.run(&mut terminal)?;
    restore_terminal(terminal)?;
    Ok(())
}

impl App {
    fn new(
        channels: Vec<Channel>,
        engine: AudioEngine,
        themes: Vec<Theme>,
        theme_index: usize,
        config: PersistedConfig,
    ) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            channels,
            list_state,
            engine,
            themes,
            theme_index,
            config,
        }
    }

    fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        loop {
            let snapshot = self.engine.snapshot();
            let theme = self.themes[self.theme_index].clone();
            terminal.draw(|frame| self.draw(frame, &snapshot, &theme))?;
            if event::poll(Duration::from_millis(80))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Down | KeyCode::Char('j') => self.next_channel(),
                        KeyCode::Up | KeyCode::Char('k') => self.prev_channel(),
                        KeyCode::Enter => {
                            if let Some(channel) = self.selected_channel() {
                                self.engine.play(channel);
                            }
                        }
                        KeyCode::Char(' ') => self.engine.toggle_pause(),
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            self.engine.set_volume_delta(0.05)
                        }
                        KeyCode::Char('-') => self.engine.set_volume_delta(-0.05),
                        KeyCode::Char('1') => self.engine.rewind_minutes(1),
                        KeyCode::Char('2') => self.engine.rewind_minutes(2),
                        KeyCode::Char('3') => self.engine.rewind_minutes(3),
                        KeyCode::Char('l') => self.engine.go_live(),
                        KeyCode::Char('t') => self.next_theme(),
                        KeyCode::Char('T') => self.prev_theme(),
                        KeyCode::Char('m') => self.next_recording_format(),
                        KeyCode::Char('r') => self.engine.toggle_recording(),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn draw(&self, frame: &mut ratatui::Frame<'_>, snapshot: &AudioSnapshot, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(30), Constraint::Min(56)])
            .split(frame.area());
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8),
                Constraint::Min(14),
                Constraint::Length(3),
                Constraint::Length(6),
            ])
            .split(chunks[1]);

        self.draw_channels(frame, chunks[0], theme);
        self.draw_now_playing(frame, right[0], snapshot, theme);
        self.draw_visualizer(frame, right[1], snapshot, theme);
        self.draw_buffer(frame, right[2], snapshot, theme);
        self.draw_help(frame, right[3], snapshot, theme);
    }

    fn draw_channels(&self, frame: &mut ratatui::Frame<'_>, area: Rect, theme: &Theme) {
        let items = self
            .channels
            .iter()
            .map(|channel| {
                ListItem::new(Line::from(vec![
                    Span::styled("󰓃 ", Style::default().fg(theme.secondary)),
                    Span::raw(channel.name.clone()),
                ]))
            })
            .collect::<Vec<_>>();
        let list = List::new(items)
            .block(
                Block::default()
                    .title("Sveriges Radio")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.accent))
                    .style(Style::default().bg(theme.background).fg(theme.foreground)),
            )
            .highlight_style(
                Style::default()
                    .fg(theme.background)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        let mut state = self.list_state.clone();
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_now_playing(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        snapshot: &AudioSnapshot,
        theme: &Theme,
    ) {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("󰐊 ", Style::default().fg(theme.secondary)),
                Span::styled(
                    snapshot.now_playing.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(format!(
                "Status: {}",
                if snapshot.paused {
                    "Paused"
                } else {
                    &snapshot.status
                }
            )),
            Line::from(format!("Volume: {:>3.0}%", snapshot.volume * 100.0)),
            Line::from(format!("Theme: {}", self.themes[self.theme_index].name)),
            Line::from(format!("Format: {}", snapshot.recording_format.extension())),
        ];
        if snapshot.recording_requested {
            lines.push(Line::from(format!(
                "Recording: {}",
                if snapshot.recording_active {
                    snapshot.recording_path.clone()
                } else {
                    "arming".to_string()
                }
            )));
        } else {
            lines.push(Line::from("Recording: off"));
        }
        lines.push(Line::from(format!(
            "Visualizer: {}",
            if snapshot.cava_active {
                "cava"
            } else {
                "internal fallback"
            }
        )));
        let widget = Paragraph::new(lines).block(
            Block::default()
                .title("Now Playing")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .style(Style::default().bg(theme.background).fg(theme.foreground)),
        );
        frame.render_widget(widget, area);
    }

    fn draw_visualizer(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        snapshot: &AudioSnapshot,
        theme: &Theme,
    ) {
        let inner_height = area.height.saturating_sub(2);
        let inner_width = area.width.saturating_sub(2);
        let columns = resample_bars(&snapshot.bars, inner_width as usize);
        let lines = render_visualizer_lines(&columns, inner_height as usize, theme);
        let widget = Paragraph::new(lines).block(
            Block::default()
                .title("Visualizer")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.secondary))
                .style(Style::default().bg(theme.background).fg(theme.foreground)),
        );
        frame.render_widget(widget, area);
    }

    fn draw_buffer(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        snapshot: &AudioSnapshot,
        theme: &Theme,
    ) {
        let ratio = (snapshot.delay_seconds as f64 / 180.0).clamp(0.0, 1.0);
        let gauge = Gauge::default()
            .block(
                Block::default()
                    .title(format!(
                        "Buffer  live-{}s / stored {}s",
                        snapshot.delay_seconds, snapshot.buffered_seconds
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.accent)),
            )
            .gauge_style(Style::default().fg(theme.secondary).bg(theme.background))
            .ratio(ratio);
        frame.render_widget(gauge, area);
    }

    fn draw_help(
        &self,
        frame: &mut ratatui::Frame<'_>,
        area: Rect,
        snapshot: &AudioSnapshot,
        theme: &Theme,
    ) {
        let lines = vec![
            Line::from("↑/↓ or j/k select channel"),
            Line::from("Enter play  Space pause  +/- volume"),
            Line::from("1/2/3 rewind minutes  l live"),
            Line::from("t/T theme  m format  r record  q quit"),
            Line::from(format!(
                "Mode: {}",
                if snapshot.delay_seconds == 0 {
                    "live"
                } else {
                    "timeshift"
                }
            )),
        ];
        let widget = Paragraph::new(lines).block(
            Block::default()
                .title("Controls")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.secondary))
                .style(Style::default().bg(theme.background).fg(theme.foreground)),
        );
        frame.render_widget(widget, area);
    }

    fn next_channel(&mut self) {
        let next = match self.list_state.selected() {
            Some(index) if index + 1 < self.channels.len() => index + 1,
            _ => 0,
        };
        self.list_state.select(Some(next));
    }

    fn prev_channel(&mut self) {
        let prev = match self.list_state.selected() {
            Some(index) if index > 0 => index - 1,
            _ => self.channels.len().saturating_sub(1),
        };
        self.list_state.select(Some(prev));
    }

    fn next_theme(&mut self) {
        self.theme_index = (self.theme_index + 1) % self.themes.len();
        self.persist_theme();
    }

    fn prev_theme(&mut self) {
        self.theme_index = if self.theme_index == 0 {
            self.themes.len().saturating_sub(1)
        } else {
            self.theme_index - 1
        };
        self.persist_theme();
    }

    fn next_recording_format(&mut self) {
        let format = self.engine.cycle_recording_format();
        self.config.recording_format = format;
        let _ = save_persisted_config(&self.config);
    }

    fn persist_theme(&mut self) {
        self.config.theme_name = self.themes[self.theme_index].name.clone();
        let _ = save_persisted_config(&self.config);
    }

    fn selected_channel(&self) -> Option<Channel> {
        let index = self.list_state.selected()?;
        self.channels.get(index).cloned()
    }
}

fn run_stream_worker(
    channel: Channel,
    shared: Arc<Mutex<AudioShared>>,
    stop: Arc<AtomicBool>,
    music_dir: PathBuf,
) {
    let client = Client::builder()
        .user_agent("srtui/0.1")
        .build()
        .expect("client");
    let response = match client.get(&channel.url).send() {
        Ok(response) => response,
        Err(err) => {
            if let Ok(mut state) = shared.lock() {
                state.status = format!("Connection failed: {err}");
            }
            return;
        }
    };
    if let Ok(mut state) = shared.lock() {
        state.status = "Streaming".to_string();
    }
    let mut decoder = minimp3::Decoder::new(BufReader::new(response));
    let mut recorder: Option<RecorderProcess> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match decoder.next_frame() {
            Ok(frame) => {
                let (dst_rate, dst_channels, want_recording, current_name, format) = {
                    let state = shared.lock().expect("shared");
                    (
                        state.sample_rate,
                        state.channels,
                        state.recording_requested,
                        state.now_playing.clone(),
                        state.recording_format,
                    )
                };
                let samples = resample_interleaved(
                    &frame.data,
                    frame.sample_rate as u32,
                    frame.channels as u16,
                    dst_rate,
                    dst_channels,
                );
                if let Ok(mut state) = shared.lock() {
                    state.append_samples(&samples);
                    state.status = "Streaming".to_string();
                    if want_recording && recorder.is_none() {
                        match start_recorder(&music_dir, &current_name, dst_rate, dst_channels, format)
                        {
                            Ok(created) => {
                                state.recording_active = true;
                                state.recording_path = created.path.clone();
                                recorder = Some(created);
                            }
                            Err(err) => {
                                state.status = format!("Recording failed: {err}");
                                state.recording_requested = false;
                            }
                        }
                    }
                    if !want_recording && recorder.is_some() {
                        if let Some(open) = recorder.take() {
                            let _ = stop_recorder(open);
                        }
                        state.recording_active = false;
                        state.recording_path.clear();
                    }
                }
                if let Some(open) = recorder.as_mut() {
                    let mut bytes = Vec::with_capacity(samples.len() * 2);
                    for sample in &samples {
                        let scaled = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        bytes.extend_from_slice(&scaled.to_le_bytes());
                    }
                    if open.stdin.write_all(&bytes).is_err() {
                        break;
                    }
                }
            }
            Err(minimp3::Error::Eof) => break,
            Err(_) => continue,
        }
    }
    if let Some(open) = recorder {
        let _ = stop_recorder(open);
    }
}

fn start_recorder(
    music_dir: &Path,
    name: &str,
    sample_rate: u32,
    channels: u16,
    format: RecordingFormat,
) -> Result<RecorderProcess> {
    let path = recording_path(music_dir, name, format);
    let mut command = Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-f")
        .arg("s16le")
        .arg("-ar")
        .arg(sample_rate.to_string())
        .arg("-ac")
        .arg(channels.to_string())
        .arg("-i")
        .arg("pipe:0")
        .arg("-y");
    match format {
        RecordingFormat::Wav => {}
        RecordingFormat::Mp3 => {
            command.arg("-c:a").arg("libmp3lame").arg("-b:a").arg("192k");
        }
        RecordingFormat::Flac => {
            command.arg("-c:a").arg("flac");
        }
        RecordingFormat::Ogg => {
            command.arg("-c:a").arg("libvorbis").arg("-q:a").arg("5");
        }
    }
    command.arg(&path);
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::null()).spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open ffmpeg stdin"))?;
    Ok(RecorderProcess {
        child,
        stdin,
        path: path.to_string_lossy().to_string(),
    })
}

fn stop_recorder(mut recorder: RecorderProcess) -> Result<()> {
    drop(recorder.stdin);
    let status = recorder.child.wait()?;
    if !status.success() {
        return Err(anyhow!("ffmpeg exited with {status}"));
    }
    Ok(())
}

fn spawn_cava_monitor(shared: Arc<Mutex<AudioShared>>) {
    let config_path = std::env::temp_dir().join(format!("srtui-cava-{}.conf", std::process::id()));
    let config = "\
[general]
bars = 32
bars = 64
framerate = 60
sensitivity = 100

[output]
method = raw
raw_target = /dev/stdout
data_format = ascii
ascii_max_range = 100
";
    if fs::write(&config_path, config).is_err() {
        return;
    }
    let mut child = match Command::new("cava")
        .arg("-p")
        .arg(&config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => return,
    };
    if let Ok(mut state) = shared.lock() {
        state.cava_active = true;
    }
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let bars = line
                        .trim()
                        .split(';')
                        .filter_map(|part| part.parse::<u64>().ok())
                        .map(|value| value.min(100))
                        .collect::<Vec<_>>();
                    if bars.is_empty() {
                        continue;
                    }
                    if let Ok(mut state) = shared.lock() {
                        for (index, bar) in state.bars.iter_mut().enumerate() {
                            let next = bars.get(index).copied().unwrap_or(0) as f64;
                            let current = *bar as f64;
                            *bar = (current * 0.68 + next * 0.32).round() as u64;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_file(config_path);
        if let Ok(mut state) = shared.lock() {
            state.cava_active = false;
        }
    });
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<Mutex<AudioShared>>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            let mut state = match shared.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            for frame in data.chunks_mut(channels) {
                for sample in frame.iter_mut() {
                    let value = if state.paused || state.play_index >= state.live_index {
                        0.0
                    } else {
                        let offset = state.play_index.saturating_sub(state.start_index) as usize;
                        let sample = state.buffer.get(offset).copied().unwrap_or(0.0);
                        state.play_index += 1;
                        sample * state.volume
                    };
                    *sample = T::from_sample(value);
                }
            }
        },
        move |err| {
            eprintln!("{err}");
        },
        None,
    )?;
    Ok(stream)
}

fn resample_interleaved(
    input: &[i16],
    src_rate: u32,
    src_channels: u16,
    dst_rate: u32,
    dst_channels: u16,
) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    let src_channels_usize = src_channels as usize;
    let dst_channels_usize = dst_channels as usize;
    let src_frames = input.len() / src_channels_usize;
    if src_frames == 0 {
        return Vec::new();
    }
    let dst_frames = ((src_frames as f64 * dst_rate as f64) / src_rate as f64).max(1.0) as usize;
    let mut output = Vec::with_capacity(dst_frames * dst_channels_usize);
    for frame_index in 0..dst_frames {
        let src_pos = frame_index as f64 * src_rate as f64 / dst_rate as f64;
        let base = src_pos.floor() as usize;
        let next = (base + 1).min(src_frames.saturating_sub(1));
        let frac = (src_pos - base as f64) as f32;
        for channel_index in 0..dst_channels_usize {
            let src_channel = channel_index.min(src_channels_usize.saturating_sub(1));
            let a = input[base * src_channels_usize + src_channel] as f32 / i16::MAX as f32;
            let b = input[next * src_channels_usize + src_channel] as f32 / i16::MAX as f32;
            output.push(a + (b - a) * frac);
        }
    }
    output
}

fn recording_path(music_dir: &Path, name: &str, format: RecordingFormat) -> PathBuf {
    let _ = fs::create_dir_all(music_dir);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let safe = name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    music_dir.join(format!("{safe}-{stamp}.{}", format.extension()))
}

fn resample_bars(source: &[u64], width: usize) -> Vec<u64> {
    if width == 0 || source.is_empty() {
        return Vec::new();
    }
    if source.len() == 1 {
        return vec![source[0]; width];
    }
    let mut out = Vec::with_capacity(width);
    let max_index = source.len() - 1;
    for x in 0..width {
        let pos = x as f64 * max_index as f64 / width.saturating_sub(1).max(1) as f64;
        let left = pos.floor() as usize;
        let right = (left + 1).min(max_index);
        let frac = pos - left as f64;
        let a = source[left] as f64;
        let b = source[right] as f64;
        out.push((a + (b - a) * frac).round() as u64);
    }
    out
}

fn render_visualizer_lines(columns: &[u64], height: usize, theme: &Theme) -> Vec<Line<'static>> {
    if height == 0 || columns.is_empty() {
        return vec![Line::from("")];
    }
    let mut lines = Vec::with_capacity(height);
    for row in 0..height {
        let mut spans = Vec::with_capacity(columns.len());
        let row_from_bottom = height - row;
        for &column in columns {
            let fill_rows = ((column as f64 / 100.0) * height as f64).clamp(0.0, height as f64);
            let partial = (fill_rows.fract() * 8.0).round() as usize;
            let whole_rows = fill_rows.floor() as usize;
            let ch = if row_from_bottom <= whole_rows {
                "█"
            } else if row_from_bottom == whole_rows + 1 {
                match partial {
                    0 | 1 => "▁",
                    2 => "▂",
                    3 => "▃",
                    4 => "▄",
                    5 => "▅",
                    6 => "▆",
                    7 => "▇",
                    _ => "█",
                }
            } else {
                " "
            };
            let style = if row < height / 3 {
                Style::default().fg(theme.secondary)
            } else if row < (height * 2) / 3 {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.foreground)
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

pub fn fetch_channels() -> Result<Vec<Channel>> {
    Ok(vec![
        channel("Ekot sänder direkt", "https://live1.sr.se/ekotdirekt-mp3-96"),
        channel("P1", "https://live1.sr.se/p1-mp3-96"),
        channel("P2", "https://live1.sr.se/p2-mp3-96"),
        channel("P3", "https://live1.sr.se/p3-mp3-96"),
        channel("P3 Din gata", "https://live1.sr.se/dingata-mp3-96"),
        channel("P4 Blekinge", "https://live1.sr.se/p4blek-mp3-96"),
        channel("P4 Dalarna", "https://live1.sr.se/p4dala-mp3-96"),
        channel("P4 Digital", "https://live1.sr.se/p4digi-mp3-96"),
        channel("P4 Gotland", "https://live1.sr.se/p4gotl-mp3-96"),
        channel("P4 Gävleborg", "https://live1.sr.se/p4gavl-mp3-96"),
        channel("P4 Göteborg", "https://live1.sr.se/p4gbg-mp3-96"),
        channel("P4 Halland", "https://live1.sr.se/p4hall-mp3-96"),
        channel("P4 Jämtland", "https://live1.sr.se/p4jmtl-mp3-96"),
        channel("P4 Jönköping", "https://live1.sr.se/p4jkpg-mp3-96"),
        channel("P4 Kalmar", "https://live1.sr.se/p4kalm-mp3-96"),
        channel("P4 Kristianstad", "https://live1.sr.se/p4krist-mp3-96"),
        channel("P4 Kronoberg", "https://live1.sr.se/p4kron-mp3-96"),
        channel("P4 Malmöhus", "https://live1.sr.se/p4malm-mp3-96"),
        channel("P4 Norrbotten", "https://live1.sr.se/p4nbtn-mp3-96"),
        channel("P4 Plus", "https://live1.sr.se/p4plus-mp3-96"),
        channel("P4 Sjuhärad", "https://live1.sr.se/p4sju-mp3-96"),
        channel("P4 Skaraborg", "https://live1.sr.se/p4skbg-mp3-96"),
        channel("P4 Stockholm", "https://live1.sr.se/p4sth-mp3-96"),
        channel("P4 Sörmland", "https://live1.sr.se/p4sorm-mp3-96"),
        channel("P4 Uppland", "https://live1.sr.se/p4uppl-mp3-96"),
        channel("P4 Värmland", "https://live1.sr.se/p4vrml-mp3-96"),
        channel("P4 Väst", "https://live1.sr.se/p4vast-mp3-96"),
        channel("P4 Västerbotten", "https://live1.sr.se/p4vbtn-mp3-96"),
        channel("P4 Västernorrland", "https://live1.sr.se/p4vnrl-mp3-96"),
        channel("P4 Västmanland", "https://live1.sr.se/p4vstm-mp3-96"),
        channel("P4 Örebro", "https://live1.sr.se/p4oreb-mp3-96"),
        channel("P4 Östergötland", "https://live1.sr.se/p4ostg-mp3-96"),
        channel("P6", "https://live1.sr.se/p6-mp3-96"),
        channel("Radioapans knattekanal", "https://live1.sr.se/knattekanalen-mp3-96"),
        channel("Sameradion", "https://live1.sr.se/sameradion-mp3-96"),
        channel("Sveriges Radio Finska", "https://live1.sr.se/finska-mp3-96"),
    ])
}

fn channel(name: &str, url: &str) -> Channel {
    Channel {
        name: name.to_string(),
        url: url.to_string(),
    }
}

fn load_alacritty_themes() -> Result<Vec<Theme>> {
    let root = home_dir().join(".config").join("alacritty");
    let mut files = Vec::new();
    collect_theme_files(&root, &mut files)?;
    let mut names = HashSet::new();
    let mut themes = Vec::new();
    for path in files {
        if let Ok(theme) = parse_theme_file(&path) {
            let key = theme.name.to_lowercase();
            if names.insert(key) {
                themes.push(theme);
            }
        }
    }
    themes.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));
    if themes.is_empty() {
        return Err(anyhow!("no Alacritty TOML themes found"));
    }
    Ok(themes)
}

fn collect_theme_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_theme_files(&path, files)?;
            continue;
        }
        let is_toml = path.extension().and_then(|ext| ext.to_str()) == Some("toml");
        let is_backup = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.contains("backup") || name.contains(".bak"))
            .unwrap_or(false);
        if is_toml && !is_backup {
            files.push(path);
        }
    }
    Ok(())
}

fn parse_theme_file(path: &Path) -> Result<Theme> {
    let text = fs::read_to_string(path)?;
    let mut section = String::new();
    let mut background = None;
    let mut foreground = None;
    let mut green = None;
    let mut blue = None;
    let mut cyan = None;
    let mut yellow = None;
    let mut magenta = None;
    let mut white = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string();
            continue;
        }
        let Some((key, raw_value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = raw_value.trim().trim_matches('"').trim_matches('\'');
        match (section.as_str(), key) {
            ("colors.primary", "background") => background = Some(value.to_string()),
            ("colors.primary", "foreground") => foreground = Some(value.to_string()),
            ("colors.normal", "green") => green = Some(value.to_string()),
            ("colors.normal", "blue") => blue = Some(value.to_string()),
            ("colors.normal", "cyan") => cyan = Some(value.to_string()),
            ("colors.normal", "yellow") => yellow = Some(value.to_string()),
            ("colors.normal", "magenta") => magenta = Some(value.to_string()),
            ("colors.normal", "white") => white = Some(value.to_string()),
            _ => {}
        }
    }
    let background = parse_hex_color(background.as_deref())?;
    let foreground = parse_hex_color(foreground.as_deref())?;
    let accent = parse_hex_color(green.as_deref().or(blue.as_deref()).or(cyan.as_deref()))?;
    let secondary = parse_hex_color(
        yellow
            .as_deref()
            .or(magenta.as_deref())
            .or(white.as_deref()),
    )?;
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow!("bad theme filename"))?
        .to_string();
    Ok(Theme {
        name,
        background,
        foreground,
        accent,
        secondary,
    })
}

fn parse_hex_color(value: Option<&str>) -> Result<Color> {
    let raw = value.ok_or_else(|| anyhow!("missing color string"))?;
    let hex = raw.trim().trim_start_matches('#').trim_start_matches("0x");
    if hex.len() != 6 {
        return Err(anyhow!("unsupported color format: {raw}"));
    }
    let r = u8::from_str_radix(&hex[0..2], 16)?;
    let g = u8::from_str_radix(&hex[2..4], 16)?;
    let b = u8::from_str_radix(&hex[4..6], 16)?;
    Ok(Color::Rgb(r, g, b))
}

fn load_persisted_config() -> PersistedConfig {
    let path = config_path();
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return PersistedConfig::default(),
    };
    toml::from_str(&text).unwrap_or_default()
}

fn save_persisted_config(config: &PersistedConfig) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string(config)?;
    fs::write(path, text)?;
    Ok(())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn config_path() -> PathBuf {
    home_dir()
        .join(".config")
        .join("srtui")
        .join("config.toml")
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}
