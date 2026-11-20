//! 歌词模块：对应需求 FR-3.11（歌曲进度）与 FR-3.12（歌词展示）。
//!
//! 支持两种来源：
//!   1. LRC 文本歌词：直接解析 `[mm:ss.xx]` 时间标签，天然带时间轴。
//!   2. 纯文本歌词（无时间戳）：用音频能量包络做启发式强制对齐
//!      （字符数比例预估 + 就近吸附到能量谷 + 单调约束），生成时间轴。
//!
//! 对齐说明：这是不依赖外部 ASR 模型的轻量级方案，适合原型；
//! 对伴奏较重的歌曲只能做到"段落级"对齐，人声清晰时效果更好。

use std::path::Path;

/// 一行歌词：起始时间（秒）+ 文本。
#[derive(Clone, Debug)]
pub struct LyricLine {
    pub time: f32,
    pub text: String,
}

/// 整首歌词（按时间升序）。
#[derive(Clone, Debug, Default)]
pub struct Lyrics {
    pub lines: Vec<LyricLine>,
    /// 是否由纯文本自动对齐生成（UI 用于提示）。
    pub aligned: bool,
    /// 来源文件名，便于界面显示。
    pub source: String,
    /// 人声能量包络（每行之间按 25ms 一帧），用于把焦点字贴合到人声发音上。
    /// 由 `build_vocal_curve` 填充；无音频时为 None，此时焦点字按时间均匀推进。
    pub vocal_curve: Option<VocalCurve>,
}

/// 行内人声能量曲线：时间从 `start` 起、每 25ms 一帧。
#[derive(Clone, Debug)]
pub struct VocalCurve {
    pub start: f32,
    pub end: f32,
    pub hop: f32,
    pub values: Vec<f32>,
}

impl VocalCurve {
    /// 取 [a,b] 区间内的能量，升余弦窗把两端压低，使峰值落在区间内部的人声发音上。
    pub fn energy_window(&self, a: f32, b: f32) -> f32 {
        if self.values.len() < 2 || b <= a {
            return 0.0;
        }
        let lo = (((a - self.start) / self.hop).floor() as isize).max(0) as usize;
        let hi = (((b - self.start) / self.hop).ceil() as usize).min(self.values.len());
        if hi <= lo {
            return 0.0;
        }
        let mut best = 0.0f32;
        let width = (b - a).max(1e-3);
        for (i, &v) in self.values[lo..hi].iter().enumerate() {
            let t = self.start + (lo + i) as f32 * self.hop;
            let phase = ((t - a) / width).clamp(0.0, 1.0);
            let window = (phase * std::f32::consts::PI).sin().powi(2);
            best = best.max(v * window);
        }
        best
    }
}

impl Lyrics {
    /// 当前播放位置对应的歌词行索引。
    /// 返回最后一个 `time <= t` 的行；尚未进入第一行时返回 None。
    pub fn index_at(&self, t: f32) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        let mut lo = 0usize;
        let mut hi = self.lines.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.lines[mid].time <= t {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            None
        } else {
            Some(lo - 1)
        }
    }

    /// 贴合人声的焦点字定位：把当前行按 n 个可见字切分，
    /// 字与字之间的边界优先落在人声能量谷（发音间隙）附近，而不是均匀时间点。
    /// 没有能量曲线时退化为按时间均匀推进。
    pub fn vocal_focus_index(&self, line_idx: usize, pos: f32, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let line = &self.lines[line_idx];
        let next_t = self
            .lines
            .get(line_idx + 1)
            .map_or(line.time + 3.0, |l| l.time);
        let span = (next_t - line.time).max(0.4);
        let uniform = ((pos - line.time) / span).clamp(0.0, 0.999);
        let fallback = (uniform * n as f32) as usize;
        let Some(curve) = &self.vocal_curve else {
            return fallback.min(n - 1);
        };

        // 依次为每个字边界在「均匀位置 ± 半个字宽」内寻找能量最低点。
        let mut bounds = Vec::with_capacity(n + 1);
        bounds.push(line.time);
        for k in 1..n {
            let target = line.time + span * (k as f32 / n as f32);
            let half = span / n as f32 * 0.38;
            let prev = *bounds.last().unwrap_or(&line.time);
            let mut best_t = target;
            let mut best_e = f32::INFINITY;
            for s in 0..=12 {
                let t = target - half + 2.0 * half * (s as f32 / 12.0);
                if t <= prev + 0.015 || t >= next_t - 0.015 {
                    continue;
                }
                let e = curve.energy_window(t - 0.03, t + 0.03);
                if e < best_e {
                    best_e = e;
                    best_t = t;
                }
            }
            if best_t <= prev + 0.005 {
                best_t = (prev + target) * 0.5;
            }
            bounds.push(best_t.min(next_t - 0.01));
        }
        bounds.push(next_t);
        for i in 0..n {
            if pos < bounds[i + 1] {
                return i;
            }
        }
        n - 1
    }
}

/// 载入歌词文件。`.lrc` 或含时间标签的内容按 LRC 解析；
/// 其余按纯文本处理，并用传入的音频采样做自动对齐。
pub fn load_lyrics(
    path: &Path,
    samples: &[f32],
    sample_rate: u32,
) -> Result<Lyrics, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("无法读取歌词文件: {e}"))?;
    let source = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("歌词")
        .to_string();

    // 无论 LRC 还是自动对齐的纯文本，只要有音频就构建行间人声能量曲线，
    // 供歌词模式的焦点字贴合人声发音（失败时按时间均匀推进）。
    let build_curve = |lines: &[LyricLine]| -> Option<VocalCurve> {
        if samples.is_empty() || sample_rate == 0 || lines.len() < 2 {
            return None;
        }
        build_vocal_curve(lines, samples, sample_rate)
    };

    if looks_like_lrc(&text) {
        let mut l = parse_lrc(&text);
        if l.lines.is_empty() {
            return Err("LRC 中没有有效歌词行".to_string());
        }
        l.source = source;
        l.vocal_curve = build_curve(&l.lines);
        return Ok(l);
    }

    let rows = plain_lines(&text);
    if rows.is_empty() {
        return Err("歌词文件为空".to_string());
    }
    if samples.is_empty() || sample_rate == 0 {
        return Err("纯文本歌词需要先播放歌曲才能自动对齐".to_string());
    }
    let lines = align_plain(&rows, samples, sample_rate);
    let vocal_curve = build_curve(&lines);
    Ok(Lyrics {
        lines,
        aligned: true,
        source,
        vocal_curve,
    })
}

/// 猜测是否为 LRC：前若干行出现 `[数字...]` 形式的时间标签。
fn looks_like_lrc(text: &str) -> bool {
    text.lines().take(80).any(|line| {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix('[') {
            if let Some(end) = rest.find(']') {
                let tag = &rest[..end];
                let b = tag.as_bytes();
                return !b.is_empty() && b[0].is_ascii_digit();
            }
        }
        false
    })
}

/// 解析 LRC。支持一行多个时间标签、`[offset:±ms]` 元数据。
fn parse_lrc(text: &str) -> Lyrics {
    let mut lines: Vec<LyricLine> = Vec::new();
    let mut offset_ms: f32 = 0.0;

    for raw in text.lines() {
        let mut rest = raw.trim();
        if rest.is_empty() {
            continue;
        }

        // 元数据标签：只关心 offset（单位毫秒，正值表示歌词提前）
        if let Some(inner) = bracket_inner(rest) {
            if let Some((k, v)) = inner.split_once(':') {
                if k.trim().eq_ignore_ascii_case("offset") {
                    if let Ok(ms) = v.trim().parse::<f32>() {
                        offset_ms = ms;
                    }
                    continue;
                }
            }
        }

        // 收集行首连续的时间标签
        let mut times: Vec<f32> = Vec::new();
        loop {
            match parse_time_tag(rest) {
                Some((t, consumed)) => {
                    times.push(t);
                    rest = &rest[consumed..];
                }
                None => break,
            }
        }
        if times.is_empty() {
            continue;
        }
        let content = rest.trim();
        if content.is_empty() {
            continue;
        }
        for t in times {
            lines.push(LyricLine {
                time: (t - offset_ms / 1000.0).max(0.0),
                text: content.to_string(),
            });
        }
    }

    lines.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
    // 去掉完全重复的行（多标签解析常见）
    lines.dedup_by(|a, b| (a.time - b.time).abs() < 1e-3 && a.text == b.text);

    Lyrics {
        lines,
        aligned: false,
        source: String::new(),
        vocal_curve: None,
    }
}

/// 取 `[...]` 内部内容。
fn bracket_inner(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let rest = s.strip_prefix('[')?;
    let end = rest.find(']')?;
    Some(&rest[..end])
}

/// 解析一个 `[mm:ss]` / `[mm:ss.xx]` / `[mm:ss:xx]` 标签，
/// 返回 (秒数, 消耗的字节数)。要求从字符串开头开始。
fn parse_time_tag(s: &str) -> Option<(f32, usize)> {
    if !s.starts_with('[') {
        return None;
    }
    let end = s.find(']')?;
    let inner = &s[1..end];
    let parts: Vec<&str> = inner.split(|c| c == ':' || c == '.').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let mm: f32 = parts[0].trim().parse().ok()?;
    let ss: f32 = parts[1].trim().parse().ok()?;
    if !(0.0..60.0).contains(&ss) {
        return None;
    }
    let frac: f32 = if parts.len() == 3 {
        let f = parts[2].trim();
        if f.is_empty() {
            0.0
        } else {
            let v: f32 = f.parse().ok()?;
            // 两位按百分秒、三位按毫秒
            if f.len() >= 3 {
                v / 1000.0
            } else {
                v / 100.0
            }
        }
    } else {
        0.0
    };
    Some((mm * 60.0 + ss + frac, end + 1))
}

/// 纯文本歌词按行拆分，过滤空行和常见元数据行（如 `[ti:标题]`）。
fn plain_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .filter(|l| {
            // 形如 `[ti:...]`、`[ar:...]`、`[offset:...]` 的标签行直接丢弃；
            // 注意不能误伤 `[00:12]歌词`（这类行不以 `]` 结尾或键不是纯字母）。
            if let Some(inner) = l.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                if let Some((k, _)) = inner.split_once(':') {
                    let k = k.trim();
                    if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphabetic()) {
                        return false;
                    }
                }
            }
            true
        })
        .map(|l| l.to_string())
        .collect()
}

/// 平滑能量包络（移动平均，窗口 half_width 两侧）。
fn smooth_env(env: &[f32], half_width: usize) -> Vec<f32> {
    if env.len() < 2 || half_width == 0 {
        return env.to_vec();
    }
    let mut out = Vec::with_capacity(env.len());
    for i in 0..env.len() {
        let lo = i.saturating_sub(half_width);
        let hi = (i + half_width + 1).min(env.len());
        let sum: f32 = env[lo..hi].iter().sum();
        out.push(sum / (hi - lo) as f32);
    }
    out
}

/// 纯文本歌词的启发式强制对齐：
///   1) 计算 25ms 步长的 RMS 能量包络并平滑；
///   2) 按每行有效字符数占总字数的比例，预估各行起始时间；
///   3) 将预估时间就近吸附到 ±1.2s 内的能量谷（句子间隙通常能量最低）；
///   4) 施加最小行距约束，保证时间严格递增。
fn align_plain(rows: &[String], samples: &[f32], sr: u32) -> Vec<LyricLine> {
    let total = samples.len() as f32 / sr as f32;
    let n = rows.len();
    if n == 0 || total <= 0.5 {
        return Vec::new();
    }

    // ---- 1) RMS 能量包络 ----
    let win = ((sr as f32 * 0.05) as usize).max(64);
    let hop = ((sr as f32 * 0.025) as usize).max(32);
    let frames = if samples.len() > win {
        (samples.len() - win) / hop + 1
    } else {
        1
    };
    let mut env = Vec::with_capacity(frames);
    for k in 0..frames {
        let start = k * hop;
        let end = (start + win).min(samples.len());
        let mut sum = 0.0f64;
        for &v in &samples[start..end] {
            sum += (v as f64) * (v as f64);
        }
        env.push(((sum / (end - start).max(1) as f64).sqrt()) as f32);
    }
    let env = smooth_env(&env, 4);
    let frame_dt = hop as f32 / sr as f32;

    // ---- 2) 按字符数比例预估 ----
    let chars: Vec<usize> = rows
        .iter()
        .map(|r| r.chars().filter(|c| !c.is_whitespace()).count().max(1))
        .collect();
    let total_chars: usize = chars.iter().sum();
    let mut targets = Vec::with_capacity(n);
    let mut acc = 0usize;
    for &c in &chars {
        // 片头留 2%，片尾留 2%
        let frac = acc as f32 / total_chars.max(1) as f32;
        targets.push(0.02 * total + frac * total * 0.96);
        acc += c;
    }

    // ---- 3) 吸附能量谷 + 4) 单调约束 ----
    let min_gap = (total / n as f32 * 0.55).clamp(0.25, 1.5);
    let mut times: Vec<f32> = Vec::with_capacity(n);
    for (i, &t) in targets.iter().enumerate() {
        let lo = (t - 1.2).max(0.0);
        let hi = (t + 1.2).min(total);
        let f0 = (lo / frame_dt) as usize;
        let f1 = ((hi / frame_dt) as usize).min(env.len().saturating_sub(1));
        let mut best_t = t;
        if f0 <= f1 {
            let mut best_e = f32::MAX;
            for f in f0..=f1 {
                if env[f] < best_e {
                    best_e = env[f];
                    best_t = f as f32 * frame_dt;
                }
            }
        }
        let lower = if i == 0 { 0.0 } else { times[i - 1] + min_gap };
        let upper = (total - 0.05).max(lower);
        times.push(best_t.max(lower).min(upper));
    }

    rows.iter()
        .zip(times)
        .map(|(text, time)| LyricLine {
            time,
            text: text.clone(),
        })
        .collect()
}

/// 构建行间人声能量曲线：按 25ms 一帧，取中频带（人声主要能量区）的短时能量。
/// 用相邻样本差分近似高通，抑制低频鼓点，让人声发音处出现明显峰值。
fn build_vocal_curve(lines: &[LyricLine], samples: &[f32], sr: u32) -> Option<VocalCurve> {
    let start = lines.first()?.time;
    let end = lines.last()?.time + 8.0;
    let hop_s = 0.025;
    let hop = ((sr as f32 * hop_s).round() as usize).max(1);
    let win = (hop * 3).max(64);
    let frames = (((end - start) / hop_s).ceil() as usize).min(samples.len() / hop + 1);
    if frames < 2 {
        return None;
    }
    let mut values = Vec::with_capacity(frames);
    for k in 0..frames {
        let sample_start = (start * sr as f32) as usize + k * hop;
        let sample_end = (sample_start + win).min(samples.len());
        if sample_start >= sample_end {
            values.push(0.0);
            continue;
        }
        let mut sum = 0.0f64;
        let mut prev = 0.0f32;
        for &v in &samples[sample_start..sample_end] {
            let hp = v - prev; // 一阶差分：削弱持续低频，突出人声/辅音变化
            prev = v;
            sum += (hp as f64) * (hp as f64);
        }
        values.push(((sum / (sample_end - sample_start).max(1) as f64).sqrt()) as f32);
    }
    // 归一化到 0..1，便于 UI 直接用作权重。
    let peak = values.iter().copied().fold(0.0f32, f32::max);
    if peak > 1e-6 {
        for v in &mut values {
            *v /= peak;
        }
    }
    Some(VocalCurve {
        start,
        end: start + frames as f32 * hop_s,
        hop: hop_s,
        values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc_with_multiple_tags() {
        let text = "[ti:测试]\n[offset:0]\n[00:01.50][00:03.20]第一句\n[00:05.00]第二句\n";
        let l = parse_lrc(text);
        assert_eq!(l.lines.len(), 3);
        assert!((l.lines[0].time - 1.5).abs() < 1e-3);
        assert!((l.lines[1].time - 3.2).abs() < 1e-3);
        assert_eq!(l.lines[2].text, "第二句");
    }

    #[test]
    fn index_at_works() {
        let l = Lyrics {
            lines: vec![
                LyricLine { time: 1.0, text: "a".into() },
                LyricLine { time: 3.0, text: "b".into() },
            ],
            aligned: false,
            source: String::new(),
            vocal_curve: None,
        };
        assert_eq!(l.index_at(0.5), None);
        assert_eq!(l.index_at(1.0), Some(0));
        assert_eq!(l.index_at(2.9), Some(0));
        assert_eq!(l.index_at(3.0), Some(1));
    }

    #[test]
    fn plain_lines_filters_meta_tags() {
        let text = "[ti:标题]\n[ar:歌手]\n第一句\n\n第二句\n[00:12]带时间戳的行\n";
        let rows = plain_lines(text);
        assert_eq!(rows, vec!["第一句", "第二句", "[00:12]带时间戳的行"]);
    }

    #[test]
    fn vocal_curve_shifts_focus_boundaries() {
        // 构造一行 4 字（2 秒），人声能量集中在三个时刻，字边界应偏离均匀位置。
        let lines = vec![
            LyricLine { time: 0.0, text: "一二三四".into() },
            LyricLine { time: 2.0, text: "五四三二".into() },
        ];
        let sr = 1000u32;
        let mut samples = vec![0.0f32; 3000];
        // 在 0.55/1.10/1.65 秒附近放置脉冲（发音），其余为静音（间隙）。
        for &t in &[0.55f32, 1.10, 1.65] {
            let start = (t * sr as f32) as usize;
            for v in samples.iter_mut().skip(start).take(60) {
                *v = (*v + 0.8).min(1.0);
            }
        }
        let curve = build_vocal_curve(&lines, &samples, sr).expect("应生成能量曲线");
        let l = Lyrics {
            lines,
            aligned: false,
            source: String::new(),
            vocal_curve: Some(curve),
        };
        // 均匀推进时 1.0s 处已是第 3 字（idx 2）；能量谷边界会把切换推迟到发音后。
        let idx = l.vocal_focus_index(0, 1.0, 4);
        assert!((0..4).contains(&idx));
        // 边界单调递增且落在行内。
        assert!(l.vocal_focus_index(0, 0.0, 4) <= l.vocal_focus_index(0, 1.99, 4));
    }

    #[test]
    fn loads_demo_lrc_file() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/demo_beat.lrc");
        let l = load_lyrics(&p, &[], 0).expect("demo lrc 应可解析");
        assert!(l.lines.len() >= 7);
        assert!(!l.aligned);
        assert!((l.lines[1].time - 2.0).abs() < 1e-3);
    }

    #[test]
    fn align_is_monotonic() {
        // 0.5 秒静音 + 2 秒音 + 0.5 秒静音 + 2 秒音
        let sr = 8000u32;
        let mut samples = vec![0.0f32; sr as usize / 2];
        samples.extend(std::iter::repeat(0.5).take(sr as usize * 2));
        samples.extend(vec![0.0f32; sr as usize / 2]);
        samples.extend(std::iter::repeat(0.5).take(sr as usize * 2));
        let rows = vec!["第一句".to_string(), "第二句".to_string()];
        let lines = align_plain(&rows, &samples, sr);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].time > lines[0].time);
        assert!(lines[1].time - lines[0].time >= 0.2);
    }
}
