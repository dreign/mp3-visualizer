//! 音频模块：解码 MP3 到 PCM，播放控制，并把 PCM 采样分流给可视化分析。
//!
//! 原型采用"一次性解码到内存"的方式：
//!   - 解码后的左右声道 f32 PCM 分别存入 Arc<Vec<f32>>，播放与可视化共用同一份数据。
//!   - rodio::Sink 以立体声（2 声道）播放，保留原始声像。
//!   - 可视化按 Sink 的播放位置从左右声道分别取窗口，做到声画同源、天然同步；
//!     单声道文件则左右两份相同，双盘显示一致。

use rodio::{Decoder, OutputStream, OutputStreamHandle, Sample, Sink, Source};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// 已加载的一首曲目（解码到内存后的形态）。
#[derive(Clone)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
    /// 左声道 f32 采样（单声道文件时与右声道相同）。
    pub samples_left: Arc<Vec<f32>>,
    /// 右声道 f32 采样（单声道文件时与左声道相同）。
    pub samples_right: Arc<Vec<f32>>,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration: Duration,
}

impl Track {
    pub fn duration_secs(&self) -> f32 {
        self.duration.as_secs_f32()
    }
    /// 是否为真正的立体声（左右声道有独立数据）。
    pub fn is_stereo(&self) -> bool {
        self.channels >= 2
    }
    /// 单声道视角的采样数（左右每声道帧数）。
    pub fn frames(&self) -> usize {
        self.samples_left.len()
    }
}

/// 解码文件到 Track，保留左右声道分离。
pub fn load_track(path: &Path) -> Result<Track, String> {
    let file = File::open(path).map_err(|e| format!("无法打开文件: {e}"))?;
    let reader = BufReader::new(file);
    let decoder = Decoder::new(reader).map_err(|e| format!("不支持的音频格式: {e}"))?;

    let sample_rate = decoder.sample_rate();
    let channels = decoder.channels();

    // 收集 f32 采样，按声道分流：ch0→左，ch1→右（更多声道忽略，单声道复制到两路）。
    // 用 rodio::Sample::to_f32 把 i16/u16/f32 统一归一到 [-1,1]。
    let mut left: Vec<f32> = Vec::new();
    let mut right: Vec<f32> = Vec::new();
    let mut channel_buf: Vec<f32> = Vec::with_capacity(channels as usize);
    let mut iter = decoder;
    loop {
        channel_buf.clear();
        for _ in 0..channels {
            match iter.next() {
                Some(v) => channel_buf.push(v.to_f32()),
                None => break,
            }
        }
        if channel_buf.len() < channels as usize {
            break;
        }
        left.push(channel_buf[0]);
        right.push(if channels >= 2 { channel_buf[1] } else { channel_buf[0] });
    }

    if left.is_empty() {
        return Err("文件中没有可解码的音频采样".to_string());
    }

    let duration = Duration::from_secs_f32(left.len() as f32 / sample_rate as f32);
    let title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("未知曲目")
        .to_string();

    Ok(Track {
        path: path.to_path_buf(),
        title,
        samples_left: Arc::new(left),
        samples_right: Arc::new(right),
        sample_rate,
        channels,
        duration,
    })
}

/// 把左右声道交织成 Sink 需要的交错立体声缓冲 [L,R,L,R,...]。
fn interleave(left: &[f32], right: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(left.len() * 2);
    for i in 0..left.len() {
        out.push(left[i]);
        out.push(right.get(i).copied().unwrap_or(left[i]));
    }
    out
}

/// 音频引擎：持有音频输出流与 Sink，管理播放状态。
pub struct AudioEngine {
    // 必须持有 _stream 保持音频设备输出存活。
    _stream: OutputStream,
    _handle: OutputStreamHandle,
    sink: Sink,
    current: Option<Track>,
    /// Seek 后 Sink 位置归零，用该偏移补偿，使 position() 仍返回全曲位置。
    position_offset: f32,
}

impl AudioEngine {
    pub fn new() -> Result<Self, String> {
        let (stream, handle) =
            OutputStream::try_default().map_err(|e| format!("无法打开音频设备: {e}"))?;
        let sink = Sink::try_new(&handle).map_err(|e| format!("无法创建播放器: {e}"))?;
        Ok(Self {
            _stream: stream,
            _handle: handle,
            sink,
            current: None,
            position_offset: 0.0,
        })
    }

    /// 装载并播放曲目（以交错立体声送给 Sink，保留声像）。
    pub fn play_track(&mut self, track: Track) {
        self.sink.clear();
        self.position_offset = 0.0;
        let data = interleave(&track.samples_left, &track.samples_right);
        let source = rodio::buffer::SamplesBuffer::new(2, track.sample_rate, data);
        self.sink.append(source);
        self.sink.play();
        self.current = Some(track);
    }

    pub fn play(&self) {
        self.sink.play();
    }

    pub fn pause(&self) {
        self.sink.pause();
    }

    pub fn toggle(&self) {
        if self.sink.is_paused() {
            self.sink.play();
        } else {
            self.sink.pause();
        }
    }

    pub fn set_volume(&self, v: f32) {
        self.sink.set_volume(v.clamp(0.0, 1.0));
    }

    pub fn volume(&self) -> f32 {
        self.sink.volume()
    }

    pub fn is_paused(&self) -> bool {
        self.sink.is_paused()
    }

    pub fn is_finished(&self) -> bool {
        self.sink.empty()
    }

    /// 当前播放位置（秒）。
    pub fn position(&self) -> f32 {
        self.position_offset + self.sink.get_pos().as_secs_f32()
    }

    /// Seek 到指定秒数（通过重建 Source 实现，原型够用）。
    pub fn seek(&mut self, secs: f32) {
        let Some(track) = self.current.clone() else { return };
        let sr = track.sample_rate;
        let secs = secs.max(0.0).min(track.duration_secs());
        let start = (secs * sr as f32) as usize;
        let start = start.min(track.frames().saturating_sub(1));
        let was_paused = self.sink.is_paused();

        self.sink.clear();
        let data = interleave(
            &track.samples_left[start..],
            &track.samples_right[start..],
        );
        let source = rodio::buffer::SamplesBuffer::new(2, sr, data);
        self.sink.append(source);
        self.position_offset = secs;
        if !was_paused {
            self.sink.play();
        } else {
            self.sink.pause();
        }
    }

    pub fn current_track(&self) -> Option<&Track> {
        self.current.as_ref()
    }

    /// 从当前播放位置附近取出左右声道分析窗口（长度固定）。
    /// 返回 (左窗口, 右窗口, 采样率)。单声道文件左右窗口内容相同。
    pub fn analysis_window(&self, window_len: usize) -> Option<(Vec<f32>, Vec<f32>, u32)> {
        let track = self.current.as_ref()?;
        let sr = track.sample_rate;
        let pos_sample = (self.position() * sr as f32) as usize;
        let start = pos_sample.saturating_sub(window_len);
        let end = (start + window_len).min(track.frames());
        let n = end.saturating_sub(start);

        let mut win_l = vec![0.0f32; window_len];
        let mut win_r = vec![0.0f32; window_len];
        if n > 0 {
            // 末尾对齐当前播放位置（与旧版一致），所以数据放在窗口尾部。
            win_l[window_len - n..].copy_from_slice(&track.samples_left[start..end]);
            win_r[window_len - n..].copy_from_slice(&track.samples_right[start..end]);
        }
        Some((win_l, win_r, sr))
    }
}
