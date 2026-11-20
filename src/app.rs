//! GUI 与可视化绘制（对应需求文档 3.3.1 的「图形化呈现」与 FR-3.x）。

use crate::analysis::{AnalysisFrame, Analyzer, BeatKind, BANDS, FFT_SIZE, WAVE_POINTS};
use crate::audio::{load_track, AudioEngine, Track};
use crate::lyrics::{load_lyrics, Lyrics};
use crate::particles::ParticleField;
use crate::recorder::{CaptureRegion, Recorder};
use crate::ribbon::RibbonField;
use crate::theme::{palette, ramp_color, ThemeId, Palette};
use crate::ui_style::{self, PanelSide, UiColors};
use eframe::egui;
use egui::{
    Align, Color32, ComboBox, Layout, Painter, Pos2, Rect, RichText, Sense, Shape, Slider,
    Stroke, Ui, Vec2,
};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

/// 录屏定位本程序窗口用的标题前缀（与 main.rs 的窗口标题一致）。
const WINDOW_TITLE_PREFIX: &str = "MP3 可视化播放器";

/// 可视化模式（FR-3.3）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VizMode {
    Dual,      // 双盘环绕频谱（参考界面，默认）
    Single,    // 单盘环绕频谱：居中一个大圆盘，左右声道混合
    NeonBars,  // 炫彩对称柱：中线上下镜像的霓虹柱 + 水平彩虹渐变
    ParticleFlow, // 粒子流：节奏跳动的炫彩粒子拖尾 + 随机几何锚定物，整体右行
    Lyrics,    // 歌词模式：动效区仅显示炫彩歌词文字，字号随律动与节奏变化
    Ribbon,    // 炫彩光带：一条随音波跳动的炫彩曲线 + 震动甩出的渐隐粒子 + 白色网格
    Spectrum,
    Waveform,
    Hybrid,
    Rhythm,
}
impl VizMode {
    const ALL: [VizMode; 10] = [
        VizMode::Dual,
        VizMode::Single,
        VizMode::NeonBars,
        VizMode::ParticleFlow,
        VizMode::Lyrics,
        VizMode::Ribbon,
        VizMode::Spectrum,
        VizMode::Waveform,
        VizMode::Hybrid,
        VizMode::Rhythm,
    ];
    fn label(self) -> &'static str {
        match self {
            VizMode::Dual => "双盘",
            VizMode::Single => "单盘",
            VizMode::NeonBars => "炫彩",
            VizMode::ParticleFlow => "粒子流",
            VizMode::Lyrics => "歌词",
            VizMode::Ribbon => "炫彩光带",
            VizMode::Spectrum => "频谱",
            VizMode::Waveform => "波形",
            VizMode::Hybrid => "混合",
            VizMode::Rhythm => "节奏",
        }
    }
    /// 验证开关：设置 MP3_VIZ_MODE=ParticleFlow 等枚举名可指定启动模式。
    fn from_env() -> Option<Self> {
        let name = std::env::var("MP3_VIZ_MODE").ok()?;
        Self::ALL
            .iter()
            .copied()
            .find(|m| format!("{m:?}").eq_ignore_ascii_case(&name))
    }
}

/// 鼓点视觉反馈的瞬时状态（供图形层做平滑衰减）。
#[derive(Default)]
struct BeatFx {
    kick: f32,
    snare: f32,
    hihat: f32,
    kind_text: String,
    kind_age: f32,
    clock: f32, // 累计时间，驱动中央喇叭震动的振荡相位
}

/// 后台加载线程回传给 UI 的结果：成功带曲目，失败带路径与原因。
type LoadResult = Result<Track, (PathBuf, String)>;

pub struct Mp3VizApp {
    engine: AudioEngine,
    analyzer: Analyzer,
    playlist: Vec<Track>,
    current_index: Option<usize>,

    mode: VizMode,
    theme: ThemeId,
    sensitivity: f32,
    volume: f32,

    last_frame: AnalysisFrame,
    beat_fx: BeatFx,
    /// 粒子流模式的状态（坐标归一化，切模式时保留现场）。
    particles: ParticleField,
    /// 炫彩光带模式的状态（波形平滑 + 震动粒子）。
    ribbon: RibbonField,
    status: String,
    show_beat_overlay: bool,

    // 微过渡状态（transitions.dev token）：模式/主题切换淡入、BPM 数字弹入。
    last_mode: VizMode,
    last_theme: ThemeId,
    switch_fade: f32,
    last_bpm: f32,
    bpm_pop: f32,

    // 歌词：LRC 直接解析，纯文本用音频能量包络自动对齐。
    lyrics: Option<Lyrics>,

    // 录屏：GDI 抓帧 + ffmpeg 后台编码；只录「动效+歌词」区域，带音轨。
    recorder: Recorder,
    record_msg: String,
    /// 每帧绘制时刷新的录制区域（相对窗口客户区的物理像素）。
    record_region: Option<CaptureRegion>,

    // 异步加载：文件对话框选完后立即返回，解码在后台线程进行；
    // 每帧从 rx 收取已完成结果，添加到列表，避免 UI 卡死。
    load_tx: Sender<LoadResult>,
    load_rx: Receiver<LoadResult>,
    loading_total: usize,
    loading_done: usize,
    load_failures: usize,

    /// 验证用：设置环境变量 MP3_RECORD_TEST_SECS 后自动录制指定秒数并关闭窗口，
    /// 不影响正常使用（未设置时为 None）。
    auto_record_secs: Option<f64>,
    auto_record_started: Option<Instant>,
}

impl Mp3VizApp {
    pub fn new() -> Result<Self, String> {
        let engine = AudioEngine::new()?;
        let analyzer = Analyzer::new();
        let blank = AnalysisFrame {
            spectrum: vec![0.0; BANDS],
            spectrum_left: vec![0.0; BANDS],
            spectrum_right: vec![0.0; BANDS],
            waveform: vec![0.0; 256],
            energy_left: 0.0,
            energy_right: 0.0,
            low: 0.0,
            mid: 0.0,
            high: 0.0,
            beat: None,
            pulse: 0.0,
            bpm: 0.0,
            beat_phase: 0.0,
        };
        let mut status = "请添加 MP3 文件开始".to_string();
        let mut playlist = Vec::new();
        let mut current_index = None;
        let (load_tx, load_rx) = mpsc::channel();

        // 验证开关：仅当显式设置环境变量时才启用自动录制测试。
        let auto_record_secs = std::env::var("MP3_RECORD_TEST_SECS")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|&s| s > 0.0);

        // 若存在随项目附带的 demo 音频，则自动加载并播放，便于直接体验鼓点可视化。
        let demo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/demo_beat.wav");
        let mut engine = engine;
        if demo.exists() {
            match load_track(&demo) {
                Ok(track) => {
                    playlist.push(track);
                    current_index = Some(0);
                    let track = playlist[0].clone();
                    engine.play_track(track);
                    engine.set_volume(0.8);
                    status = "已自动加载 120 BPM 鼓点演示曲目".to_string();
                }
                Err(e) => status = format!("演示曲目加载失败: {e}"),
            }
        }

        // 指定初始模式（验证用），默认仍为双盘。
        let initial_mode = VizMode::from_env().unwrap_or(VizMode::Dual);

        let mut app = Self {
            engine,
            analyzer,
            playlist,
            current_index,
            mode: initial_mode,
            theme: ThemeId::Rainbow,
            sensitivity: 0.5,
            volume: 0.8,
            last_frame: blank,
            beat_fx: BeatFx::default(),
            particles: ParticleField::new(),
            ribbon: RibbonField::new(),
            status,
            show_beat_overlay: true,
            last_mode: initial_mode,
            last_theme: ThemeId::Rainbow,
            switch_fade: 0.0,
            last_bpm: 0.0,
            bpm_pop: 0.0,
            lyrics: None,
            recorder: Recorder::default(),
            record_msg: String::new(),
            record_region: None,
            load_tx,
            load_rx,
            loading_total: 0,
            loading_done: 0,
            load_failures: 0,
            auto_record_secs,
            auto_record_started: None,
        };
        // demo 曲目也自动载入同目录歌词（若有）
        if let Some(track) = app.playlist.first().cloned() {
            app.try_sidecar_lyrics(&track);
        }
        Ok(app)
    }

    // ---------- 录屏控制 ----------
    fn toggle_record(&mut self) {
        if self.recorder.is_recording() {
            match self.recorder.stop() {
                Ok(path) => {
                    self.record_msg = format!("已保存 {}", path.display());
                    self.status = "录屏已保存".to_string();
                }
                Err(e) => {
                    self.record_msg = format!("录屏失败：{e}");
                    self.status = "录屏失败".to_string();
                }
            }
        } else {
            let Some(region) = self.record_region else {
                self.record_msg = "录制区域尚未就绪，请稍后再试".to_string();
                self.status = "录屏启动失败".to_string();
                return;
            };
            match self.recorder.start(WINDOW_TITLE_PREFIX, region) {
                Ok(()) => {
                    self.record_msg = "正在录制动效+歌词（含声音）…".to_string();
                    self.status = "录制中…".to_string();
                }
                Err(e) => {
                    self.record_msg = format!("无法开始录屏：{e}");
                    self.status = "录屏启动失败".to_string();
                }
            }
        }
    }

    // ---------- 播放列表操作（异步加载） ----------
    /// 把待加载路径交给后台线程池：主线程只做派发，解码不阻塞 UI。
    fn add_files(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() { return; }
        self.loading_total += paths.len();
        self.status = format!("正在加载 0/{} 首…", self.loading_total);

        // 共享任务队列与结果通道：worker 线程并发从队列取路径解码，
        // 完成的 Track 通过 channel 回传主线程统一入列（UI 线程独占 playlist）。
        let queue = Arc::new(Mutex::new(VecDeque::from(paths)));
        let workers = 4usize.min(self.loading_total.max(1));
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let tx = self.load_tx.clone();
            thread::spawn(move || loop {
                let path = {
                    let mut q = queue.lock().unwrap();
                    q.pop_front()
                };
                let Some(path) = path else { break };
                let result = load_track(&path).map_err(|e| (path.clone(), e));
                if tx.send(result).is_err() { break; } // 接收端已关闭
            });
        }
    }

    /// 每帧收取后台解码结果，增量更新播放列表与进度状态。
    fn poll_loads(&mut self) {
        let mut changed = false;
        while let Ok(result) = self.load_rx.try_recv() {
            changed = true;
            self.loading_done += 1;
            match result {
                Ok(track) => self.playlist.push(track),
                Err((path, e)) => {
                    self.load_failures += 1;
                    self.status = format!("加载失败 {}: {}", path.display(), e);
                }
            }
        }
        if changed && self.loading_done >= self.loading_total {
            let ok = self.loading_done - self.load_failures;
            if ok > 0 {
                if self.load_failures > 0 {
                    self.status = format!("已添加 {ok} 首曲目（{} 首失败）", self.load_failures);
                } else {
                    self.status = format!("已添加 {ok} 首曲目");
                }
            } else {
                self.status = format!("加载完成，{} 首均失败", self.load_failures);
            }
            // 自动播放第一首（若此前无曲目在播）。
            if self.current_index.is_none() && !self.playlist.is_empty() {
                self.play_index(0);
            }
            self.loading_total = 0;
            self.loading_done = 0;
            self.load_failures = 0;
        } else if changed {
            self.status = format!("正在加载 {}/{} 首…", self.loading_done, self.loading_total);
        }
    }

    fn pick_files(&mut self) {
        let files = rfd::FileDialog::new()
            .add_filter("音频文件", &["mp3", "wav", "flac", "ogg", "m4a"])
            .pick_files();
        if let Some(files) = files {
            self.add_files(files);
        }
    }

    fn pick_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            let mut paths = Vec::new();
            if let Ok(entries) = std::fs::read_dir(dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                        if matches!(ext.to_lowercase().as_str(), "mp3" | "wav" | "flac" | "ogg" | "m4a") {
                            paths.push(p);
                        }
                    }
                }
            }
            paths.sort();
            self.add_files(paths);
        }
    }

    fn play_index(&mut self, idx: usize) {
        if idx >= self.playlist.len() { return; }
        self.current_index = Some(idx);
        let track = self.playlist[idx].clone();
        self.engine.play_track(track.clone());
        self.engine.set_volume(self.volume);
        // 自动查找同目录同名 .lrc/.txt 歌词
        let has_lyrics = self.try_sidecar_lyrics(&track);
        self.status = if has_lyrics {
            format!("正在播放: {}（歌词已载入）", track.title)
        } else {
            format!("正在播放: {}", track.title)
        };
    }

    // ---------- 歌词 ----------
    /// 手动选择歌词文件（.lrc 带时间轴 / .txt 纯文本自动对齐）。
    fn pick_lyrics(&mut self) {
        let file = rfd::FileDialog::new()
            .add_filter("歌词文件", &["lrc", "txt"])
            .pick_file();
        if let Some(path) = file {
            self.load_lyrics_file(&path);
        }
    }

    fn load_lyrics_file(&mut self, path: &Path) {
        let (samples, sr) = match self.engine.current_track() {
            Some(t) => (t.samples_left.clone(), t.sample_rate),
            None => (std::sync::Arc::new(Vec::new()), 0),
        };
        match load_lyrics(path, &samples[..], sr) {
            Ok(l) => {
                let n = l.lines.len();
                let aligned = l.aligned;
                self.lyrics = Some(l);
                self.status = if aligned {
                    format!("已载入 {n} 行歌词（纯文本 · 能量包络自动对齐）")
                } else {
                    format!("已载入 {n} 行歌词（LRC 时间轴）")
                };
            }
            Err(e) => self.status = format!("歌词载入失败: {e}"),
        }
    }

    /// 播放时自动查找同目录同名歌词文件；返回是否载入成功。
    fn try_sidecar_lyrics(&mut self, track: &Track) -> bool {
        for ext in ["lrc", "txt"] {
            let p = track.path.with_extension(ext);
            if p.exists() {
                if let Ok(l) = load_lyrics(&p, &track.samples_left[..], track.sample_rate) {
                    self.lyrics = Some(l);
                    return true;
                }
            }
        }
        self.lyrics = None;
        false
    }

    fn next_track(&mut self) {
        let len = self.playlist.len();
        if len == 0 { return; }
        let next = match self.current_index {
            Some(i) => (i + 1) % len,
            None => 0,
        };
        self.play_index(next);
    }

    fn prev_track(&mut self) {
        let len = self.playlist.len();
        if len == 0 { return; }
        let prev = match self.current_index {
            Some(i) => (i + len - 1) % len,
            None => 0,
        };
        self.play_index(prev);
    }

    // ---------- 每帧更新分析 ----------
    fn update_analysis(&mut self, dt: f32) {
        self.beat_fx.clock += dt;
        let win = self.engine.analysis_window(FFT_SIZE);
        if let Some((samples_l, samples_r, sr)) = win {
            // 暂停时不推进鼓点检测，只保留衰减，避免误触发。
            let paused = self.engine.is_paused() || self.current_index.is_none();
            if !paused {
                self.last_frame = self.analyzer.analyze(&samples_l, &samples_r, sr, Instant::now());
                if let Some(beat) = &self.last_frame.beat {
                    match beat.kind {
                        BeatKind::Kick => self.beat_fx.kick = beat.strength,
                        BeatKind::Snare => self.beat_fx.snare = beat.strength,
                        BeatKind::Hihat => self.beat_fx.hihat = beat.strength,
                    }
                    self.beat_fx.kind_text = beat.kind.label().to_string();
                    self.beat_fx.kind_age = 1.0;
                }
            }
        }
        // 视觉消退
        let decay = (-dt * 4.0).exp() as f32;
        self.beat_fx.kick *= decay;
        self.beat_fx.snare *= decay;
        self.beat_fx.hihat *= decay;
        self.beat_fx.kind_age = (self.beat_fx.kind_age - dt * 1.2).max(0.0);

        // 粒子流：无论暂停与否都推进飘动；鼓点爆发/跳动仅在播放时触发。
        let playing = self.current_index.is_some() && !self.engine.is_paused();
        self.particles.update(dt, &self.last_frame, playing);
        self.ribbon.update(dt, &self.last_frame, playing);
    }
}

impl eframe::App for Mp3VizApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let dt = ctx.input(|i| i.stable_dt).min(0.1);
        let pal = palette(self.theme);
        let colors = ui_style::tokens(&pal);
        // 每帧同步语义 token：切主题时控件与画布一起换色。
        ui_style::apply(ctx, &pal);

        // 微过渡：控制条切换只触发一次；按 token 时长线性衰减，绘制端做缓动。
        if self.mode != self.last_mode || self.theme != self.last_theme {
            self.last_mode = self.mode;
            self.last_theme = self.theme;
            self.switch_fade = 1.0;
        }
        self.switch_fade = (self.switch_fade - dt / ui_style::DUR_FAST).max(0.0);
        if (self.last_frame.bpm - self.last_bpm).abs() > 0.5 {
            self.last_bpm = self.last_frame.bpm;
            self.bpm_pop = 1.0;
        }
        self.bpm_pop = (self.bpm_pop - dt / ui_style::DUR_QUICK).max(0.0);

        // 收取后台异步解码完成的曲目，增量更新播放列表。
        self.poll_loads();

        // 曲目自然结束后自动切下一首。
        if self.current_index.is_some() && self.engine.is_finished() && !self.engine.is_paused() {
            self.next_track();
        }

        self.update_analysis(dt);
        ctx.request_repaint(); // 保持动画刷新

        // 验证开关：等绘制出录制区域后自动开始，到时停止并关闭窗口。
        if let Some(secs) = self.auto_record_secs {
            if self.record_region.is_some() {
                match self.auto_record_started {
                    None => {
                        self.toggle_record();
                        eprintln!("[record-test] start: {}", self.record_msg);
                        self.auto_record_started = Some(Instant::now());
                    }
                    Some(t) if t.elapsed().as_secs_f64() >= secs => {
                        self.toggle_record();
                        eprintln!("[record-test] stop: {}", self.record_msg);
                        self.auto_record_secs = None;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    _ => {}
                }
            }
        }

        // 录屏：只抓「动效+歌词」区域；音轨按真实时间从当前曲目 PCM 取，带声音。
        if self.recorder.is_recording() {
            self.recorder.capture(WINDOW_TITLE_PREFIX);
            let idx = self.current_index;
            let track = idx.and_then(|i| self.playlist.get(i));
            let position = self.engine.position();
            let paused = self.engine.is_paused();
            let volume = self.engine.volume();
            self.recorder.feed_audio(track, position, paused, volume);
        }

        // ---------- 顶部菜单 ----------
        egui::TopBottomPanel::top("menu")
            .frame(ui_style::panel_frame(&colors, PanelSide::Top))
            .exact_height(48.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // 文件操作：主要动作用强调色边框，保持一组
                    if ui.button("＋ 文件").clicked() { self.pick_files(); }
                    if ui.button("＋ 文件夹").clicked() { self.pick_folder(); }
                    if ui.button("♫ 歌词").clicked() { self.pick_lyrics(); }

                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(6.0);

                    // 播放控制
                    if ui.button("⏮").clicked() { self.prev_track(); }
                    let play_label = if self.engine.is_paused() { "▶" } else { "⏸" };
                    if ui.button(play_label).clicked() { self.engine.toggle(); }
                    if ui.button("⏭").clicked() { self.next_track(); }

                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(6.0);

                    // 录屏按钮：录制中显示红点 + 计时，点击停止；否则点击开始。
                    let rec = self.recorder.status(&self.record_msg);
                    let rec_label = if rec.recording {
                        format!("■ {:.0}s", rec.elapsed.as_secs_f32())
                    } else {
                        "● 录屏".to_string()
                    };
                    let rec_color = if rec.recording {
                        Color32::from_rgb(255, 90, 90)
                    } else {
                        colors.foreground
                    };
                    if ui.button(RichText::new(rec_label).color(rec_color))
                        .on_hover_text("只录制中央的动效+歌词区域，并包含当前曲目声音")
                        .clicked()
                    {
                        self.toggle_record();
                    }
                    if !rec.recording && !self.record_msg.is_empty() {
                        ui.label(RichText::new(&self.record_msg).small().color(colors.muted));
                    }

                    // 右侧状态：状态徽标 + 弱化提示文本（不挤压左侧按钮）
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let playing = self.current_index.is_some() && !self.engine.is_paused();
                        let (state, state_color) = if playing {
                            ("● 播放中", colors.primary)
                        } else {
                            ("○ 已暂停", colors.muted)
                        };
                        ui.label(RichText::new(state).small().strong().color(state_color));
                        ui.add_space(10.0);
                        ui.label(RichText::new(&self.status).small().color(colors.muted));
                    });
                });
            });

        // ---------- 左侧播放列表 ----------
        egui::SidePanel::left("playlist")
            .frame(ui_style::panel_frame(&colors, PanelSide::Left))
            .resizable(true)
            .default_width(230.0)
            .min_width(160.0)
            .show(ctx, |ui| {
                // 标题 + 计数，形成清晰的列表头部层级
                ui.horizontal(|ui| {
                    ui.label(RichText::new("播放列表").strong().color(colors.foreground));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{} 首", self.playlist.len()))
                                .small().color(colors.muted),
                        );
                    });
                });
                ui.add_space(2.0);
                ui.separator();
                let mut action: Option<(usize, bool)> = None; // (index, is_selected_click)
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, track) in self.playlist.iter().enumerate() {
                        let selected = self.current_index == Some(i);
                        let text = format!("{}  {}", if selected { "▶" } else { "♫" }, track.title);
                        if ui.selectable_label(selected, text).clicked() {
                            action = Some((i, selected));
                        }
                        ui.label(
                            egui::RichText::new(format!("   {:.0} 秒", track.duration_secs()))
                                .small()
                                .weak(),
                        );
                    }
                });
                if let Some((i, selected)) = action {
                    if selected {
                        self.engine.toggle();
                    } else {
                        self.play_index(i);
                    }
                }
            });

        // ---------- 底部：控件行 + 进度条行 ----------
        // 拆成两个 panel（各只放一行）：单个 bottom panel 内连续两个 horizontal 会让
        // 第二行拿到 0 高度并被推到面板外（egui 0.29 的布局规划行为）。
        egui::TopBottomPanel::bottom("controls")
            .frame(egui::Frame {
                inner_margin: egui::Margin::symmetric(14.0, 6.0),
                ..ui_style::panel_frame(&colors, PanelSide::Bottom)
            })
            .exact_height(46.0)
            .show(ctx, |ui| {
                self.draw_controls(ui, &colors);
            });
        egui::TopBottomPanel::bottom("seek")
            .frame(egui::Frame {
                inner_margin: egui::Margin::symmetric(14.0, 6.0),
                rounding: egui::Rounding::ZERO,
                ..ui_style::panel_frame(&colors, PanelSide::Bottom)
            })
            .exact_height(48.0)
            .show(ctx, |ui| {
                self.draw_seek(ui, &colors);
            });

        // ---------- 中央可视化画布 ----------
        // 自定义 Frame：用 token 的 1px 边框，避免默认 panel 的亮色粗描边。
        let canvas_frame = egui::Frame {
            inner_margin: egui::Margin::ZERO,
            outer_margin: egui::Margin::ZERO,
            rounding: egui::Rounding::same(ui_style::RADIUS),
            shadow: egui::Shadow::NONE,
            fill: c(&pal.bg),
            stroke: Stroke::new(1.0, colors.border.gamma_multiply(0.5)),
        };
        egui::CentralPanel::default()
            .frame(canvas_frame)
            .show(ctx, |ui| {
                self.draw_visualization(ui);
            });
    }
}

impl Mp3VizApp {
    /// 进度条行：自绘轨道 + 填充 + 圆点，支持点击/拖拽跳转。
    fn draw_seek(&mut self, ui: &mut Ui, t: &UiColors) {
        // 宽度必须在 horizontal 之外算：horizontal 内部取 available_width 会把自身
        // 已占用的宽度也算进去，导致进度条过宽、整行 wrap。
        let bar_w = (ui.available_width() - 128.0).max(60.0);
        ui.horizontal_centered(|ui| {
            let (cur, total) = match self.engine.current_track() {
                Some(track) => (self.engine.position(), track.duration_secs()),
                None => (0.0, 1.0),
            };
            let time = format!("{:0>2}:{:0>2} / {:0>2}:{:0>2}",
                (cur / 60.0) as i32, (cur % 60.0) as i32,
                (total / 60.0) as i32, (total % 60.0) as i32);
            // 时间字加大提亮，和轨道同排对齐
            ui.label(RichText::new(time).monospace().size(15.0).color(t.foreground));
            ui.add_space(10.0);

            // 分配一个更高的条带：轨道 10px 高，圆点半径 8，hover 命中区 24px。
            let (bar, resp) = ui.allocate_exact_size(Vec2::new(bar_w, 24.0), Sense::click_and_drag());
            let p = ui.painter_at(bar);
            let cy = bar.center().y;
            let half_h = 5.0; // 轨道半高 -> 10px
            let rail = Rect::from_min_max(
                Pos2::new(bar.left(), cy - half_h),
                Pos2::new(bar.right(), cy + half_h),
            );
            // 未播放轨道：比面板底更深的凹槽 + 清楚描边
            p.rect_filled(rail, half_h, t.surface_alt.gamma_multiply(0.55));
            p.rect_stroke(rail, half_h, Stroke::new(1.0, t.border.gamma_multiply(0.8)));

            let frac = (cur / total.max(0.1)).clamp(0.0, 1.0);
            let knob_x = rail.left() + rail.width() * frac;
            if frac > 0.0 {
                // 已播放：主题主色（不压暗）+ 顶部高光渐变，对比鲜明
                let fill = Rect::from_min_max(rail.min, Pos2::new(knob_x, rail.max.y));
                p.rect_filled(fill, half_h, t.primary);
                let gloss = Rect::from_min_max(
                    Pos2::new(rail.left(), rail.top() + 1.0),
                    Pos2::new(knob_x, rail.top() + half_h * 0.7),
                );
                p.rect_filled(gloss, half_h * 0.5, Color32::from_white_alpha(70));
            }
            // 圆点：白色实心 + 主题色描边 + 深色细环，任何主题下都醒目
            let knob = Pos2::new(knob_x, cy);
            p.circle_filled(knob, 8.0, t.primary);
            p.circle_stroke(knob, 8.0, Stroke::new(2.0, Color32::WHITE));
            p.circle_filled(knob, 3.0, Color32::WHITE);

            // hover 时高亮整条轨道，提示可交互
            if resp.hovered() || resp.dragged() {
                p.rect_stroke(rail.expand(2.0), half_h + 2.0,
                    Stroke::new(1.5, t.primary.gamma_multiply(0.9)));
            }

            if resp.clicked() || resp.dragged() {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let f = ((pos.x - rail.left()) / rail.width()).clamp(0.0, 1.0);
                    self.engine.seek(f * total.max(0.1));
                }
            }
        });
    }

    fn draw_controls(&mut self, ui: &mut Ui, t: &UiColors) {
        // 控件分组，标签统一用 muted 小字，弱化"表单感"。
        ui.horizontal_centered(|ui| {
            ui.label(RichText::new("音量").small().color(t.muted));
            ui.add(Slider::new(&mut self.volume, 0.0..=1.0).show_value(false));
            self.engine.set_volume(self.volume);

            ui.add_space(14.0);
            ui.separator();
            ui.add_space(14.0);

            ui.label(RichText::new("模式").small().color(t.muted));
            ComboBox::from_id_salt("viz_mode")
                .selected_text(self.mode.label())
                .show_ui(ui, |ui| {
                    for m in VizMode::ALL {
                        ui.selectable_value(&mut self.mode, m, m.label());
                    }
                });

            ui.label(RichText::new("主题").small().color(t.muted));
            ComboBox::from_id_salt("theme")
                .selected_text(self.theme.label())
                .show_ui(ui, |ui| {
                    for theme in ThemeId::ALL {
                        ui.selectable_value(&mut self.theme, theme, theme.label());
                    }
                });

            ui.add_space(14.0);
            ui.separator();
            ui.add_space(14.0);

            ui.label(RichText::new("灵敏度").small().color(t.muted));
            ui.add(Slider::new(&mut self.sensitivity, 0.0..=1.0).show_value(false));
            self.analyzer.set_sensitivity(self.sensitivity);

            ui.checkbox(&mut self.show_beat_overlay, RichText::new("鼓点特效").small());
        });
    }

    fn draw_visualization(&mut self, ui: &mut Ui) {
        let pal = palette(self.theme);
        let available = ui.available_size();
        let (rect, _) = ui.allocate_exact_size(available, Sense::hover());
        let painter = ui.painter_at(rect);

        let t = ui_style::tokens(&pal);
        // 背景
        painter.rect_filled(rect, ui_style::RADIUS, c(&pal.bg));

        // 顶部信息行：标题 / 播放状态徽标 / BPM，1px 分隔线与面板体系一致。
        let title = self.engine.current_track()
            .map(|track| track.title.clone())
            .unwrap_or_else(|| "MP3 可视化播放器 · 原型".to_string());
        painter.text(
            rect.min + Vec2::new(18.0, 11.0),
            egui::Align2::LEFT_TOP,
            &title,
            egui::FontId::proportional(19.0),
            t.foreground,
        );

        let playing = self.current_index.is_some() && !self.engine.is_paused();
        let (state_text, state_color) = if playing {
            ("● 播放中", t.primary)
        } else {
            ("○ 已暂停", t.muted)
        };
        painter.text(
            rect.min + Vec2::new(18.0, 36.0),
            egui::Align2::LEFT_TOP,
            state_text,
            egui::FontId::proportional(11.0),
            state_color,
        );

        let bpm_text = if self.last_frame.bpm > 0.0 {
            format!("BPM {:.0}", self.last_frame.bpm)
        } else {
            "BPM --".to_string()
        };
        // 数字更新时轻微放大并提亮（ease_smooth_out，80–150ms 量级）。
        let bpm_pop = ui_style::ease_smooth_out(self.bpm_pop);
        painter.text(
            Pos2::new(rect.right() - 18.0, rect.top() + 9.0),
            egui::Align2::RIGHT_TOP,
            &bpm_text,
            egui::FontId::proportional(19.0 + 5.0 * bpm_pop),
            t.primary.gamma_multiply(0.9 + 0.7 * bpm_pop),
        );
        painter.line_segment(
            [
                Pos2::new(rect.left() + 18.0, rect.top() + 55.0),
                Pos2::new(rect.right() - 18.0, rect.top() + 55.0),
            ],
            Stroke::new(1.0, t.border.gamma_multiply(0.6)),
        );

        // 内容区（扣除顶部信息栏）。有歌词时纵向切成「上动效 / 下歌词」两个互不重叠的
        // 独立区域——歌词不再以半透明浮层压在动效上。
        let content = Rect::from_min_max(
            rect.min + Vec2::new(16.0, 62.0),
            rect.max - Vec2::new(16.0, 16.0),
        );
        if content.width() < 40.0 || content.height() < 40.0 { return; }

        // 录制区域 = 动效区 + 歌词区（即整个内容区），转成相对客户区的物理像素。
        // 每帧刷新，窗口缩放/DPI 变化后下次开始录制会用新尺寸。
        let ppp = ui.ctx().pixels_per_point();
        self.record_region = Some(CaptureRegion {
            x: (content.min.x * ppp).round() as i32,
            y: (content.min.y * ppp).round() as i32,
            w: (content.width() * ppp).round().max(16.0) as u32,
            h: (content.height() * ppp).round().max(16.0) as u32,
        });

        // 歌词模式独占整块内容区，不再切出下方歌词面板（动效区仅显示歌词文字）。
        let has_lyrics = self.mode != VizMode::Lyrics
            && self.lyrics.as_ref().map_or(false, |l| !l.lines.is_empty());
        let (viz_rect, lyrics_rect): (Rect, Option<Rect>) = if has_lyrics {
            let gap = 14.0;
            let lh = (content.height() * 0.26).clamp(92.0, 148.0);
            let viz = Rect::from_min_max(
                content.min,
                Pos2::new(content.max.x, content.max.y - lh - gap),
            );
            let lyr = Rect::from_min_max(
                Pos2::new(content.min.x, content.max.y - lh),
                content.max,
            );
            (viz, Some(lyr))
        } else {
            (content, None)
        };
        if viz_rect.width() < 40.0 || viz_rect.height() < 40.0 { return; }

        // 网格底线（双盘/单盘/炫彩/粒子流/光带自带背景，跳过普通网格）
        if !matches!(self.mode,
            VizMode::Dual | VizMode::Single | VizMode::NeonBars | VizMode::ParticleFlow
            | VizMode::Lyrics | VizMode::Ribbon)
        {
            draw_grid(&painter, viz_rect, &pal);
        }

        match self.mode {
            VizMode::Dual => draw_dual_decks(
                &painter,
                viz_rect,
                &self.last_frame,
                &self.beat_fx,
                &pal,
            ),
            VizMode::Single => draw_single_deck(
                &painter,
                viz_rect,
                &self.last_frame,
                &self.beat_fx,
                &pal,
            ),
            VizMode::Spectrum => draw_spectrum(&painter, viz_rect, &self.last_frame.spectrum, &pal),
            VizMode::Waveform => draw_waveform(&painter, viz_rect, &self.last_frame.waveform, &pal),
            VizMode::Hybrid => {
                draw_spectrum(&painter, viz_rect, &self.last_frame.spectrum, &pal);
                draw_waveform(&painter, viz_rect, &self.last_frame.waveform, &pal);
            }
            VizMode::Rhythm => {
                draw_radial(&painter, viz_rect, &self.last_frame.spectrum, &pal);
            }
            VizMode::NeonBars => draw_neon_bars(&painter, viz_rect, &self.last_frame, &pal),
            VizMode::ParticleFlow => {
                self.particles.draw(&painter, viz_rect, &pal, &self.last_frame);
            }
            VizMode::Ribbon => {
                self.ribbon.draw(&painter, viz_rect, &pal);
            }
            VizMode::Lyrics => {
                self.draw_lyrics_mode(&painter, viz_rect, ui.ctx(), &pal, playing);
            }
        }

        // 鼓点特效（所有模式叠加）—— 这是可视化的侧重点。
        // 粒子流模式自带节奏爆发/跳动与鼓点文字提示，跳过通用叠加避免重复。
        // 粒子流与歌词模式自带节奏表现，跳过通用叠加避免重复/干扰。
        if self.show_beat_overlay
            && self.mode != VizMode::ParticleFlow
            && self.mode != VizMode::Lyrics
            && self.mode != VizMode::Ribbon
        {
            match self.mode {
                VizMode::Dual => draw_dual_beat_fx(&painter, viz_rect, &self.last_frame, &self.beat_fx, &pal),
                VizMode::Single => draw_single_beat_fx(&painter, viz_rect, &self.last_frame, &self.beat_fx, &pal),
                _ => draw_beat_fx(&painter, viz_rect, &self.last_frame, &self.beat_fx, &pal),
            }
        }

        // 歌词独立面板：独占下半区，与动效区域完全分离，不再覆盖频谱。
        if let Some(lr) = lyrics_rect {
            self.draw_lyrics_panel(&painter, lr, &t);
        }

        if !playing && self.mode != VizMode::Lyrics {
            let hint_color = if self.mode == VizMode::Ribbon {
                Color32::from_gray(90) // 白底模式用深色提示
            } else {
                c(&pal.accent).gamma_multiply(0.6)
            };
            painter.text(
                viz_rect.center(),
                egui::Align2::CENTER_CENTER,
                if self.playlist.is_empty() { "添加音乐，让节奏点亮画面" } else { "已暂停" },
                egui::FontId::proportional(24.0),
                hint_color,
            );
        }

        // 模式/主题切换：主题色薄雾短暂淡出，避免生硬瞬切（DUR_FAST = 250ms）。
        if self.switch_fade > 0.001 {
            let alpha = self.switch_fade * self.switch_fade * 0.32;
            painter.rect_filled(rect, ui_style::RADIUS, t.primary.gamma_multiply(alpha));
        }
    }

    /// 歌词独立区域：占据自己的矩形，与动效区分隔；无边框，仅文字悬浮显示。
    fn draw_lyrics_panel(&self, p: &Painter, r: Rect, t: &UiColors) {
        let Some(lyr) = &self.lyrics else { return };
        if lyr.lines.is_empty() { return; }
        let Some(idx) = lyr.index_at(self.engine.position()) else { return };

        // 无边框、无底色：歌词文字直接悬浮在自己的区域里。
        let cx = r.center().x;
        let cy = r.center().y;

        // 上下文行（弱化）
        if idx > 0 {
            p.text(
                Pos2::new(cx, cy - 34.0),
                egui::Align2::CENTER_CENTER,
                &lyr.lines[idx - 1].text,
                egui::FontId::proportional(15.0),
                t.muted.gamma_multiply(0.85),
            );
        }
        if idx + 1 < lyr.lines.len() {
            p.text(
                Pos2::new(cx, cy + 34.0),
                egui::Align2::CENTER_CENTER,
                &lyr.lines[idx + 1].text,
                egui::FontId::proportional(15.0),
                t.muted.gamma_multiply(0.85),
            );
        }

        // 当前行（主题色高亮）
        p.text(
            Pos2::new(cx, cy),
            egui::Align2::CENTER_CENTER,
            &lyr.lines[idx].text,
            egui::FontId::proportional(24.0),
            t.primary,
        );

        // 纯文本自动对齐的标识
        if lyr.aligned {
            p.text(
                Pos2::new(r.right() - 10.0, r.top() + 8.0),
                egui::Align2::RIGHT_TOP,
                "自动对齐",
                egui::FontId::proportional(11.0),
                t.muted,
            );
        }
    }

    /// 歌词模式：动效区仅显示歌词文字。逐字取主题色带形成炫彩，
    /// 字号随鼓点脉冲、频段能量与节拍相位弹跳；无歌词时用几何图形占位。
    fn draw_lyrics_mode(
        &self,
        p: &Painter,
        r: Rect,
        ctx: &egui::Context,
        pal: &Palette,
        playing: bool,
    ) {
        let frame = &self.last_frame;
        let time = self.beat_fx.clock;
        let cx = r.center().x;
        let cy = r.center().y;

        let Some(lyr) = self.lyrics.as_ref().filter(|l| !l.lines.is_empty()) else {
            draw_geometry_placeholder(p, r, pal, frame, time, playing);
            return;
        };
        let Some(idx) = lyr.index_at(self.engine.position()) else {
            draw_geometry_placeholder(p, r, pal, frame, time, playing);
            return;
        };

        let base = (r.width() / 13.0).clamp(26.0, 68.0);
        let energy = (frame.low * 0.50 + frame.mid * 0.30 + frame.high * 0.20).clamp(0.0, 1.0);
        let beat_bounce = if playing { (1.0 - frame.beat_phase).powi(4) * 0.22 } else { 0.0 };
        // 只放大当前唱到的字：明显放大 + 鼓点额外弹跳，其余字保持固定字号。
        let focus_scale = 1.0 + frame.pulse * 0.42 + energy * 0.18 + beat_bounce;
        let hue = (time * 0.10 + idx as f32 * 0.11) % 1.0;
        let pos = self.engine.position();
        let line = &lyr.lines[idx];
        // 空格不算一个字，避免焦点落到空格时出现「没有字在跳」的空档。
        let char_count = line.text.chars().filter(|c| !c.is_whitespace()).count().max(1);
        // 按人声能量谷划分字边界，让焦点字切换贴合实际发音间隙。
        let focus_idx = lyr.vocal_focus_index(idx, pos, char_count);

        // 上下文行：同样炫彩但更弱更小，焦点留在当前行；不参与跳动。
        let gap = base * 1.15;
        if idx > 0 {
            draw_rainbow_text(
                p, ctx, Pos2::new(cx, cy - gap), &lyr.lines[idx - 1].text,
                base * 0.40, pal, hue + 0.12, 0.40, None, 1.0,
            );
        }
        if idx + 1 < lyr.lines.len() {
            draw_rainbow_text(
                p, ctx, Pos2::new(cx, cy + gap), &lyr.lines[idx + 1].text,
                base * 0.40, pal, hue + 0.62, 0.40, None, 1.0,
            );
        }

        // 当前行：基础字号固定，仅焦点字放大并带辉光。
        draw_rainbow_text(
            p, ctx, Pos2::new(cx, cy), &line.text, base, pal, hue, 1.0,
            Some(focus_idx), focus_scale,
        );
    }
}

// ---------- 绘图辅助函数 ----------

/// 逐字取主题色带绘制炫彩文字。焦点字（当前唱到的字）单独放大并带辉光，
/// 其余字使用固定槽位与字号，不会随焦点一起跳动。
fn draw_rainbow_text(
    p: &Painter,
    ctx: &egui::Context,
    center: Pos2,
    text: &str,
    size: f32,
    pal: &Palette,
    hue_offset: f32,
    alpha: f32,
    focus_idx: Option<usize>,
    focus_scale: f32,
) {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() { return; }
    // 焦点序号按可见字计数（跳过空格），与 draw_lyrics_mode 里的 char_count 一致。
    let visible: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter(|(_, ch)| !ch.is_whitespace())
        .map(|(i, _)| i)
        .collect();
    let focus_char = focus_idx.and_then(|f| visible.get(f).copied());
    let font = egui::FontId::proportional(size);
    let mut widths = Vec::with_capacity(chars.len());
    let mut total = 0.0f32;
    ctx.fonts(|fonts| {
        for ch in &chars {
            let job = egui::text::LayoutJob::simple_singleline(
                ch.to_string(), font.clone(), Color32::WHITE,
            );
            let w = fonts.layout_job(job).rect.width();
            widths.push(w);
            total += w;
        }
    });
    let mut x = center.x - total * 0.5;
    let n = chars.len() as f32;
    for (i, ch) in chars.iter().enumerate() {
        let hue = (hue_offset + i as f32 / n.max(1.0) * 0.55) % 1.0;
        let col = c(&ramp_color(pal, hue)).gamma_multiply(alpha);
        let slot = Pos2::new(x + widths[i] * 0.5, center.y);
        let is_focus = focus_char == Some(i);
        let s = if is_focus { size * focus_scale } else { size };
        if is_focus && focus_scale > 1.001 {
            // 三层柔光：外圈大面积低透明 + 中圈 + 靠近本体的亮边。
            p.text(
                slot,
                egui::Align2::CENTER_CENTER,
                &ch.to_string(),
                egui::FontId::proportional(s * 1.55),
                col.gamma_multiply(alpha * 0.14),
            );
            p.text(
                slot,
                egui::Align2::CENTER_CENTER,
                &ch.to_string(),
                egui::FontId::proportional(s * 1.30),
                col.gamma_multiply(alpha * 0.30),
            );
            p.text(
                slot,
                egui::Align2::CENTER_CENTER,
                &ch.to_string(),
                egui::FontId::proportional(s * 1.12),
                col.gamma_multiply(alpha * 0.50),
            );
        }
        p.text(
            slot,
            egui::Align2::CENTER_CENTER,
            &ch.to_string(),
            egui::FontId::proportional(s),
            col,
        );
        x += widths[i];
    }
}

/// 无歌词时的几何占位：5 个多边形随频段能量/节拍跳动，颜色取主题色带。
fn draw_geometry_placeholder(
    p: &Painter,
    r: Rect,
    pal: &Palette,
    frame: &AnalysisFrame,
    time: f32,
    playing: bool,
) {
    let energy = (frame.low * 0.50 + frame.mid * 0.30 + frame.high * 0.20).clamp(0.0, 1.0);
    let cx = r.center().x;
    let cy = r.center().y;
    let gap = (r.width() / 7.0).clamp(52.0, 150.0);
    let base = (r.height() * 0.11).clamp(16.0, 52.0);
    for i in 0..5usize {
        let phase = time * 1.8 + i as f32 * 0.85;
        let bounce = if playing {
            (1.0 - ((frame.beat_phase + i as f32 / 5.0) % 1.0)).powi(6) * 0.38
        } else {
            0.0
        };
        let size = base * (0.78 + energy * 0.32 + frame.pulse * 0.38 + bounce);
        let x = cx + (i as f32 - 2.0) * gap;
        let y = cy + phase.sin() * base * 0.32;
        let hue = (i as f32 / 5.0 + time * 0.06) % 1.0;
        let col = c(&ramp_color(pal, hue));
        let rot = time * (0.4 + i as f32 * 0.12) + i as f32;
        let glow = placeholder_poly(x, y, size * 1.28, i, rot);
        p.add(Shape::convex_polygon(glow, col.gamma_multiply(0.10), Stroke::NONE));
        let pts = placeholder_poly(x, y, size, i, rot);
        p.add(Shape::convex_polygon(pts.clone(), col.gamma_multiply(0.62), Stroke::NONE));
        let mut outline = pts.clone();
        outline.push(pts[0]);
        p.add(Shape::line(outline, Stroke::new(2.0, col.gamma_multiply(0.92))));
    }
}

/// 占位几何顶点：三角 / 方 / 五边形 / 六边形 / 菱形。
fn placeholder_poly(cx: f32, cy: f32, size: f32, kind: usize, rot: f32) -> Vec<Pos2> {
    let n = match kind { 0 => 3, 1 => 4, 2 => 5, 3 => 6, _ => 4 };
    let offset = if kind == 4 {
        std::f32::consts::FRAC_PI_4
    } else {
        -std::f32::consts::FRAC_PI_2
    };
    (0..n)
        .map(|i| {
            let a = rot + offset + i as f32 * std::f32::consts::TAU / n as f32;
            Pos2::new(cx + size * a.cos(), cy + size * a.sin())
        })
        .collect()
}

fn c(rgb: &[u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// 渐变取色：t in [0,1]，从当前色系的色带上取色（整体随主题色系变化）。
fn rainbow(t: f32, pal: &Palette) -> Color32 {
    c(&ramp_color(pal, t))
}

/// 双盘环绕频谱（参考界面）：左右两个对称的彩虹放射圆盘 + 中央连接波形 + 两侧电平表。
/// 单声道 PCM 下，左右盘使用同一频谱（右盘镜像），视觉上与立体声版本一致。
fn draw_dual_decks(p: &Painter, r: Rect, frame: &AnalysisFrame, fx: &BeatFx, pal: &Palette) {
    // 暗色同心圆背景（以画面中心为圆心）
    let center = r.center();
    let max_r = r.width().max(r.height()) * 0.6;
    for i in 1..9 {
        let rad = max_r * i as f32 / 9.0;
        p.circle_stroke(center, rad, Stroke::new(1.0, c(&pal.grid).gamma_multiply(0.22)));
    }
    // 斜向交叉的淡线，呼应参考图背景纹理
    for k in -4..=4 {
        let off = k as f32 * r.height() / 8.0;
        p.line_segment(
            [Pos2::new(r.left(), center.y + off), Pos2::new(r.right(), center.y + off * 0.4)],
            Stroke::new(0.5, c(&pal.grid).gamma_multiply(0.12)),
        );
    }

    // 两个圆盘的半径与圆心
    let deck_r = (r.width() * 0.23).min(r.height() * 0.42);
    let gap = deck_r * 1.18;
    let left_c = center - Vec2::new(gap, 0.0);
    let right_c = center + Vec2::new(gap, 0.0);

    // 低音驱动量：Kick 冲击（强、短）+ 低频能量（弱、持续），让喇叭既有踩点又随低音呼吸。
    let bass = (fx.kick + frame.low * 0.25).clamp(0.0, 1.4);
    // 绘制每个盘：左盘用左声道频谱、右盘用右声道频谱（中位数阈值放大后，整圈更饱满），
    // 各盘喇叭再叠加本声道的音量脉冲，双声道声像差异直接可见。
    let bass_l = (bass + frame.energy_left * 0.5).min(1.6);
    let bass_r = (bass + frame.energy_right * 0.5).min(1.6);
    draw_one_deck(p, left_c, deck_r, &frame.spectrum_left, frame.pulse, false, pal, bass_l, fx.clock);
    draw_one_deck(p, right_c, deck_r, &frame.spectrum_right, frame.pulse, true, pal, bass_r, fx.clock);

    // 中央连接波形（左右盘之间的正弦红蓝线）
    draw_deck_bridge(p, left_c, right_c, deck_r, &frame.waveform, frame.pulse, pal);

    // 两侧 L/R 电平柱：各用本声道音量能量（参考 L/R 分离逻辑）；
    // 柱体由正方形小块堆叠，块宽即块高。
    let vu_h = r.height() * 0.62;
    let vu_top = r.top() + (r.height() - vu_h) * 0.5;
    let cell = 14.0;
    draw_level_meter(p, Pos2::new(r.left() + 14.0, vu_top), cell, vu_h, frame.energy_left, pal, "L");
    draw_level_meter(p, Pos2::new(r.right() - 14.0 - cell, vu_top), cell, vu_h, frame.energy_right, pal, "R");
}

/// 单盘模式：居中一个大圆盘，左右声道频谱均值驱动放射条；背景与 L/R 电平沿用双盘。
fn draw_single_deck(p: &Painter, r: Rect, frame: &AnalysisFrame, fx: &BeatFx, pal: &Palette) {
    let center = r.center();
    // 暗色同心圆背景（以画面中心为圆心）
    let max_r = r.width().max(r.height()) * 0.6;
    for i in 1..9 {
        let rad = max_r * i as f32 / 9.0;
        p.circle_stroke(center, rad, Stroke::new(1.0, c(&pal.grid).gamma_multiply(0.22)));
    }
    for k in -4..=4 {
        let off = k as f32 * r.height() / 8.0;
        p.line_segment(
            [Pos2::new(r.left(), center.y + off), Pos2::new(r.right(), center.y + off * 0.4)],
            Stroke::new(0.5, c(&pal.grid).gamma_multiply(0.12)),
        );
    }

    // 单个圆盘半径：只放一个盘，可比双盘的单个盘更大，占满中间区域。
    let count = frame.spectrum_left.len().min(frame.spectrum_right.len()).min(BANDS);
    let deck_r = (r.width() * 0.36).min(r.height() * 0.44);

    // 左右声道频谱逐 bin 取均值，作为单盘放射条数据源（两者都已完成中位数放大）。
    let mut mixed = Vec::with_capacity(count);
    for i in 0..count {
        mixed.push((frame.spectrum_left[i] + frame.spectrum_right[i]) * 0.5);
    }

    // 低音驱动量：Kick + 低频能量 + 左右音量脉冲，驱动中央喇叭震动。
    let bass = (fx.kick + frame.low * 0.25 + (frame.energy_left + frame.energy_right) * 0.25)
        .clamp(0.0, 1.6);
    // 单盘居中，不镜像；用混合左右声道后的频谱。
    draw_one_deck(p, center, deck_r, &mixed, frame.pulse, false, pal, bass, fx.clock);

    // 两侧 L/R 电平柱：仍分别显示左右声道，保留立体声信息。
    let vu_h = r.height() * 0.62;
    let vu_top = r.top() + (r.height() - vu_h) * 0.5;
    let cell = 14.0;
    draw_level_meter(p, Pos2::new(r.left() + 14.0, vu_top), cell, vu_h, frame.energy_left, pal, "L");
    draw_level_meter(p, Pos2::new(r.right() - 14.0 - cell, vu_top), cell, vu_h, frame.energy_right, pal, "R");
}

/// 绘制单个放射圆盘。
fn draw_one_deck(
    p: &Painter,
    center: Pos2,
    deck_r: f32,
    spectrum: &[f32],
    pulse: f32,
    mirror: bool,
    pal: &Palette,
    bass: f32,
    clock: f32,
) {
    // 外圈淡底环
    p.circle_stroke(center, deck_r * 1.02, Stroke::new(1.0, c(&pal.grid).gamma_multiply(0.5)));
    p.circle_stroke(center, deck_r * 0.72, Stroke::new(1.0, c(&pal.grid).gamma_multiply(0.35)));

    let n = spectrum.len().min(BANDS);
    let inner = deck_r * 0.44;          // 放射条起始半径（唱片芯外缘）
    let max_len = deck_r * 0.56;        // 最大条长
    let bar_w = 2.4;

    for i in 0..n {
        let v = spectrum[i].clamp(0.0, 1.0);
        // 镜像右盘：角度反向，颜色保持同一彩虹序，形成左右对称。
        let t = if mirror { 1.0 - i as f32 / n as f32 } else { i as f32 / n as f32 };
        let angle = t * std::f64::consts::TAU as f32 - std::f32::consts::FRAC_PI_2;
        let (sin, a_cos) = angle.sin_cos();
        let len = 6.0 + v * max_len * (1.0 + pulse * 0.15);
        let p0 = Pos2::new(center.x + a_cos * inner, center.y + sin * inner);
        let p1 = Pos2::new(center.x + a_cos * (inner + len), center.y + sin * (inner + len));
        let col = rainbow(i as f32 / n as f32, pal);
        p.line_segment([p0, p1], Stroke::new(bar_w, col.gamma_multiply(0.75 + v * 0.25)));
    }

    // ---- 中央"喇叭"（唱片芯）：随低音鼓点震动 ----
    // bass = Kick 冲击 + 低频能量；clock 驱动高频抖动与波纹扩散，模拟振膜震动。
    let shake_x = bass * 2.4 * (clock * 51.0).sin() + bass * 1.0 * (clock * 33.0).cos();
    let shake_y = bass * 1.5 * (clock * 43.0 + 1.7).sin();
    let hub_c = center + Vec2::new(shake_x, shake_y);
    let core = deck_r * 0.40 * (1.0 + bass * 0.20); // 振膜随低音向外鼓起

    // 振膜波纹：从喇叭边缘一圈圈向外推出，随 Kick 增强（低音炮声波感）
    if bass > 0.02 {
        let phase = (clock * 3.2).fract();
        for k in 0..3 {
            let t = (phase + k as f32 / 3.0).fract();
            let rr = core * (1.02 + t * 0.95);
            let a = (bass * (1.0 - t) * 0.8).clamp(0.0, 1.0);
            p.circle_stroke(
                hub_c,
                rr,
                Stroke::new(1.0 + bass * 2.2, rainbow(0.86, pal).gamma_multiply(a)),
            );
        }
    }

    // 喇叭本体：外暗底 + 同心圈 + 中心振膜（同心圈用色带高端色，随主题变化）
    p.circle_filled(hub_c, core, Color32::from_rgb(18, 14, 34));
    for (frac, t) in [(0.95, 0.92), (0.78, 0.96), (0.60, 1.0)] {
        p.circle_stroke(
            hub_c,
            core * frac,
            Stroke::new(2.5 + bass * 2.0, rainbow(t, pal)),
        );
    }
    // 中心振膜：主题 high 色（经典彩虹下为品红），随低音鼓起、增亮
    let hub_col = c(&pal.high).gamma_multiply(0.9 + bass * 0.5);
    p.circle_filled(hub_c, core * (0.46 + bass * 0.10), hub_col);
    p.circle_filled(hub_c, core * 0.14, Color32::from_rgb(30, 12, 40));
}

/// 两个圆盘之间的连接波形（参考图中央的红蓝正弦线）。
fn draw_deck_bridge(
    p: &Painter,
    left_c: Pos2,
    right_c: Pos2,
    deck_r: f32,
    wave: &[f32],
    pulse: f32,
    pal: &Palette,
) {
    let y_mid = (left_c.y + right_c.y) * 0.5;
    let x0 = left_c.x + deck_r * 0.75;
    let x1 = right_c.x - deck_r * 0.75;
    let span = x1 - x0;
    if span <= 4.0 { return; }

    // 用波形数据绘制，采样点数等于波形长度
    let n = wave.len().min(WAVE_POINTS);
    let amp = deck_r * 0.16 * (1.0 + pulse * 0.5);
    let left_col = c(&pal.low);   // 左段青/红
    let right_col = c(&pal.high); // 右段品红
    let mut prev: Option<Pos2> = None;
    for i in 0..n {
        let frac = i as f32 / (n - 1) as f32;
        let x = x0 + span * frac;
        let w = wave[i].clamp(-1.0, 1.0);
        // 让波形在中间收束成一点（呼应参考图两个尖峰在中央相接）
        let envelope = (frac - 0.5).abs() * 2.0;
        let y = y_mid - w * amp * envelope;
        let cur = Pos2::new(x, y);
        if let Some(pp) = prev {
            let col = Color32::from_rgb(
                lerp(left_col.r(), right_col.r(), frac),
                lerp(left_col.g(), right_col.g(), frac),
                lerp(left_col.b(), right_col.b(), frac),
            );
            p.line_segment([pp, cur], Stroke::new(2.0, col.gamma_multiply(0.9)));
        }
        prev = Some(cur);
    }
}

/// 两侧 L/R 电平柱：由正方形小块自上而下堆叠而成，亮块数量随本声道能量变化。
fn draw_level_meter(p: &Painter, pos: Pos2, w: f32, h: f32, level: f32, pal: &Palette, label: &str) {
    let gap = 3.0;
    let cell = w; // 正方形小块：块高 = 块宽
    // 按可用高度算出能叠多少块，并整体垂直居中。
    let segs = (((h + gap) / (cell + gap)).floor() as usize).max(1);
    let total_h = segs as f32 * cell + (segs - 1) as f32 * gap;
    let y0 = pos.y + (h - total_h) * 0.5;
    let lit = (level.clamp(0.0, 1.0) * segs as f32).round() as usize;

    for i in 0..segs {
        // i=0 在最底部
        let y = y0 + total_h - (i as f32 + 1.0) * cell - i as f32 * gap;
        let rect = Rect::from_min_size(Pos2::new(pos.x, y), Vec2::splat(cell));
        let t = i as f32 / (segs - 1).max(1) as f32;
        // 色带避开最暗的两端，低段不会暗到和未点亮块分不清。
        let band = (1.0 - t).clamp(0.08, 0.9);
        let base = rainbow(band, pal); // 底紫红 → 中绿 → 顶青（随主题色系）
        let on = i < lit;
        let col = if on {
            // 越靠近当前峰值越亮，形成"跳动"的层次感。
            let head = if i + 1 == lit { 1.25 } else { 1.35 };
            base.gamma_multiply(head)
        } else {
            c(&pal.grid).gamma_multiply(0.3)
        };
        // 小块带一点圆角，未点亮的块留一层暗描边，像 LED 段位。
        p.rect_filled(rect, cell * 0.14, col);
        if !on {
            p.rect_stroke(rect, cell * 0.14, Stroke::new(1.0, c(&pal.grid).gamma_multiply(0.45)));
        }
    }
    // 顶部峰值块的辉光，让柱子看起来是"动态顶上去的"。
    if lit > 0 {
        let top_y = y0 + total_h - lit as f32 * cell - (lit as f32 - 1.0) * gap;
        let glow = Rect::from_min_size(
            Pos2::new(pos.x - 3.0, top_y - 3.0),
            Vec2::splat(cell + 6.0),
        );
        // 辉光颜色取顶部亮块自身的色带位置，保持与柱身同色。
        let t_top = (lit.saturating_sub(1)) as f32 / (segs - 1).max(1) as f32;
        let band_top = (1.0 - t_top).clamp(0.08, 0.9);
        p.rect_filled(glow, cell * 0.5, rainbow(band_top, pal).gamma_multiply(0.16));
    }
    p.text(
        Pos2::new(pos.x + w * 0.5, y0 + total_h + 6.0),
        egui::Align2::CENTER_TOP,
        label,
        egui::FontId::proportional(13.0),
        c(&pal.accent),
    );
}

/// 单盘模式的鼓点叠加：中央闪光 + 单盘外圈震动大环 + 底边军鼓线 + 鼓点类型。
fn draw_single_beat_fx(p: &Painter, r: Rect, frame: &AnalysisFrame, fx: &BeatFx, pal: &Palette) {
    let center = r.center();
    if frame.pulse > 0.02 {
        p.circle_filled(center, 6.0 + frame.pulse * 16.0, c(&pal.high).gamma_multiply(frame.pulse * 0.5));
    }
    if fx.kick > 0.01 {
        // 最外圈震动大圆环：取当前主题色带颜色，随主题切换整体换色。
        let deck_r = (r.width() * 0.36).min(r.height() * 0.44);
        let ring_col = rainbow(0.08, pal);
        let rad = deck_r * (1.04 + fx.kick * 0.10);
        p.circle_stroke(
            center,
            rad,
            Stroke::new(2.0 + fx.kick * 4.0, ring_col.gamma_multiply(0.35 + fx.kick * 0.75)),
        );
    }
    if fx.snare > 0.01 {
        p.rect_filled(
            Rect::from_min_max(
                Pos2::new(r.left() + 12.0, r.bottom() - 4.0),
                Pos2::new(r.right() - 12.0, r.bottom()),
            ),
            1.0,
            c(&pal.mid).gamma_multiply(fx.snare * 0.6),
        );
    }
    if fx.kind_age > 0.0 && !fx.kind_text.is_empty() {
        p.text(
            Pos2::new(center.x, r.bottom() - 22.0),
            egui::Align2::CENTER_BOTTOM,
            &format!("● {}", fx.kind_text),
            egui::FontId::proportional(14.0),
            c(&pal.accent).gamma_multiply(fx.kind_age),
        );
    }
}

/// 双盘模式的鼓点叠加：中央闪光 + 两盘同步脉冲环。
fn draw_dual_beat_fx(p: &Painter, r: Rect, frame: &AnalysisFrame, fx: &BeatFx, pal: &Palette) {
    let center = r.center();
    // 中央连接点的鼓点闪光
    if frame.pulse > 0.02 {
        p.circle_filled(center, 6.0 + frame.pulse * 16.0, c(&pal.high).gamma_multiply(frame.pulse * 0.5));
    }
    if fx.kick > 0.01 {
        let gap = (r.width() * 0.23).min(r.height() * 0.42) * 1.18;
        // 最外圈震动大圆环：取当前主题色带颜色，随主题切换整体换色；
        // 亮度随 Kick 冲击增强，保持踩点震动的视觉反馈。
        let ring_col = rainbow(0.08, pal);
        for cc in [center - Vec2::new(gap, 0.0), center + Vec2::new(gap, 0.0)] {
            let rad = (r.width() * 0.23).min(r.height() * 0.42) * (1.04 + fx.kick * 0.10);
            p.circle_stroke(
                cc,
                rad,
                Stroke::new(2.0 + fx.kick * 4.0, ring_col.gamma_multiply(0.35 + fx.kick * 0.75)),
            );
        }
    }
    if fx.snare > 0.01 {
        p.rect_filled(
            Rect::from_min_max(
                Pos2::new(r.left() + 12.0, r.bottom() - 4.0),
                Pos2::new(r.right() - 12.0, r.bottom()),
            ),
            1.0,
            c(&pal.mid).gamma_multiply(fx.snare * 0.6),
        );
    }
    if fx.kind_age > 0.0 && !fx.kind_text.is_empty() {
        p.text(
            Pos2::new(center.x, r.bottom() - 22.0),
            egui::Align2::CENTER_BOTTOM,
            &format!("● {}", fx.kind_text),
            egui::FontId::proportional(14.0),
            c(&pal.accent).gamma_multiply(fx.kind_age),
        );
    }
}

fn draw_grid(p: &Painter, r: Rect, pal: &Palette) {
    let grid = c(&pal.grid);
    for i in 1..6 {
        let y = r.top() + r.height() * i as f32 / 6.0;
        p.line_segment([Pos2::new(r.left(), y), Pos2::new(r.right(), y)], Stroke::new(1.0, grid.gamma_multiply(0.5)));
    }
    for i in 1..10 {
        let x = r.left() + r.width() * i as f32 / 10.0;
        p.line_segment([Pos2::new(x, r.top()), Pos2::new(x, r.bottom())], Stroke::new(1.0, grid.gamma_multiply(0.35)));
    }
}

fn band_color(t: f32, pal: &Palette) -> Color32 {
    // t: 0=低频, 1=高频，在低/中/高主题色之间插值。
    let (a, b, f) = if t < 0.5 {
        (&pal.low, &pal.mid, t * 2.0)
    } else {
        (&pal.mid, &pal.high, (t - 0.5) * 2.0)
    };
    Color32::from_rgb(
        lerp(a[0], b[0], f),
        lerp(a[1], b[1], f),
        lerp(a[2], b[2], f),
    )
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t) as u8
}

/// 炫彩对称频谱（参考图）：以画面水平中线为轴，上下镜像的细长圆角柱；
/// 颜色沿水平方向取主题色带，柱体带辉光，整体像一条流动的彩带。
fn draw_neon_bars(p: &Painter, r: Rect, frame: &AnalysisFrame, pal: &Palette) {
    let spec = &frame.spectrum;
    if spec.len() < 2 { return; }

    let mid_y = r.center().y;
    // 深色底 + 中线细亮线
    p.rect_filled(r, 0.0, c(&pal.bg));
    p.line_segment(
        [Pos2::new(r.left(), mid_y), Pos2::new(r.right(), mid_y)],
        Stroke::new(1.0, c(&pal.accent).gamma_multiply(0.35)),
    );

    let bars = 112usize;                       // 柱数（比 256 段稀疏，贴近参考图间距）
    let slot_w = r.width() / bars as f32;
    let bar_w = (slot_w * 0.58).max(1.5);      // 柱宽，留出间隙
    let max_h = r.height() * 0.44;             // 单侧最大高度
    let pulse = 1.0 + frame.pulse * 0.08;      // 鼓点时整体轻微扩张

    for i in 0..bars {
        let t = i as f32 / (bars - 1) as f32;
        // 从 256 段频谱线性插值取样，避免柱子出现台阶
        let src = t * (spec.len() - 1) as f32;
        let i0 = src.floor() as usize;
        let i1 = (i0 + 1).min(spec.len() - 1);
        let f = src - i0 as f32;
        let v = (spec[i0] * (1.0 - f) + spec[i1] * f).clamp(0.0, 1.0);

        let h = (2.0 + v * max_h * pulse).min(max_h * pulse);
        let x = r.left() + (i as f32 + 0.5) * slot_w;
        let col = rainbow(t, pal);

        // 1) 外发光：更宽、低透明度的一层，模拟霓虹辉光
        p.rect_filled(
            Rect::from_min_max(
                Pos2::new(x - bar_w, mid_y - h - 3.0),
                Pos2::new(x + bar_w, mid_y + h + 3.0),
            ),
            bar_w,
            col.gamma_multiply(0.16),
        );

        // 2) 主体：以中线为轴上下对称的圆角胶囊
        p.rect_filled(
            Rect::from_min_max(
                Pos2::new(x - bar_w * 0.5, mid_y - h),
                Pos2::new(x + bar_w * 0.5, mid_y + h),
            ),
            bar_w * 0.45,
            col.gamma_multiply(0.92),
        );

        // 3) 内芯提亮：窄条高光，增强彩色灯管质感
        p.rect_filled(
            Rect::from_min_max(
                Pos2::new(x - bar_w * 0.16, mid_y - h),
                Pos2::new(x + bar_w * 0.16, mid_y + h),
            ),
            bar_w * 0.16,
            col.gamma_multiply(1.15),
        );
    }
}

fn draw_spectrum(p: &Painter, r: Rect, spectrum: &[f32], pal: &Palette) {
    let n = spectrum.len().min(BANDS);
    let bar_w = r.width() / n as f32;
    for i in 0..n {
        let v = spectrum[i].clamp(0.0, 1.0);
        let h = v * r.height() * 0.92;
        if h < 1.0 { continue; }
        let x0 = r.left() + i as f32 * bar_w;
        let bar = Rect::from_min_max(
            Pos2::new(x0 + 1.0, r.bottom() - h),
            Pos2::new(x0 + bar_w - 2.0, r.bottom()),
        );
        let col = band_color(i as f32 / n as f32, pal).gamma_multiply(0.55 + v * 0.45);
        p.rect_filled(bar, 2.0, col);
        // 峰值高亮帽
        p.rect_filled(
            Rect::from_min_max(
                Pos2::new(x0 + 1.0, r.bottom() - h - 2.0),
                Pos2::new(x0 + bar_w - 2.0, r.bottom() - h),
            ),
            1.0,
            col.gamma_multiply(1.3),
        );
    }
}

fn draw_waveform(p: &Painter, r: Rect, wave: &[f32], pal: &Palette) {
    if wave.len() < 2 { return; }
    let mid = r.center().y;
    let col = c(&pal.accent).gamma_multiply(0.85);
    let n = wave.len();
    let mut pts = Vec::with_capacity(n);
    for (i, v) in wave.iter().enumerate() {
        let x = r.left() + i as f32 / (n - 1) as f32 * r.width();
        let y = mid - v.clamp(-1.0, 1.0) * r.height() * 0.45;
        pts.push(Pos2::new(x, y));
    }
    p.add(Shape::line(pts, Stroke::new(2.0, col)));
}

/// 放射状频谱（FR-3.10）。
fn draw_radial(p: &Painter, r: Rect, spectrum: &[f32], pal: &Palette) {
    let center = r.center();
    let radius = r.width().min(r.height()) * 0.18;
    let n = spectrum.len().min(BANDS);
    let max_len = r.width().min(r.height()) * 0.30;
    for i in 0..n {
        let v = spectrum[i].clamp(0.0, 1.0);
        let angle = i as f32 / n as f32 * std::f64::consts::TAU as f32 - std::f32::consts::FRAC_PI_2;
        let (sin, cos) = angle.sin_cos();
        let inner = Pos2::new(center.x + cos * radius, center.y + sin * radius);
        let outer = Pos2::new(center.x + cos * (radius + v * max_len), center.y + sin * (radius + v * max_len));
        let col = band_color(i as f32 / n as f32, pal).gamma_multiply(0.6 + v * 0.4);
        p.line_segment([inner, outer], Stroke::new(3.0, col));
    }
    // 中心圆
    p.circle_filled(center, radius * 0.5, c(&pal.grid));
}

/// 鼓点图形化：脉冲光晕 + 放射环 + 节拍指示 + 鼓点类型闪现（FR-3.8.x）。
fn draw_beat_fx(p: &Painter, r: Rect, frame: &AnalysisFrame, fx: &BeatFx, pal: &Palette) {
    let center = r.center();
    let base = r.width().min(r.height());

    // 底鼓：中心光环脉冲
    if fx.kick > 0.01 {
        let radius = base * (0.10 + fx.kick * 0.22);
        let col = band_color(0.0, pal);
        p.circle_stroke(center, radius, Stroke::new(3.0 + fx.kick * 5.0, col.gamma_multiply(fx.kick)));
        p.circle_filled(center, radius * 0.35, col.gamma_multiply(fx.kick * 0.35));
    }

    // 军鼓：外圈扩散环
    if fx.snare > 0.01 {
        let radius = base * (0.22 + (1.0 - fx.snare) * 0.18);
        let col = band_color(0.5, pal);
        p.circle_stroke(center, radius, Stroke::new(2.0 + fx.snare * 3.0, col.gamma_multiply(fx.snare * 0.8)));
    }

    // 踩镲：顶部闪光条
    if fx.hihat > 0.01 {
        let col = band_color(1.0, pal);
        let w = r.width() * fx.hihat * 0.6;
        let y = r.top() + 4.0;
        p.rect_filled(
            Rect::from_min_max(Pos2::new(center.x - w / 2.0, y), Pos2::new(center.x + w / 2.0, y + 3.0)),
            2.0,
            col.gamma_multiply(fx.hihat),
        );
    }

    // 全局脉冲亮度（随整体 pulse）
    if frame.pulse > 0.02 {
        p.rect_filled(r, 6.0, c(&pal.low).gamma_multiply(frame.pulse * 0.06));
    }

    // 节拍指示器：底部 4 个点，随 beat_phase 高亮（FR-3.9）
    let dot_y = r.bottom() - 16.0;
    let dot_r = 6.0;
    let gap = 26.0;
    let active = (frame.beat_phase * 4.0) as i32 % 4;
    for i in 0..4 {
        let x = center.x + (i as f32 - 1.5) * gap;
        let on = i == active && frame.bpm > 0.0;
        let col = c(&pal.accent).gamma_multiply(if on { 1.0 } else { 0.25 });
        p.circle_filled(Pos2::new(x, dot_y), if on { dot_r + 2.0 } else { dot_r }, col);
    }

    // 鼓点类型文字闪现
    if fx.kind_age > 0.0 && !fx.kind_text.is_empty() {
        let col = c(&pal.accent).gamma_multiply(fx.kind_age);
        p.text(
            Pos2::new(r.left() + 12.0, r.bottom() - 40.0),
            egui::Align2::LEFT_BOTTOM,
            &format!("● {}", fx.kind_text),
            egui::FontId::proportional(16.0),
            col,
        );
    }
}
