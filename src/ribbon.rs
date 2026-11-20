//! 炫彩光带模式（FR-3.15）：多条分频段的炫彩曲线，随音波与律动跳动，
//! 曲线震动时甩出粒子，粒子跟随震动方向飞行并逐渐渐隐；白色透视空间网格背景。
//!
//! 设计要点：
//! - 三条曲线分别绑定低/中/高频段，各取 AnalysisFrame.spectrum 对应区间的频谱驱动形状，
//!   自上而下按 高频 / 中频 / 低频 分层排列（低频厚重在下，高频轻盈在上）；
//! - 每条线独立用所属频段能量（low/mid/high）+ 鼓点脉冲 + 节拍相位调制振幅与跳动，
//!   低频跳幅大、高频跳幅小；频谱经一阶低通平滑，避免逐帧锯齿；
//! - 颜色沿各线取主题色带的不同区段（低=暖、中=中、高=冷）并随时间流动，切换色系整屏换色；
//! - 粒子按频段能量加权从对应曲线发射，继承该线颜色与震动方向，带轻微重力与阻尼，
//!   生命周期内透明度渐隐；归一化坐标 0..1，窗口缩放自动映射；
//! - 带透视纵深的白色空间网格由本模块自绘（覆盖深色底）：
//!   消失点在画布正中，纵线放射收敛、横线向消失点加密、远处淡近处浓，营造隧道式立体感。

use crate::analysis::AnalysisFrame;
use crate::theme::{ramp_color, Palette};
use eframe::egui::{pos2, Color32, Painter, Pos2, Rect, Stroke};

const BAND_COUNT: usize = 3;      // 分频段线条数：低 / 中 / 高
const LINE_POINTS: usize = 96;    // 每条线采样点数
const MAX_SPARKS: usize = 360;    // 粒子上限，防止长时间播放无限增长

// 各线在 spectrum(256) 中取的频段区间（bin 索引，频谱按对数频率分布）。
const BAND_RANGES: [(usize, usize); BAND_COUNT] = [(0, 44), (44, 148), (148, 256)];
// 各线基线在画布中的归一化 y（0 顶 1 底）：高频在上、中频居中、低频在下。
const BAND_BASE_Y: [f32; BAND_COUNT] = [0.70, 0.50, 0.30];
// 各线取主题色带的色段：低频暖(0~0.30)、中频中(0.32~0.66)、高频冷(0.68~1.0)。
const BAND_HUE: [(f32, f32); BAND_COUNT] = [(0.00, 0.30), (0.32, 0.66), (0.68, 1.00)];
// 各线跳动幅度系数：低频大、高频小。
const JUMP_SCALE: [f32; BAND_COUNT] = [1.00, 0.72, 0.46];
// 各线能量→振幅增益：低频厚重起伏大、高频轻盈起伏小。
const ENERGY_GAIN: [f32; BAND_COUNT] = [1.15, 1.00, 0.85];
// 各线共享波形成分的系数（有正有负，让三线错落起伏而非平行）：
// 低频跟随、中频反向、高频半幅跟随。
const WAVE_FACTOR: [f32; BAND_COUNT] = [1.00, -0.85, 0.70];

/// 一枚从曲线甩出的粒子。
struct Spark {
    x: f32,      // 画布归一化 0..1
    y: f32,      // 画布归一化 0..1
    vx: f32,     // 归一化/秒
    vy: f32,     // 归一化/秒
    hue: f32,    // 色带位置 0..1
    age: f32,    // 已存活秒数
    life: f32,   // 寿命秒数
    radius: f32, // 逻辑像素半径
}

/// 一条分频段曲线：平滑频谱 + 独立振幅/跳动/能量。
struct BandLine {
    smooth: Vec<f32>, // 平滑后的该频段频谱 0..1
    amplitude: f32,   // 当前振幅（相对画布高度比例）
    jump: f32,        // 随节拍的整体跳动（画布 y 向上为负）
    energy: f32,      // 当前频段能量 0..1
}

fn new_line() -> BandLine {
    BandLine {
        smooth: vec![0.0; LINE_POINTS],
        amplitude: 0.14,
        jump: 0.0,
        energy: 0.0,
    }
}

/// 炫彩光带状态。切模式时保留现场（与粒子流一致）。
pub struct RibbonField {
    bands: [BandLine; BAND_COUNT],
    wave_smooth: Vec<f32>, // 平滑后的共享波形 -1..1，叠加到各线制造起伏
    sparks: Vec<Spark>,
    rng: u64,
    time: f32, // 视觉时钟，驱动颜色流动
}

impl RibbonField {
    pub fn new() -> Self {
        Self {
            bands: [new_line(), new_line(), new_line()],
            wave_smooth: vec![0.0; LINE_POINTS],
            sparks: Vec::new(),
            rng: 0x9E37_79B9_7F4A_7C15,
            time: 0.0,
        }
    }

    /// xorshift64*，不引入 rand 依赖。
    fn rand_u64(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn rand(&mut self) -> f32 {
        ((self.rand_u64() >> 11) as f32) / ((1u64 << 53) as f32)
    }

    /// 某条线在 t 处的平滑频谱值（0..1），线性插值。
    fn sample(&self, band: usize, t: f32) -> f32 {
        let s = &self.bands[band].smooth;
        let x = t.clamp(0.0, 1.0) * (s.len() - 1) as f32;
        let i = x as usize;
        let j = (i + 1).min(s.len() - 1);
        let f = x - i as f32;
        s[i] * (1.0 - f) + s[j] * f
    }

    /// 共享波形在 t 处的平滑值（线性插值）。
    fn sample_wave(&self, t: f32) -> f32 {
        let x = t.clamp(0.0, 1.0) * (self.wave_smooth.len() - 1) as f32;
        let i = x as usize;
        let j = (i + 1).min(self.wave_smooth.len() - 1);
        let f = x - i as f32;
        self.wave_smooth[i] * (1.0 - f) + self.wave_smooth[j] * f
    }

    /// 某条线在 t 处的画布归一化 y（0 顶 1 底）：
    /// 基线 -（频谱起伏 + 共享波形起伏）- 节拍跳动。
    /// draw 与粒子发射共用，保证粒子正好从曲线上甩出。
    fn curve_y(&self, band: usize, t: f32) -> f32 {
        let line = &self.bands[band];
        // 频谱起伏：以 0.26 为中心，压缩异常大值。
        let v = self.sample(band, t);
        let spec = (v - 0.26).clamp(-0.26, 0.74);
        // 波形起伏：非线性放大让纯鼓点这类小波形也可见，各线系数有正有负 → 错落而非平行。
        let w = self.sample_wave(t);
        let wave = w.signum() * w.abs().powf(0.7) * WAVE_FACTOR[band];
        let offset = spec * 0.9 + wave * 0.55;
        (BAND_BASE_Y[band] - offset * line.amplitude * 2.0 - line.jump).clamp(0.04, 0.96)
    }

    /// 某条线在 t 处的色带位置（随时间流动，限定在该频段色段内）。
    fn hue_at(band: usize, t: f32, time: f32) -> f32 {
        let (lo, hi) = BAND_HUE[band];
        lo + ((t + time * 0.05) % 1.0) * (hi - lo)
    }

    pub fn update(&mut self, dt: f32, frame: &AnalysisFrame, playing: bool) {
        self.time += dt;

        // 频谱平滑向当前帧缓动，系数帧率无关。
        let k = (1.0 - (-dt * 16.0).exp()) as f32;
        let n = frame.spectrum.len().max(2);
        // 共享波形平滑（供三线叠加起伏，避免纯频谱驱动过于平直）。
        let nw = frame.waveform.len().max(2);
        for i in 0..self.wave_smooth.len() {
            let src = frame.waveform[(i * nw / self.wave_smooth.len()).min(nw - 1)];
            self.wave_smooth[i] += (src - self.wave_smooth[i]) * k;
        }
        let (lo_e, mid_e, hi_e) = (frame.low, frame.mid, frame.high);
        let energies = [lo_e, mid_e, hi_e];
        let pulse = frame.pulse;
        let beat_wave = (frame.beat_phase * std::f32::consts::TAU).sin();

        for b in 0..BAND_COUNT {
            let (bin_lo, bin_hi) = BAND_RANGES[b];
            let energy = energies[b] * ENERGY_GAIN[b];
            let line = &mut self.bands[b];
            line.energy = energy;
            line.amplitude = (0.07 + energy * 0.20 + pulse * 0.11).clamp(0.06, 0.32);
            // 该线节拍跳动：低频大、高频小，鼓点脉冲放大。
            let target_jump = beat_wave
                * (0.03 + pulse * 0.10 + energy * 0.04)
                * JUMP_SCALE[b];
            line.jump += (target_jump - line.jump) * (1.0 - (-dt * 11.0).exp()) as f32;
            // 从该频段频谱区间重采样到每条线的采样点。
            for i in 0..LINE_POINTS {
                let f = i as f32 / (LINE_POINTS - 1) as f32;
                let idx = (bin_lo as f32 + f * (bin_hi - bin_lo) as f32) as usize;
                let idx = idx.min(bin_hi.saturating_sub(1)).min(n - 1);
                let v = frame.spectrum[idx];
                line.smooth[i] += (v - line.smooth[i]) * k;
            }
        }

        // 粒子推进：轻微重力 + 阻尼，寿命到期或出界回收。
        for s in self.sparks.iter_mut() {
            s.x += s.vx * dt;
            s.y += s.vy * dt;
            s.vy += 0.16 * dt;
            s.vx *= (-dt * 1.6).exp() as f32;
            s.age += dt;
        }
        self.sparks.retain(|s| {
            s.age < s.life && s.x > -0.05 && s.x < 1.05 && s.y > -0.05 && s.y < 1.05
        });

        if !playing {
            return;
        }

        // 发射：哪条线所属频段响，就从哪条线发射更多粒子。
        let weights = [
            0.30 + lo_e * 2.4,
            0.30 + mid_e * 2.4,
            0.30 + hi_e * 2.4,
        ];
        let total_weight: f32 = weights.iter().sum();
        let energy = lo_e * 0.5 + mid_e * 0.3 + hi_e * 0.2;
        let rate = 90.0 + energy * 220.0 + pulse * 380.0;
        let mut budget = rate * dt;
        while budget >= 1.0 {
            budget -= 1.0;
            if self.sparks.len() >= MAX_SPARKS {
                break;
            }
            // 按频段能量加权选线。
            let mut r = self.rand() * total_weight;
            let mut band = BAND_COUNT - 1;
            for (i, w) in weights.iter().enumerate() {
                if r < *w {
                    band = i;
                    break;
                }
                r -= *w;
            }
            let t = self.rand();
            // 曲线在该点的画布 y，转成"向上为正"的相对量供发射用。
            let cy = self.curve_y(band, t);
            let y = cy;
            // 甩出方向：取曲线局部斜率 + 该线跳动方向，叠加随机散布。
            let slope = self.curve_y(band, (t + 0.012).min(1.0))
                - self.curve_y(band, (t - 0.012).max(0.0));
            let line_jump = self.bands[band].jump;
            let kick = 0.10 + energy * 0.26 + pulse * 0.5;
            let vy = (line_jump.signum() * 0.7 + slope * 2.4) * kick
                + (self.rand() - 0.5) * 0.36;
            let vx = (self.rand() - 0.5) * 0.22 + 0.03;
            // 先算完所有随机数，避免在 push 的结构体字面量里再次借用 self。
            let life = 0.6 + self.rand() * 1.1;
            let radius = 2.2 + self.rand() * 3.2 + pulse * 1.8;
            let hue = Self::hue_at(band, t, self.time);
            self.sparks.push(Spark {
                x: t,
                y,
                vx,
                vy,
                hue,
                age: 0.0,
                life,
                radius,
            });
        }
    }

    /// 带透视纵深的白色空间网格：消失点在画布正中（曲线中线附近）。
    /// - 纵线：从消失点放射到上/下边缘，按深度分段着色（远淡近浓）；
    /// - 横线：按 pow 规律向消失点加密（近大远小），同样随深度变色变粗；
    /// - 背景：中部最白、上下略灰的垂直渐变，强化空间纵深。
    fn draw_space_grid(p: &Painter, r: Rect) {
        // 渐变底：距中心越远越灰，模拟纵深光照。
        let rows = 48;
        for i in 0..rows {
            let t = i as f32 / rows as f32;
            let y0 = r.top() + r.height() * t;
            let y1 = r.top() + r.height() * (i as f32 + 1.0) / rows as f32 + 0.7;
            let d = (t - 0.5).abs() * 2.0; // 0(中心) .. 1(上下边缘)
            let g = (250.0 - d * 13.0) as u8;
            p.rect_filled(
                Rect::from_min_max(pos2(r.left(), y0), pos2(r.right(), y1)),
                0.0,
                Color32::from_gray(g),
            );
        }

        let cx = r.center().x;
        let cy = r.center().y;
        let w = r.width();
        let h = r.height();

        // 深度样式：t=0 远处(消失点)淡而细，t=1 近处(边缘)浓而粗。
        let depth_style = |t: f32| -> (u8, f32) {
            ((237.0 - 34.0 * t) as u8, 0.55 + 0.95 * t)
        };

        // 放射纵线：消失点 -> 上/下边缘等分点，每条分 16 段做深度渐变。
        let rays = 12;
        let segs = 16;
        for side in [-1.0f32, 1.0] {
            let edge_y = if side < 0.0 { r.top() } else { r.bottom() };
            for i in 0..=rays {
                let u = i as f32 / rays as f32;
                let ex = r.left() + u * w;
                // 主视线（正中）稍淡，两侧略浓，避免中心一团密集。
                let center_fade = (1.0 - (u - 0.5).abs() * 2.0) * 16.0;
                for s in 0..segs {
                    let t0 = s as f32 / segs as f32;
                    let t1 = (s + 1) as f32 / segs as f32;
                    let a = pos2(cx + (ex - cx) * t0, cy + (edge_y - cy) * t0);
                    let b = pos2(cx + (ex - cx) * t1, cy + (edge_y - cy) * t1);
                    let (g0, width) = depth_style(t1);
                    let g = (g0 as f32 + center_fade) as u8;
                    p.line_segment([a, b], Stroke::new(width, Color32::from_gray(g.min(252))));
                }
            }
        }

        // 透视横线：pow 规律向消失点加密；地板在下方、天花板在上方。
        let rings = 9;
        for side in [-1.0f32, 1.0] {
            for j in 1..=rings {
                let f = j as f32 / rings as f32;
                let persp = f.powf(2.1); // 近消失点密、近边缘疏
                let y = cy + side * h * 0.5 * persp;
                let (g, width) = depth_style(persp);
                p.line_segment(
                    [pos2(r.left(), y), pos2(r.right(), y)],
                    Stroke::new(width, Color32::from_gray(g)),
                );
            }
        }

        // 地平线（消失点所在水平线）稍强调，作为纵深分界。
        p.line_segment(
            [pos2(r.left(), cy), pos2(r.right(), cy)],
            Stroke::new(1.3, Color32::from_gray(209)),
        );
        p.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::from_gray(203)));
    }

    /// 绘制白色网格 + 三条分频段炫彩光带 + 渐隐粒子。
    pub fn draw(&self, p: &Painter, r: Rect, pal: &Palette) {
        // 带透视纵深的白色空间网格（自绘，覆盖深色画布底）。
        Self::draw_space_grid(p, r);

        let h = r.height();
        // 从上到下按 高频→中频→低频 绘制（低频最后画，压在上层更醒目）。
        for b in (0..BAND_COUNT).rev() {
            let line = &self.bands[b];
            let point = |i: usize| -> Pos2 {
                let t = i as f32 / (LINE_POINTS - 1) as f32;
                pos2(r.left() + t * r.width(), r.top() + self.curve_y(b, t) * h)
            };
            // 线宽：低频最粗、高频最细，呼应频段质感。
            let main_w = 3.4 - b as f32 * 0.5;

            // 三层描边：宽幅柔光 + 主色线 + 高光芯。
            for i in 0..LINE_POINTS - 1 {
                let t = i as f32 / (LINE_POINTS - 1) as f32;
                let rgb = ramp_color(pal, Self::hue_at(b, t, self.time));
                let glow = Color32::from_rgba_premultiplied(rgb[0], rgb[1], rgb[2], 58);
                p.line_segment([point(i), point(i + 1)], Stroke::new(main_w * 3.4, glow));
            }
            for i in 0..LINE_POINTS - 1 {
                let t = i as f32 / (LINE_POINTS - 1) as f32;
                let a = point(i);
                let bb = point(i + 1);
                let rgb = ramp_color(pal, Self::hue_at(b, t, self.time));
                // 能量越高，主线越亮越实。
                let boost = 0.55 + line.energy * 0.45;
                let col = Color32::from_rgba_premultiplied(
                    rgb[0], rgb[1], rgb[2], (230.0 * boost) as u8,
                );
                p.line_segment([a, bb], Stroke::new(main_w, col));
                p.line_segment(
                    [pos2(a.x, a.y - 0.6), pos2(bb.x, bb.y - 0.6)],
                    Stroke::new(0.9, Color32::from_white_alpha(120)),
                );
            }
        }

        // 粒子：透明度随寿命渐隐；先画一圈浅灰描边，保证浅色（黄/青）粒子在白底上也醒目。
        for s in &self.sparks {
            let life_t = (s.age / s.life).clamp(0.0, 1.0);
            let alpha = ((1.0 - life_t).powf(1.2) * 250.0) as u8;
            let rgb = ramp_color(pal, s.hue);
            let center = pos2(r.left() + s.x * r.width(), r.top() + s.y * r.height());
            let rad = s.radius * (1.0 - life_t * 0.3);
            p.circle_filled(center, rad + 0.8, Color32::from_gray(150).gamma_multiply(alpha as f32 / 255.0));
            p.circle_filled(center, rad, Color32::from_rgba_premultiplied(rgb[0], rgb[1], rgb[2], alpha));
            if rad > 2.4 {
                p.circle_filled(
                    pos2(center.x - 0.6, center.y - 0.6),
                    rad * 0.32,
                    Color32::from_white_alpha((alpha as f32 * 0.85) as u8),
                );
            }
        }
    }
}
