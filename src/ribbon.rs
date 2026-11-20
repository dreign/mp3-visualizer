//! 炫彩光带模式（FR-3.15）：一条随音波与律动跳动的炫彩曲线，
//! 曲线震动时甩出粒子，粒子跟随震动方向飞行并逐渐渐隐；白色网格背景。
//!
//! 设计要点：
//! - 曲线由 AnalysisFrame.waveform 驱动，低频/中频能量与鼓点脉冲放大振幅；
//! - 波形经一阶低通平滑，避免逐帧抖动成锯齿，同时保留节奏感；
//! - 颜色沿曲线取主题色带，并随时间缓慢流动，切换色系整屏换色；
//! - 粒子从曲线采样点发射，继承该点的震动方向（垂直速度），带轻微重力与阻尼，
//!   生命周期内透明度渐隐；归一化坐标 0..1，窗口缩放自动映射；
//! - 带透视纵深的白色空间网格由本模块自绘（覆盖深色底），与其他深色模式形成鲜明对比：
//!   消失点在画布正中，纵线放射收敛、横线向消失点加密、远处淡近处浓，营造隧道式立体感。

use crate::analysis::AnalysisFrame;
use crate::theme::{ramp_color, Palette};
use eframe::egui::{pos2, Color32, Painter, Pos2, Rect, Stroke};

const LINE_POINTS: usize = 256; // 曲线采样点（与波形点数一致）
const MAX_SPARKS: usize = 320;  // 粒子上限，防止长时间播放无限增长

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

/// 炫彩光带状态。切模式时保留现场（与粒子流一致）。
pub struct RibbonField {
    smooth: Vec<f32>,   // 平滑后的波形 -1..1
    sparks: Vec<Spark>,
    rng: u64,
    time: f32,          // 视觉时钟，驱动颜色流动
    amplitude: f32,     // 当前振幅（相对画布高度比例），每帧随能量更新
    jump: f32,          // 随节拍的整体上下跳动（相对画布高度比例，带符号）
}

impl RibbonField {
    pub fn new() -> Self {
        Self {
            smooth: vec![0.0; LINE_POINTS],
            sparks: Vec::new(),
            rng: 0x9E37_79B9_7F4A_7C15,
            time: 0.0,
            amplitude: 0.18,
            jump: 0.0,
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

    /// 曲线振幅（相对画布高度的比例），由频段能量与鼓点脉冲驱动。
    fn amplitude_of(frame: &AnalysisFrame) -> f32 {
        let e = frame.low * 0.55 + frame.mid * 0.30 + frame.high * 0.15;
        (0.16 + e * 0.34 + frame.pulse * 0.26).clamp(0.14, 0.5)
    }

    /// 波形非线性放大：小振幅也能看出明显起伏（保留符号，幂次 < 1）。
    fn shape_wave(w: f32) -> f32 {
        w.signum() * w.abs().powf(0.6)
    }

    /// 曲线在某点的归一化 y（0 中心，值越大越靠上）；含波形起伏 + 全局节拍跳动。
    /// draw 与粒子发射共用，保证粒子正好从曲线上甩出。
    fn curve_y(&self, t: f32) -> f32 {
        let w = Self::shape_wave(self.sample(t));
        w * self.amplitude + self.jump
    }

    /// 平滑后的波形值（供发射共用）。
    fn sample(&self, t: f32) -> f32 {
        let x = t.clamp(0.0, 1.0) * (self.smooth.len() - 1) as f32;
        let i = x as usize;
        let j = (i + 1).min(self.smooth.len() - 1);
        let f = x - i as f32;
        self.smooth[i] * (1.0 - f) + self.smooth[j] * f
    }

    pub fn update(&mut self, dt: f32, frame: &AnalysisFrame, playing: bool) {
        self.time += dt;
        self.amplitude = Self::amplitude_of(frame);

        // 全局节拍跳动：用 beat_phase 做平滑正弦起伏，鼓点脉冲放大幅度。
        // 即使波形平缓（如纯鼓点），整条线也会随节奏明显上下跳动。
        let phase = frame.beat_phase * std::f32::consts::TAU;
        let wave = phase.sin();
        let target_jump = wave * (0.035 + frame.pulse * 0.14 + frame.low * 0.05);
        self.jump += (target_jump - self.jump) * (1.0 - (-dt * 12.0).exp()) as f32;
        // 波形平滑：向当前帧缓动，系数帧率无关。
        let k = (1.0 - (-dt * 18.0).exp()) as f32;
        let n = frame.waveform.len().max(2);
        for i in 0..self.smooth.len() {
            let src = frame.waveform[(i * n / self.smooth.len()).min(n - 1)];
            self.smooth[i] += (src - self.smooth[i]) * k;
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

        // 发射：持续发射 + 能量/鼓点加量，粒子从曲线上甩出，随震动方向飞行。
        let energy = frame.low * 0.5 + frame.mid * 0.3 + frame.high * 0.2;
        let rate = 90.0 + energy * 220.0 + frame.pulse * 380.0;
        let mut budget = rate * dt;
        while budget >= 1.0 {
            budget -= 1.0;
            if self.sparks.len() >= MAX_SPARKS { break; }
            let t = self.rand();
            // 曲线在该点的归一化高度（向上为正），转成画布 y（向下为正）。
            let cy = self.curve_y(t);
            let y = 0.5 - cy;
            // 甩出方向：取曲线局部斜率的时间变化 + 全局跳动方向，叠加随机扰动。
            let slope = self.curve_y((t + 0.01).min(1.0)) - self.curve_y((t - 0.01).max(0.0));
            let kick = 0.10 + energy * 0.26 + frame.pulse * 0.5;
            // 主方向随跳动+斜率，叠加较大的随机散布，粒子明显脱离曲线四散后渐隐。
            let vy = (self.jump.signum() * 0.6 - slope * 2.0) * kick
                + (self.rand() - 0.5) * 0.34;
            let vx = (self.rand() - 0.5) * 0.22 + 0.03;
            // 先算完所有随机数，避免在 push 的结构体字面量里再次借用 self。
            let life = 0.6 + self.rand() * 1.1;
            let radius = 2.2 + self.rand() * 3.2 + frame.pulse * 1.8;
            self.sparks.push(Spark {
                x: t,
                y,
                vx,
                vy,
                hue: (t + self.time * 0.05) % 1.0,
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

    /// 绘制白色网格 + 炫彩光带 + 渐隐粒子。
    pub fn draw(&self, p: &Painter, r: Rect, pal: &Palette) {
        // 带透视纵深的白色空间网格（自绘，覆盖深色画布底）。
        Self::draw_space_grid(p, r);

        let n = self.smooth.len();
        let h = r.height();
        let cy = r.center().y;
        let color_at = |t: f32| -> Color32 {
            let rgb = ramp_color(pal, (t + self.time * 0.05) % 1.0);
            Color32::from_rgb(rgb[0], rgb[1], rgb[2])
        };
        let point = |i: usize| -> Pos2 {
            let t = i as f32 / (n - 1) as f32;
            pos2(r.left() + t * r.width(), cy - self.curve_y(t) * h)
        };

        // 三层描边：宽幅柔光 + 主色线 + 高光芯。
        for i in 0..n - 1 {
            let t = i as f32 / (n - 1) as f32;
            let rgb = ramp_color(pal, (t + self.time * 0.05) % 1.0);
            let glow = Color32::from_rgba_premultiplied(rgb[0], rgb[1], rgb[2], 70);
            p.line_segment([point(i), point(i + 1)], Stroke::new(13.0, glow));
        }
        for i in 0..n - 1 {
            let t = i as f32 / (n - 1) as f32;
            let a = point(i);
            let b = point(i + 1);
            p.line_segment([a, b], Stroke::new(3.2, color_at(t)));
            p.line_segment(
                [pos2(a.x, a.y - 0.7), pos2(b.x, b.y - 0.7)],
                Stroke::new(1.0, Color32::from_white_alpha(140)),
            );
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
