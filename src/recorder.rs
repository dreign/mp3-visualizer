//! 录屏：只抓「动效 + 歌词」内容区（GDI BGRA），同时把当前播放曲目的 PCM
//! 作为音轨写入临时文件，停止后用 ffmpeg 合并为带声音的 H.264/AAC MP4。
//!
//! 设计要点：
//! - 录制区域由 UI 每帧提供（相对窗口客户区的物理像素），窗口移动时自动跟随；
//! - 抓帧节流到目标帧率；音轨按真实时间从已解码 PCM 取样本，与画面同源同步；
//! - 视频编码在独立后台线程完成，UI 线程只抓帧/写音频，不阻塞；
//! - 音轨采用「先写 s16le 原始文件、停止时合并」的方式，避开 Windows 下
//!   无法从父进程写入子进程 stdout（ffmpeg pipe:1）的限制。

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio::Track;

/// 目标录制帧率。抓帧按此节流，MP4 也按此设定时基。
pub const RECORD_FPS: u32 = 30;
/// 输出音轨采样率。源曲目采样率不同时按时间插值重采样。
pub const RECORD_SR: u32 = 48_000;

/// 录制会话状态，供 UI 显示。
#[derive(Clone, Debug)]
pub struct RecordStatus {
    pub recording: bool,
    pub elapsed: Duration,
    pub frames: u64,
    pub output: Option<PathBuf>,
    pub message: String,
}

impl Default for RecordStatus {
    fn default() -> Self {
        Self {
            recording: false,
            elapsed: Duration::ZERO,
            frames: 0,
            output: None,
            message: String::new(),
        }
    }
}

/// 录制区域：相对窗口客户区左上角的物理像素矩形 (x, y, w, h)。
#[derive(Clone, Copy, Debug)]
pub struct CaptureRegion {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

type Frame = Vec<u8>;

/// 录屏控制器。持有一条到编码线程的视频通道；None 表示未在录制。
pub struct Recorder {
    frame_tx: Option<Sender<Frame>>,
    done_rx: Option<Receiver<Result<(), String>>>,
    worker: Option<JoinHandle<()>>,
    /// 录制中持续写入的 s16le 音轨原始文件。
    audio_file: Option<File>,
    video_temp: Option<PathBuf>,
    audio_temp: Option<PathBuf>,
    started_at: Option<Instant>,
    last_capture: Option<Instant>,
    last_audio: Option<Instant>,
    /// 已写入音轨的源音频时间游标（秒）。音轨样本严格从此处连续推进，
    /// 不再每帧拿播放器的 position 当起点（position 与 wall-clock 会漂移，
    /// 导致相邻帧区间重叠/跳跃，产生沙沙声与咔哒）。
    audio_cursor: Option<f32>,
    region: Option<CaptureRegion>,
    frames: u64,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            frame_tx: None,
            done_rx: None,
            worker: None,
            audio_file: None,
            video_temp: None,
            audio_temp: None,
            started_at: None,
            last_capture: None,
            last_audio: None,
            audio_cursor: None,
            region: None,
            frames: 0,
        }
    }
}

impl Recorder {
    pub fn is_recording(&self) -> bool {
        self.frame_tx.is_some()
    }

    /// 输出目录：程序工作目录下的 recordings/。
    fn output_dir() -> PathBuf {
        let mut dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        dir.push("recordings");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// 开始录制。region 为「动效+歌词」区域相对客户区的物理像素矩形。
    pub fn start(&mut self, title_prefix: &str, region: CaptureRegion) -> Result<(), String> {
        if self.is_recording() {
            return Err("已在录制中".to_string());
        }

        let hwnd = find_window(title_prefix).ok_or_else(|| "未找到播放器窗口".to_string())?;
        let (cw, ch) = client_size(hwnd).ok_or_else(|| "无法读取窗口客户区尺寸".to_string())?;
        if region.w < 16 || region.h < 16
            || region.x < 0 || region.y < 0
            || region.x as u32 + region.w > cw
            || region.y as u32 + region.h > ch
        {
            return Err(format!(
                "录制区域异常：x={} y={} w={} h={}，客户区 {}x{}",
                region.x, region.y, region.w, region.h, cw, ch
            ));
        }

        let w = region.w;
        let h = region.h;
        let w_even = w - w % 2;
        let h_even = h - h % 2;

        let stamp = chrono_like_timestamp();
        let dir = Self::output_dir();
        let video_temp = dir.join(format!(".viz_{stamp}_video.mp4"));
        let audio_temp = dir.join(format!(".viz_{stamp}_audio.pcm"));
        let audio_file = File::create(&audio_temp)
            .map_err(|e| format!("无法创建音轨临时文件：{e}"))?;

        // 视频先单独编码成临时 MP4；停止后再与音轨合并。
        let mut child = Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error",
                "-f", "rawvideo",
                "-pix_fmt", "bgra",
                "-s", &format!("{w}x{h}"),
                "-r", &RECORD_FPS.to_string(),
                "-i", "pipe:0",
                "-vf", &format!("scale={w_even}:{h_even}"),
                "-c:v", "libx264",
                "-preset", "ultrafast",
                "-pix_fmt", "yuv420p",
                "-an",
                video_temp.to_string_lossy().as_ref(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("无法启动 ffmpeg（请确认已安装并在 PATH 中）：{e}"))?;

        let stdin: ChildStdin = child
            .stdin
            .take()
            .ok_or_else(|| "无法打开 ffmpeg 画面管道".to_string())?;

        let frame_bytes = (w * h * 4) as usize;
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>();
        let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();
        let worker = std::thread::spawn(move || {
            let result = encode_loop(child, stdin, frame_rx, frame_bytes);
            let _ = done_tx.send(result);
        });

        self.frame_tx = Some(frame_tx);
        self.done_rx = Some(done_rx);
        self.worker = Some(worker);
        self.audio_file = Some(audio_file);
        self.video_temp = Some(video_temp);
        self.audio_temp = Some(audio_temp);
        self.started_at = Some(Instant::now());
        self.last_capture = None;
        self.last_audio = None;
        self.audio_cursor = None; // 首帧用当时的 position 初始化
        self.region = Some(region);
        self.frames = 0;
        Ok(())
    }

    /// 尝试抓一帧并投递。按 RECORD_FPS 节流；抓帧失败静默跳过该帧。
    pub fn capture(&mut self, title_prefix: &str) {
        if !self.is_recording() { return; }
        let now = Instant::now();
        let interval = Duration::from_millis(1000 / RECORD_FPS as u64);
        if let Some(last) = self.last_capture {
            if now.duration_since(last) < interval { return; }
        }

        let hwnd = match find_window(title_prefix) {
            Some(h) => h,
            None => return,
        };
        let region = match self.region {
            Some(r) => r,
            None => return,
        };
        // 窗口缩放后区域可能越界，跳过该帧而不是抓到窗口外的屏幕内容。
        if let Some((cw, ch)) = client_size(hwnd) {
            if region.x as u32 + region.w > cw || region.y as u32 + region.h > ch {
                return;
            }
        }

        if let Some(frame) = grab_region_bgra(hwnd, region) {
            if let Some(tx) = &self.frame_tx {
                if tx.send(frame).is_ok() {
                    self.frames += 1;
                    self.last_capture = Some(now);
                }
            }
        }
    }

    /// 写入一段音轨。position 为播放器当前源音频位置（秒）。
    ///
    /// 关键：音轨样本严格由 `audio_cursor` 连续推进——每帧写入 `[cursor, target]`
    /// 的源音频，写完把游标设为 target。绝不拿 position 直接当采样起点再配合
    /// wall-clock 的样本数（那会让两个时钟漂移，导致帧边界波形重叠/跳跃→沙沙声）。
    /// 暂停时 position 冻结，按真实经过时间写静音推进音轨，保持与画面同步。
    pub fn feed_audio(&mut self, track: Option<&Track>, position: f32, paused: bool, volume: f32) {
        if !self.is_recording() { return; }
        let now = Instant::now();
        let elapsed = self
            .last_audio
            .map(|t| now.duration_since(t))
            .unwrap_or(Duration::ZERO);
        self.last_audio = Some(now);
        let wall_n = (elapsed.as_secs_f32() * RECORD_SR as f32).round() as usize;

        // 首帧：以当前播放位置为起点。
        let cursor = *self.audio_cursor.get_or_insert(position);

        let mut chunk: Vec<u8> = Vec::new();
        let mut next_cursor = cursor;

        match track {
            Some(t) if !paused && volume > 0.0 => {
                // 正常播放：目标 = 当前音频位置，写入 [cursor, position]。
                // 若 position 因暂停/跳转落后于游标，则以游标为准，不倒读音频。
                let target = position.max(cursor);
                let span = (target - cursor).min(1.0); // 卡顿保护：一次最多补 1 秒
                let n = (span * RECORD_SR as f32).round() as usize;
                if n > 0 {
                    let sr = t.sample_rate;
                    chunk.reserve_exact(n * 4);
                    for i in 0..n {
                        let src_t = cursor + i as f32 / RECORD_SR as f32;
                        let (l, r) = sample_stereo(&t.samples_left, &t.samples_right, src_t, sr);
                        push_i16(&mut chunk, l * volume);
                        push_i16(&mut chunk, r * volume);
                    }
                    next_cursor = cursor + n as f32 / RECORD_SR as f32;
                }
            }
            _ => {
                // 暂停 / 无曲目 / 静音：按真实时间写静音推进音轨，使音轨时长与画面一致。
                // cursor 保持不动（暂停没有消费源音频），恢复播放后立即从原处衔接。
                if wall_n > 0 {
                    chunk.resize(wall_n.min(RECORD_SR as usize) * 4, 0u8);
                }
            }
        }

        self.audio_cursor = Some(next_cursor);
        if !chunk.is_empty() {
            if let Some(f) = self.audio_file.as_mut() {
                let _ = f.write_all(&chunk);
            }
        }
    }

    /// 停止录制：关闭通道、等待视频编码，再合并音轨，返回最终输出路径。
    pub fn stop(&mut self) -> Result<PathBuf, String> {
        self.frame_tx.take();
        self.audio_file.take(); // 关闭音轨文件，写入的数据全部落盘
        let done_rx = self.done_rx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }

        let video_temp = self.video_temp.take();
        let audio_temp = self.audio_temp.take();
        self.started_at = None;
        self.last_capture = None;
        self.last_audio = None;
        self.audio_cursor = None;
        self.region = None;

        let video_result = match done_rx.and_then(|rx| rx.recv().ok()) {
            Some(r) => r,
            None => Err("编码线程未返回结果".to_string()),
        };
        if let Err(e) = video_result { return Err(e); }

        let (Some(video_temp), Some(audio_temp)) = (video_temp, audio_temp) else {
            return Err("录制临时文件丢失".to_string());
        };
        let out = Self::output_dir().join(format!(
            "viz_{}.mp4",
            out_stamp_from_temp(&video_temp)
        ));

        let merge_result = merge_audio_video(&video_temp, &audio_temp, &out);
        let _ = std::fs::remove_file(&video_temp);
        let _ = std::fs::remove_file(&audio_temp);
        merge_result
    }

    /// 当前状态快照。
    pub fn status(&self, last_message: &str) -> RecordStatus {
        RecordStatus {
            recording: self.is_recording(),
            elapsed: self.started_at.map(|t| t.elapsed()).unwrap_or_default(),
            frames: self.frames,
            output: None,
            message: last_message.to_string(),
        }
    }
}

/// 从临时视频文件名里取回时间戳，生成最终输出文件名。
fn out_stamp_from_temp(video_temp: &Path) -> String {
    let name = video_temp
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    name.strip_prefix(".viz_")
        .and_then(|s| s.strip_suffix("_video.mp4"))
        .unwrap_or("unknown")
        .to_string()
}

/// 合并无声视频与 s16le 音轨为带 AAC 声音的 MP4。
fn merge_audio_video(video: &Path, audio: &Path, out: &Path) -> Result<PathBuf, String> {
    let status = Command::new("ffmpeg")
        .args([
            "-y", "-hide_banner", "-loglevel", "error",
            "-i", video.to_string_lossy().as_ref(),
            "-f", "s16le",
            "-ar", &RECORD_SR.to_string(),
            "-ac", "2",
            "-i", audio.to_string_lossy().as_ref(),
            "-c:v", "copy",
            "-c:a", "aac",
            "-b:a", "128k",
            "-shortest",
            out.to_string_lossy().as_ref(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("合并音视频失败：{e}"))?;
    if !status.success() {
        return Err(format!("合并音视频失败（code {status}）"));
    }
    Ok(out.to_path_buf())
}

/// 从曲目 PCM 按时间插值取出左右声道样本（源采样率任意）。
fn sample_stereo(left: &[f32], right: &[f32], t: f32, sr: u32) -> (f32, f32) {
    if sr == 0 { return (0.0, 0.0); }
    let x = t * sr as f32;
    if x < 0.0 { return (0.0, 0.0); }
    let i = x as usize;
    let frac = x - i as f32;
    let l0 = left.get(i).copied().unwrap_or(0.0);
    let l1 = left.get(i + 1).copied().unwrap_or(l0);
    let r0 = right.get(i).copied().unwrap_or(0.0);
    let r1 = right.get(i + 1).copied().unwrap_or(r0);
    (l0 + (l1 - l0) * frac, r0 + (r1 - r0) * frac)
}

fn push_i16(out: &mut Vec<u8>, v: f32) {
    let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
    out.extend_from_slice(&s.to_le_bytes());
}

/// 编码线程：从通道收帧写入 ffmpeg stdin，结束后 close 并等待进程退出。
fn encode_loop(
    mut child: Child,
    mut stdin: ChildStdin,
    rx: Receiver<Frame>,
    expected_bytes: usize,
) -> Result<(), String> {
    let mut frames = 0u64;
    let mut write_err: Option<String> = None;
    while let Ok(frame) = rx.recv() {
        if frame.len() != expected_bytes {
            write_err = Some(format!("帧尺寸不符：{} != {expected_bytes}", frame.len()));
            break;
        }
        if let Err(e) = stdin.write_all(&frame) {
            write_err = Some(format!("写入 ffmpeg 失败：{e}"));
            break;
        }
        frames += 1;
    }
    drop(stdin);
    let status = child.wait().map_err(|e| format!("等待 ffmpeg 失败：{e}"))?;
    if let Some(e) = write_err {
        return Err(e);
    }
    if !status.success() {
        return Err(format!("ffmpeg 编码失败（code {status}）"));
    }
    if frames == 0 {
        return Err("没有捕获到任何帧".to_string());
    }
    Ok(())
}

/// 无 chrono 依赖的时间戳：YYYYMMDD_HHMMSS（本地时间，用系统命令获取）。
fn chrono_like_timestamp() -> String {
    if let Ok(out) = Command::new("cmd").args(["/C", "echo %DATE% %TIME%"]).output() {
        let s = String::from_utf8_lossy(&out.stdout);
        let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() >= 14 {
            return digits[..14].to_string();
        }
    }
    format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    )
}

// ---------------- Win32 GDI FFI（纯 std，无外部 crate）----------------

#[link(name = "user32")]
extern "system" {
    fn EnumWindows(lp_enum_proc: EnumProc, l_param: isize) -> i32;
    fn IsWindowVisible(h_wnd: isize) -> i32;
    fn GetWindowTextLengthW(h_wnd: isize) -> i32;
    fn GetWindowTextW(h_wnd: isize, lp_string: *mut u16, n_max_count: i32) -> i32;
    fn GetClientRect(h_wnd: isize, lp_rect: *mut Rect) -> i32;
    fn ClientToScreen(h_wnd: isize, lp_point: *mut Point) -> i32;
    fn GetDC(h_wnd: isize) -> isize;
    fn ReleaseDC(h_wnd: isize, h_dc: isize) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(h_dc: isize) -> isize;
    fn CreateCompatibleBitmap(h_dc: isize, cx: i32, cy: i32) -> isize;
    fn SelectObject(h_dc: isize, h_gdi_obj: isize) -> isize;
    fn BitBlt(h_dc: isize, x: i32, y: i32, cx: i32, cy: i32,
              h_src_dc: isize, x_src: i32, y_src: i32, rop: u32) -> i32;
    fn GetDIBits(h_dc: isize, h_bmp: isize, start: u32, c_lines: u32,
                 lp_bits: *mut u8, lp_bi: *mut BitmapInfo, usage: u32) -> i32;
    fn DeleteObject(h_object: isize) -> i32;
    fn DeleteDC(h_dc: isize) -> i32;
}

type EnumProc = extern "system" fn(isize, isize) -> i32;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rect { left: i32, top: i32, right: i32, bottom: i32 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Point { x: i32, y: i32 }

#[repr(C)]
#[derive(Clone, Copy)]
struct BitmapInfoHeader {
    size: u32, width: i32, height: i32, planes: u16, bit_count: u16,
    compression: u32, size_image: u32, x_ppm: i32, y_ppm: i32,
    clr_used: u32, clr_important: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 1],
}

/// 按标题前缀查找可见窗口，返回第一个匹配的句柄。
fn find_window(prefix: &str) -> Option<isize> {
    struct Ctx { prefix: String, found: isize }
    extern "system" fn cb_raw(hwnd: isize, lparam: isize) -> i32 {
        cb(hwnd, lparam)
    }
    extern "system" fn cb(hwnd: isize, lparam: isize) -> i32 {
        let ctx = unsafe { &mut *(lparam as *mut Ctx) };
        unsafe {
            if IsWindowVisible(hwnd) == 0 { return 1; }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 { return 1; }
            let mut buf = vec![0u16; len as usize + 1];
            GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            let title = String::from_utf16_lossy(&buf);
            if title.contains(&ctx.prefix) {
                ctx.found = hwnd;
                return 0;
            }
            1
        }
    }
    let mut ctx = Ctx { prefix: prefix.to_string(), found: 0 };
    unsafe { EnumWindows(cb_raw, &mut ctx as *mut Ctx as isize); }
    if ctx.found == 0 { None } else { Some(ctx.found) }
}

/// 读取窗口客户区物理像素尺寸。
fn client_size(hwnd: isize) -> Option<(u32, u32)> {
    let mut rect = Rect::default();
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0 { return None; }
    let w = (rect.right - rect.left).max(0) as u32;
    let h = (rect.bottom - rect.top).max(0) as u32;
    Some((w, h))
}

/// 抓取客户区内指定区域为 BGRA 像素（top-down DIB，第一行即图像顶部）。
fn grab_region_bgra(hwnd: isize, region: CaptureRegion) -> Option<Frame> {
    let mut pt = Point { x: 0, y: 0 };
    if unsafe { ClientToScreen(hwnd, &mut pt) } == 0 { return None; }
    let w = region.w as i32;
    let h = region.h as i32;
    let src_x = pt.x + region.x;
    let src_y = pt.y + region.y;

    unsafe {
        let screen_dc = GetDC(0);
        if screen_dc == 0 { return None; }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bmp = CreateCompatibleBitmap(screen_dc, w, h);
        let old = SelectObject(mem_dc, bmp);
        let ok = BitBlt(mem_dc, 0, 0, w, h, screen_dc, src_x, src_y, 0x00CC0020);
        let mut frame = vec![0u8; (w * h * 4) as usize];
        let mut bi = BitmapInfo {
            header: BitmapInfoHeader {
                size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                width: w,
                height: -h,
                planes: 1,
                bit_count: 32,
                compression: 0,
                size_image: 0,
                x_ppm: 0, y_ppm: 0, clr_used: 0, clr_important: 0,
            },
            colors: [0],
        };
        let got = if ok != 0 {
            GetDIBits(mem_dc, bmp, 0, h as u32, frame.as_mut_ptr(), &mut bi, 0)
        } else { 0 };

        SelectObject(mem_dc, old);
        DeleteObject(bmp);
        DeleteDC(mem_dc);
        ReleaseDC(0, screen_dc);

        if got == 0 { None } else { Some(frame) }
    }
}
