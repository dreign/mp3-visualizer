//! 离线验证程序：不影响 GUI 原型。
//! 用固定时间间隔把 demo_beat.wav 喂给 Analyzer，统计鼓点事件与 BPM，
//! 用于确认「采集 → 计算」链路有效（无需打开图形界面）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

#[path = "../analysis.rs"]
mod analysis;
use analysis::{Analyzer, BeatKind, FFT_SIZE};

fn main() {
    // 直接读取单声道 WAV（16-bit PCM），避免依赖解码器。
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/demo_beat.wav");
    let bytes = std::fs::read(&path).expect("读 demo_beat.wav 失败，请先运行 python tools/make_demo.py");
    let samples = read_wav_stereo(&bytes).expect("WAV 解析失败");
    let sr = 44100u32;
    println!(
        "样本数: {}, 采样率: {}, 时长: {:.1}s (立体声)",
        samples.0.len(), sr, samples.0.len() as f32 / sr as f32
    );

    let mut analyzer = Analyzer::new();
    analyzer.set_sensitivity(0.5);

    let hop = 735; // ~60fps @44.1kHz
    let frame_dur = hop as f32 / sr as f32;
    let start = Instant::now();
    let mut kicks = 0;
    let mut snares = 0;
    let mut hihats = 0;

    let (samples_l, samples_r) = samples;
    let mut le = 0.0f32;
    let mut re = 0.0f32;
    let mut frames_n = 0usize;
    let mut t = 0usize;
    let mut dup_pairs = 0usize;
    let mut total_pairs = 0usize;
    while t + FFT_SIZE <= samples_l.len() {
        let win_l = &samples_l[t..t + FFT_SIZE];
        let win_r = &samples_r[t..t + FFT_SIZE];
        let now = start + Duration::from_secs_f32(t as f32 / sr as f32);
        let frame = analyzer.analyze(win_l, win_r, sr, now);
        le += frame.energy_left;
        re += frame.energy_right;
        frames_n += 1;
        // 统计相邻柱子完全等值的比例：验证插值是否消除了「整段齐平」。
        for w in frame.spectrum_left.windows(2) {
            total_pairs += 1;
            if (w[0] - w[1]).abs() < 1e-6 { dup_pairs += 1; }
        }
        if let Some(beat) = frame.beat {
            match beat.kind {
                BeatKind::Kick => kicks += 1,
                BeatKind::Snare => snares += 1,
                BeatKind::Hihat => hihats += 1,
            }
        }
        t += hop;
    }

    println!("---- 检测结果 ----");
    println!("Kick : {}", kicks);
    println!("Snare: {}", snares);
    println!("Hihat: {}", hihats);
    println!("估算 BPM: {:.1}", analyzer.bpm());
    println!("左右声道平均音量: L={:.3} R={:.3} (差异 {:.3})",
        le / frames_n as f32, re / frames_n as f32, (le - re).abs() / frames_n as f32);
    println!("相邻频谱柱完全同值比例: {:.1}%", dup_pairs as f32 / total_pairs as f32 * 100.0);
    // demo 为 120 BPM、16 秒，理论约 32 拍（Kick）。
    let ok = kicks >= 20 && (110.0..=130.0).contains(&analyzer.bpm());
    if ok {
        println!("✅ 验证通过：鼓点检测与 BPM 估算有效");
    } else {
        println!("⚠️  检测结果偏离预期（demo 理论 Kick≈32, BPM≈120），可调整灵敏度/阈值");
    }
    let _ = frame_dur;
}

/// 解析 WAV（16-bit PCM），返回 (左声道, 右声道)；单声道则复制到两路。
fn read_wav_stereo(bytes: &[u8]) -> Option<(Vec<f32>, Vec<f32>)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" { return None; }
    let mut pos = 12usize;
    let mut data: Option<&[u8]> = None;
    let mut channels = 1u16;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().ok()?) as usize;
        let chunk = &bytes[pos + 8..(pos + 8 + size).min(bytes.len())];
        if id == b"fmt " && chunk.len() >= 2 {
            channels = u16::from_le_bytes([chunk[2], chunk[3]]);
        }
        if id == b"data" { data = Some(chunk); break; }
        pos += 8 + size + (size & 1);
    }
    let data = data?;
    let ch = channels.max(1) as usize;
    let mut left = Vec::new();
    let mut right = Vec::new();
    for frame in data.chunks_exact(2 * ch) {
        let l = i16::from_le_bytes([frame[0], frame[1]]);
        let r = if ch >= 2 { i16::from_le_bytes([frame[2], frame[3]]) } else { l };
        left.push(l as f32 / 32768.0);
        right.push(r as f32 / 32768.0);
    }
    Some((left, right))
}
