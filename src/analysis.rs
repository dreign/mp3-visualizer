//! 分析模块：对应需求文档 3.3.1 的「专项计算」。
//!   - 采集：由 audio 模块按播放位置提供单声道 PCM 窗口（本篇不重复取数）。
//!   - 计算：FFT 频谱、时域波形、分频段能量包络、Onset 检测、鼓点事件、BPM 与节拍相位。
//!   - 输出：AnalysisFrame，供 GUI 层做「图形化呈现」。

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// FFT 窗口长度（采样点数）。
pub const FFT_SIZE: usize = 2048;
/// 频谱合并后的显示频段数。
pub const BANDS: usize = 256;
/// 波形显示点数。
pub const WAVE_POINTS: usize = 256;

/// 鼓点类型：对应低/中/高频段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeatKind {
    Kick,  // 底鼓，低频
    Snare, // 军鼓，中频
    Hihat, // 踩镲，高频
}

impl BeatKind {
    pub fn label(self) -> &'static str {
        match self {
            BeatKind::Kick => "KICK",
            BeatKind::Snare => "SNARE",
            BeatKind::Hihat => "HIHAT",
        }
    }
}

/// 一个鼓点事件（对应需求中的 BeatEvent）。
#[derive(Clone, Debug)]
pub struct BeatEvent {
    pub kind: BeatKind,
    pub strength: f32, // 0..1
    pub at: Instant,
}

/// 每帧分析结果，是「计算层 → 图形化层」的契约。
#[derive(Clone, Debug)]
pub struct AnalysisFrame {
    pub spectrum: Vec<f32>,        // 0..1，长度 BANDS，左右混合（供频谱/波形/节奏模式）
    pub spectrum_left: Vec<f32>,   // 左声道频谱（中位数阈值放大后，双盘左盘用）
    pub spectrum_right: Vec<f32>,  // 右声道频谱（中位数阈值放大后，双盘右盘用）
    pub waveform: Vec<f32>,        // -1..1，长度 WAVE_POINTS
    pub energy_left: f32,          // 左声道音量 0..1（左右电平表用）
    pub energy_right: f32,         // 右声道音量 0..1
    pub low: f32,                  // 混合低频能量 0..1
    pub mid: f32,
    pub high: f32,
    pub beat: Option<BeatEvent>, // 本帧新产生的鼓点（可能为空）
    pub pulse: f32,           // 视觉脉冲包络 0..1（检测到鼓点时置高，随后衰减）
    pub bpm: f32,
    pub beat_phase: f32,      // 0..1，当前处于节拍中的位置
}

/// 分析器。持有 FFT 计划与各频段的历史统计状态。
pub struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<Complex<f32>>,
    hann: Vec<f32>,
    spectrum: Vec<f32>,        // 混合频谱（平滑用）
    spectrum_left: Vec<f32>,   // 左声道频谱（平滑用，放大前的原始平滑值）
    spectrum_right: Vec<f32>,  // 右声道频谱（平滑用，放大前的原始平滑值）

    low_hist: VecDeque<f32>,
    mid_hist: VecDeque<f32>,
    high_hist: VecDeque<f32>,
    energy_hist_left: VecDeque<f32>,   // L 音量历史（中位数阈值放大用）
    energy_hist_right: VecDeque<f32>,  // R 音量历史
    prev_low: f32,
    prev_mid: f32,
    prev_high: f32,

    last_beat: [Option<Instant>; 3],
    kick_times: VecDeque<Instant>,
    bpm: f32,
    last_kick_at: Option<Instant>,

    sensitivity: f32, // 0..1，越高越灵敏（阈值越低）
    pulse: f32,
}

impl Analyzer {
    pub fn new() -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let hann: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (FFT_SIZE - 1) as f64).cos()) as f32)
            .collect();
        Self {
            fft,
            window: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            hann,
            spectrum: vec![0.0; BANDS],
            spectrum_left: vec![0.0; BANDS],
            spectrum_right: vec![0.0; BANDS],
            low_hist: VecDeque::with_capacity(60),
            mid_hist: VecDeque::with_capacity(60),
            high_hist: VecDeque::with_capacity(60),
            energy_hist_left: VecDeque::with_capacity(60),
            energy_hist_right: VecDeque::with_capacity(60),
            prev_low: 0.0,
            prev_mid: 0.0,
            prev_high: 0.0,
            last_beat: [None, None, None],
            kick_times: VecDeque::with_capacity(10),
            bpm: 0.0,
            last_kick_at: None,
            sensitivity: 0.5,
            pulse: 0.0,
        }
    }

    pub fn set_sensitivity(&mut self, s: f32) {
        self.sensitivity = s.clamp(0.0, 1.0);
    }

    pub fn sensitivity(&self) -> f32 {
        self.sensitivity
    }

    pub fn bpm(&self) -> f32 {
        self.bpm
    }

    fn band_range(&self, freq_lo: f32, freq_hi: f32, sample_rate: u32) -> (usize, usize) {
        let bin_hz = sample_rate as f32 / FFT_SIZE as f32;
        let lo = (freq_lo / bin_hz).round() as usize;
        let hi = (freq_hi / bin_hz).round() as usize;
        (lo.clamp(1, FFT_SIZE / 2 - 1), hi.clamp(lo + 1, FFT_SIZE / 2))
    }

    /// 在幅度谱上按浮点 bin 位置做线性插值。
    /// 对数分频的低频段宽不足 1 个 bin，若把边界 round 成整数 bin，会让十几个段
    /// 取到同一个 bin 的完全相同的值（柱子整段齐平）。插值后相邻段中心频率不同，
    /// 取到的幅值连续变化，柱子不再重复，同时不增加 FFT 点数。
    fn mag_at(mag: &[f32], pos: f32) -> f32 {
        if mag.is_empty() {
            return 0.0;
        }
        let pos = pos.max(0.0);
        let i = pos.floor() as usize;
        let frac = pos - i as f32;
        let a = mag.get(i).copied().unwrap_or(0.0);
        let b = mag.get(i + 1).copied().unwrap_or(a);
        a * (1.0 - frac) + b * frac
    }

    fn energy_of(mag: &[f32], range: (usize, usize)) -> f32 {
        let (lo, hi) = range;
        if hi <= lo {
            return 0.0;
        }
        mag[lo..hi].iter().sum::<f32>() / (hi - lo) as f32
    }

    /// 对单声道窗口做 FFT + 对数分频段归并 + 平滑，结果写入 out。
    /// 返回归一化的幅度谱 mag，供能量包络使用。
    /// 拆成关联函数是为了让调用方传入不相交的字段借用（&mut self.window 与 &mut self.spectrum_*）。
    #[allow(clippy::too_many_arguments)]
    fn compute_spectrum(
        fft: &Arc<dyn Fft<f32>>,
        window: &mut [Complex<f32>],
        hann: &[f32],
        samples: &[f32],
        sample_rate: u32,
        out: &mut [f32],
    ) -> Vec<f32> {
        for i in 0..FFT_SIZE {
            let v = samples.get(i).copied().unwrap_or(0.0) * hann[i];
            window[i] = Complex::new(v, 0.0);
        }
        fft.process(window);
        let mut mag = vec![0.0f32; FFT_SIZE / 2];
        for i in 0..FFT_SIZE / 2 {
            mag[i] = window[i].re.hypot(window[i].im) / (FFT_SIZE as f32 * 0.5);
        }

        // 对数分频段 + 浮点 bin 插值归并（低频段宽 < 1 bin，避免多段取同一整数 bin）。
        let fmax = (sample_rate as f32 / 2.0).min(16000.0);
        let bin_hz = sample_rate as f32 / FFT_SIZE as f32;
        for b in 0..BANDS {
            let f0 = 20.0 * (fmax / 20.0).powf(b as f32 / BANDS as f32);
            let f1 = 20.0 * (fmax / 20.0).powf((b + 1) as f32 / BANDS as f32);
            let lo = (f0 / bin_hz).max(1.0);
            let hi = (f1 / bin_hz).max(lo + 0.02);
            let steps = ((hi - lo) * 2.0).ceil().max(1.0) as usize;
            let mut peak = 0.0f32;
            for s in 0..=steps {
                let pos = lo + (hi - lo) * (s as f32 / steps as f32);
                let v = Self::mag_at(&mag, pos);
                if v > peak { peak = v; }
            }
            let target = (peak * 3.2).min(1.0);
            let cur = out[b];
            out[b] = if target > cur {
                cur * 0.35 + target * 0.65
            } else {
                cur * 0.82 + target * 0.18
            };
        }
        mag
    }

    /// 中位数阈值放大（参考 audioVisualizer/visualEffects.js 的双喇叭频谱）：
    /// 把各柱值降序排序取中位数作阈值，高于阈值的抬到 0.5~1、低于的压到 0~0.5，
    /// 再做 pow(0.92) 非线性（指数越接近 1 放大越弱）。效果是「保证至少一半柱子亮起来」，
    /// 但比 pow(0.85) 更温和，避免过度饱满；可通过该指数调节放大强度。
    fn median_amplify(values: &[f32]) -> Vec<f32> {
        let mut sorted: Vec<f32> = values.to_vec();
        sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        let mid = sorted.len() / 2;
        let threshold = sorted.get(mid).copied().unwrap_or(0.0).max(0.05);
        values
            .iter()
            .map(|&raw| {
                let v = if raw >= threshold {
                    0.5 + (raw - threshold) / (1.0 - threshold).max(1e-4) * 0.5
                } else {
                    raw / threshold * 0.5
                };
                v.powf(0.92).clamp(0.0, 1.0)
            })
            .collect()
    }

    /// 取音量历史的中位数（供左右共用的阈值使用）。
    fn energy_median(hist: &VecDeque<f32>) -> f32 {
        let mut sorted: Vec<f32> = hist.iter().copied().collect();
        sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        sorted.get(sorted.len() / 2).copied().unwrap_or(0.0)
    }

    /// 中位数阈值放大（把「保证至少一半亮起来」的思路用在 L/R 音量上）。
    /// 单帧只有一个标量，没有 256 根柱子可取中位数，因此用最近 N 帧的音量历史中位数当阈值：
    /// 高于阈值映射到 0.5~1，低于映射到 0~0.5，再 pow(0.92)（放大再减弱一点）。
    /// 关键：左右共用同一个阈值（取两侧中位数之和），这样响度大的一侧映射后仍然更高，
    /// 保留 L/R 差异；若各自用自己的中位数，两侧都会被归一化到 0.5 而丢失差异。
    fn amplify_energy(raw: f32, shared_threshold: f32) -> f32 {
        let threshold = shared_threshold.max(0.01);
        let v = if raw >= threshold {
            0.5 + (raw - threshold) / (1.0 - threshold).max(1e-4) * 0.5
        } else {
            raw / threshold * 0.5
        };
        v.powf(0.92).clamp(0.0, 1.0)
    }

    /// 计算单声道音量能量（对齐参考 audioVisualizer.js）：
    /// 参考实现用 Web Audio 的 getByteFrequencyData，其结果是 minDecibels=-90 ~ maxDecibels=-10
    /// 的 dB 映射后再除以 255。这里对线性幅度做同样的 dB 映射再取 0~20kHz 均值。
    fn channel_energy(mag: &[f32], sample_rate: u32, analyser_fft: usize) -> f32 {
        const MIN_DB: f32 = -90.0;
        const MAX_DB: f32 = -10.0;
        let bin_hz = sample_rate as f32 / analyser_fft as f32;
        let max_bin = (20000.0f32 / bin_hz).ceil() as usize;
        let effective = max_bin.min(mag.len()).max(1);
        let sum: f32 = mag[..effective]
            .iter()
            .map(|&m| {
                let db = 20.0 * m.max(1e-7).log10();
                ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0)
            })
            .sum();
        (sum / effective as f32 * 1.5).min(1.0)
    }

    /// 处理一帧左右声道 PCM 窗口，返回分析结果。
    /// `now` 由调用方提供（实时传 Instant::now()，离线验证可传虚拟时间），便于测试。
    /// 鼓点/波形/能量检测使用左右混合单声道，保持与单声道版本一致的检测结果。
    pub fn analyze(
        &mut self,
        samples_left: &[f32],
        samples_right: &[f32],
        sample_rate: u32,
        now: Instant,
    ) -> AnalysisFrame {
        // 左右混合单声道：用于鼓点检测（沿用既有算法，结果不变）。
        let mono: Vec<f32> = samples_left
            .iter()
            .zip(samples_right.iter())
            .map(|(l, r)| (l + r) * 0.5)
            .collect();

        // ---- 混合频谱（供频谱/波形/节奏模式）----
        let mag = Self::compute_spectrum(
            &self.fft, &mut self.window, &self.hann, &mono, sample_rate, &mut self.spectrum,
        );

        // ---- 左右声道频谱（各自 FFT）+ 音量能量 ----
        let mag_l = Self::compute_spectrum(
            &self.fft, &mut self.window, &self.hann, samples_left, sample_rate, &mut self.spectrum_left,
        );
        let mag_r = Self::compute_spectrum(
            &self.fft, &mut self.window, &self.hann, samples_right, sample_rate, &mut self.spectrum_right,
        );
        let raw_energy_left = Self::channel_energy(&mag_l, sample_rate, FFT_SIZE);
        let raw_energy_right = Self::channel_energy(&mag_r, sample_rate, FFT_SIZE);
        // 中位数阈值放大：左右共用阈值（两侧中位数之和），弱音下也跳起约一半格数且保留 L/R 差异。
        Self::push_hist(&mut self.energy_hist_left, raw_energy_left);
        Self::push_hist(&mut self.energy_hist_right, raw_energy_right);
        let shared = (Self::energy_median(&self.energy_hist_left) + Self::energy_median(&self.energy_hist_right)) * 0.5;
        let energy_left = Self::amplify_energy(raw_energy_left, shared);
        let energy_right = Self::amplify_energy(raw_energy_right, shared);
        // 放大后的左右频谱（双盘显示用），平滑值保留在 self.spectrum_* 中供下一帧继续平滑。
        let spectrum_left = Self::median_amplify(&self.spectrum_left);
        let spectrum_right = Self::median_amplify(&self.spectrum_right);

        // ---- 波形：降采样取峰值（用混合单声道）----
        let mut waveform = Vec::with_capacity(WAVE_POINTS);
        let step = (FFT_SIZE / WAVE_POINTS).max(1);
        for i in 0..WAVE_POINTS {
            let start = i * step;
            let end = (start + step).min(FFT_SIZE);
            let mut peak = 0.0f32;
            for j in start..end {
                let v = mono.get(j).copied().unwrap_or(0.0);
                if v.abs() > peak.abs() { peak = v; }
            }
            waveform.push(peak);
        }

        // ---- 分频段能量包络 ----
        let low = Self::energy_of(&mag, self.band_range(20.0, 160.0, sample_rate));
        let mid = Self::energy_of(&mag, self.band_range(160.0, 2000.0, sample_rate));
        let high = Self::energy_of(&mag, self.band_range(4000.0, 12000.0, sample_rate));
        let low_n = (low * 6.0).min(1.0);
        let mid_n = (mid * 8.0).min(1.0);
        let high_n = (high * 12.0).min(1.0);

        let mut beat: Option<BeatEvent> = None;

        // ---- Onset 检测：当前能量显著高于局部均值，且处于上升沿，并满足冷却间隔 ----
        // 灵敏度越高，阈值系数越接近 1（越容易触发）。
        let k = 1.55 - self.sensitivity * 0.5;
        let cooldowns = [Duration::from_millis(150), Duration::from_millis(130), Duration::from_millis(90)];

        let detect = |hist: &VecDeque<f32>, cur: f32, prev: f32, kind: BeatKind, idx: usize,
                      last: &[Option<Instant>; 3], out: &mut Option<BeatEvent>| {
            let n = hist.len();
            let avg = if n > 0 { hist.iter().sum::<f32>() / n as f32 } else { 0.0 };
            let threshold = avg * k + 0.0015;
            let ready = match last[idx] {
                Some(t) => now.duration_since(t) >= cooldowns[idx],
                None => true,
            };
            if ready && cur > threshold && cur > prev {
                let strength = ((cur - avg) / (avg + 0.01) * 0.9).clamp(0.2, 1.0);
                *out = Some(BeatEvent { kind, strength, at: now });
            }
        };

        detect(&self.low_hist, low, self.prev_low, BeatKind::Kick, 0, &self.last_beat, &mut beat);
        if beat.is_none() {
            detect(&self.mid_hist, mid, self.prev_mid, BeatKind::Snare, 1, &self.last_beat, &mut beat);
        }
        if beat.is_none() {
            detect(&self.high_hist, high, self.prev_high, BeatKind::Hihat, 2, &self.last_beat, &mut beat);
        }

        // 更新历史（保留约 1 秒窗口 @60fps）
        Self::push_hist(&mut self.low_hist, low);
        Self::push_hist(&mut self.mid_hist, mid);
        Self::push_hist(&mut self.high_hist, high);
        self.prev_low = low;
        self.prev_mid = mid;
        self.prev_high = high;

        // ---- 鼓点事件后处理：脉冲、BPM、节拍相位 ----
        if let Some(ev) = &beat {
            let idx = match ev.kind { BeatKind::Kick => 0, BeatKind::Snare => 1, BeatKind::Hihat => 2 };
            self.last_beat[idx] = Some(now);
            self.pulse = self.pulse.max(ev.strength);
            if ev.kind == BeatKind::Kick {
                self.last_kick_at = Some(now);
                self.kick_times.push_back(now);
                if self.kick_times.len() > 12 { self.kick_times.pop_front(); }
                self.update_bpm();
            }
        }
        // 脉冲指数衰减
        self.pulse *= 0.90;
        if self.pulse < 0.001 { self.pulse = 0.0; }

        let beat_phase = self.compute_phase(now);

        AnalysisFrame {
            spectrum: self.spectrum.clone(),
            spectrum_left,
            spectrum_right,
            waveform,
            energy_left,
            energy_right,
            low: low_n,
            mid: mid_n,
            high: high_n,
            beat,
            pulse: self.pulse,
            bpm: self.bpm,
            beat_phase,
        }
    }

    fn push_hist(h: &mut VecDeque<f32>, v: f32) {
        h.push_back(v);
        if h.len() > 60 { h.pop_front(); }
    }

    fn update_bpm(&mut self) {
        if self.kick_times.len() < 3 { return; }
        let times: Vec<Instant> = self.kick_times.iter().copied().collect();
        let mut intervals: Vec<f32> = Vec::new();
        for w in times.windows(2) {
            let d = w[1].duration_since(w[0]).as_secs_f32();
            // 过滤 40–200 BPM 之外的异常间隔（0.3s–1.5s）
            if d > 0.3 && d < 1.5 { intervals.push(d); }
        }
        if intervals.is_empty() { return; }
        intervals.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = intervals[intervals.len() / 2];
        let target = 60.0 / median;
        self.bpm = if self.bpm == 0.0 { target } else { self.bpm * 0.7 + target * 0.3 };
    }

    fn compute_phase(&self, now: Instant) -> f32 {
        if self.bpm <= 0.0 { return 0.0; }
        let Some(last) = self.last_kick_at else { return 0.0 };
        let beat_period = 60.0 / self.bpm;
        let elapsed = now.duration_since(last).as_secs_f32();
        if elapsed < 0.0 || elapsed > beat_period * 4.0 { return 0.0; }
        (elapsed / beat_period) % 1.0
    }
}
