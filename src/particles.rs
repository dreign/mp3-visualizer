//! 粒子流可视化模式：跟随节奏跳动的炫彩粒子拖尾，整体向右前行，
//! 以随机几何形状（三角/方/五边形/六边形/菱形/星形）作为主锚定物。
//!
//! 设计要点：
//! - 坐标用相对画布的归一化值 0..1，窗口缩放后粒子位置自动按比例映射；
//! - 每个粒子保存最近若干帧的历史位置，绘制时按新旧递减透明度形成拖尾；
//! - 鼓点事件（Kick/Snare/Hihat）分别从随机锚定物喷发出不同数量/色段的粒子，
//!   并让所有锚定物产生跳动脉冲；低频能量与 BPM 提高环境发射率和右行速度；
//! - 几何锚定物绑定频段（低/中/高）：低频厚重、中频均衡、高频尖锐，
//!   出现概率由对应频段能量加权，寿命结束后按当前频谱重新选择频段；
//!   每个锚定物有自己的生命周期，淡入后维持、末段渐隐；
//! - 锚定物拖尾按所属频段的正弦频率与能量波动，低频摆幅大且慢、高频摆幅小且快；
//! - 背景速度线自右向左掠过，与粒子右行形成相对运动，强化「向右前行」的观感；
//! - 颜色统一取主题色带 ramp，切换色系时整屏粒子一起换色。

use crate::analysis::{AnalysisFrame, BeatKind};
use crate::theme::{ramp_color, Palette};
use eframe::egui::{pos2, Color32, Painter, Pos2, Rect, Shape, Stroke};
use std::f32::consts::TAU;
use std::time::Instant;

const TRAIL_LEN: usize = 14;       // 普通粒子拖尾长度
const ANCHOR_TRAIL_LEN: usize = 30; // 锚定物拖尾更长，突出主视觉
const MAX_PARTICLES: usize = 300;  // 粒子上限，防止长时间播放后无限增长
const ANCHOR_COUNT: usize = 8;     // 同时在场的几何锚定物数量
const FADE_IN: f32 = 0.16;         // 生命周期前 16% 淡入
const FADE_OUT: f32 = 0.36;        // 生命周期后 36% 渐隐

/// 锚定物可选的几何形状。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShapeKind {
    Triangle,
    Square,
    Pentagon,
    Hexagon,
    Diamond,
    Star,
}

/// 锚定物所属频段：决定形状、色段、寿命、尺寸与拖尾波动频率。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Band {
    Low,
    Mid,
    High,
}

impl Band {
    const ALL: [Band; 3] = [Band::Low, Band::Mid, Band::High];

    fn energy(self, f: &AnalysisFrame) -> f32 {
        match self {
            Band::Low => f.low,
            Band::Mid => f.mid,
            Band::High => f.high,
        }
    }

    /// 各频段取主题色带的不同区段，视觉上能区分低频/中频/高频几何。
    fn hue_range(self) -> (f32, f32) {
        match self {
            Band::Low => (0.00, 0.30),
            Band::Mid => (0.28, 0.66),
            Band::High => (0.62, 1.00),
        }
    }

    /// 低频厚重、中频均衡、高频尖锐。
    fn shapes(self) -> &'static [ShapeKind] {
        match self {
            Band::Low => &[ShapeKind::Square, ShapeKind::Hexagon, ShapeKind::Diamond],
            Band::Mid => &[ShapeKind::Pentagon, ShapeKind::Diamond, ShapeKind::Triangle],
            Band::High => &[ShapeKind::Triangle, ShapeKind::Star, ShapeKind::Hexagon],
        }
    }

    fn life_range(self) -> (f32, f32) {
        match self {
            Band::Low => (9.0, 15.0),
            Band::Mid => (6.0, 11.0),
            Band::High => (4.0, 8.0),
        }
    }

    fn size_range(self) -> (f32, f32) {
        match self {
            Band::Low => (26.0, 44.0),
            Band::Mid => (19.0, 33.0),
            Band::High => (13.0, 25.0),
        }
    }

    fn speed_range(self) -> (f32, f32) {
        match self {
            Band::Low => (0.055, 0.105),
            Band::Mid => (0.075, 0.140),
            Band::High => (0.100, 0.180),
        }
    }

    /// 拖尾波动角速度：低频慢、高频快。
    fn wave_speed(self) -> f32 {
        match self {
            Band::Low => 1.6,
            Band::Mid => 3.2,
            Band::High => 5.8,
        }
    }

    /// 拖尾波动幅度（归一化画布高度）：低频摆幅大、高频细密。
    fn wave_gain(self) -> f32 {
        match self {
            Band::Low => 0.020,
            Band::Mid => 0.013,
            Band::High => 0.008,
        }
    }

    fn from_beat(kind: BeatKind) -> Band {
        match kind {
            BeatKind::Kick => Band::Low,
            BeatKind::Snare => Band::Mid,
            BeatKind::Hihat => Band::High,
        }
    }
}

/// 轻量确定性 PRNG（xorshift64*），不引入 rand 依赖。
struct Rng(u64);
impl Rng {
    fn next_f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f32) / ((1u64 << 53) as f32)
    }
    fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.next_f32()
    }
    fn pick_shape(&mut self, band: Band) -> ShapeKind {
        let shapes = band.shapes();
        let i = (self.next_f32() * shapes.len() as f32) as usize;
        shapes[i.min(shapes.len() - 1)]
    }
}

/// 一枚粒子（归一化坐标 + 拖尾历史）。
struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    age: f32,
    life: f32,
    size: f32,
    hue: f32,
    trail: Vec<(f32, f32)>,
    trail_acc: f32,   // 拖尾按时间间隔采样，与帧率无关
}

/// 几何锚定物：绑定频段、右移浮动、随鼓点跳动，有生命周期并留下波动拖尾。
struct Anchor {
    band: Band,
    x: f32,
    y: f32,
    vx: f32,
    base_size: f32,
    hue: f32,
    kind: ShapeKind,
    rot: f32,
    spin: f32,
    phase: f32,
    wave_phase: f32,
    age: f32,
    life: f32,
    bounce: f32,
    trail: Vec<(f32, f32)>,
    trail_acc: f32,
}

/// 粒子场：由 app 每帧 update + draw。
pub struct ParticleField {
    particles: Vec<Particle>,
    anchors: Vec<Anchor>,
    rng: Rng,
    emit_acc: f32,
    scroll: f32,             // 背景速度线滚动相位
    time: f32,               // 累计时间，驱动锚定物拖尾波动
    last_beat_at: Option<Instant>, // 用事件时间去重，避免暂停时重复触发同一鼓点
    beat_text: String,
    beat_text_age: f32,
}

impl ParticleField {
    pub fn new() -> Self {
        let mut field = Self {
            particles: Vec::new(),
            anchors: Vec::new(),
            rng: Rng(0x9E37_79B9_7F4A_7C15),
            emit_acc: 0.0,
            scroll: 0.0,
            time: 0.0,
            last_beat_at: None,
            beat_text: String::new(),
            beat_text_age: 0.0,
        };
        // 初始三频段均匀铺开，位置覆盖整个画面，年龄打散，避免同时批量消失。
        for i in 0..ANCHOR_COUNT {
            let band = Band::ALL[i % Band::ALL.len()];
            let x = field.rng.range(0.0, 1.0);
            let age_ratio = field.rng.range(0.0, 0.65);
            field.anchors.push(Self::spawn_anchor(&mut field.rng, band, 0.5, x, age_ratio));
        }
        // 起始就铺一批粒子，切到该模式时画面不会空。
        for _ in 0..90 {
            field.particles.push(Self::spawn_particle(&mut field.rng, true));
        }
        field
    }

    fn spawn_particle(rng: &mut Rng, spread: bool) -> Particle {
        Particle {
            x: if spread { rng.range(0.0, 1.0) } else { rng.range(-0.08, 0.03) },
            y: rng.range(0.04, 0.96),
            vx: rng.range(0.16, 0.40),
            vy: rng.range(-0.05, 0.05),
            age: 0.0,
            life: rng.range(2.4, 5.2),
            size: rng.range(1.4, 3.4),
            hue: rng.range(0.0, 1.0),
            trail: Vec::with_capacity(TRAIL_LEN),
            trail_acc: rng.range(0.0, 0.035),
        }
    }

    /// 生成锚定物。x 由调用方指定：鼓点原地变形时保留原位置，回收重生时分散出生。
    fn spawn_anchor(rng: &mut Rng, band: Band, energy: f32, x: f32, age_ratio: f32) -> Anchor {
        let (life_lo, life_hi) = band.life_range();
        let life = rng.range(life_lo, life_hi);
        let (size_lo, size_hi) = band.size_range();
        let (speed_lo, speed_hi) = band.speed_range();
        let (hue_lo, hue_hi) = band.hue_range();
        Anchor {
            band,
            x,
            y: rng.range(0.14, 0.86),
            vx: rng.range(speed_lo, speed_hi),
            base_size: rng.range(size_lo, size_hi) * (0.86 + energy * 0.34),
            hue: rng.range(hue_lo, hue_hi),
            kind: rng.pick_shape(band),
            rot: rng.range(0.0, TAU),
            spin: rng.range(-0.9, 0.9),
            phase: rng.range(0.0, TAU),
            wave_phase: rng.range(0.0, TAU),
            age: life * age_ratio,
            life,
            bounce: 0.0,
            trail: Vec::with_capacity(ANCHOR_TRAIL_LEN),
            trail_acc: 0.0,
        }
    }

    /// 按当前三段能量加权选择新锚定物的频段：哪段响，哪段几何更容易出现。
    fn choose_band(&mut self, frame: &AnalysisFrame, playing: bool) -> Band {
        let weights = if playing {
            [
                0.28 + frame.low * 2.6,
                0.28 + frame.mid * 2.3,
                0.28 + frame.high * 2.0,
            ]
        } else {
            [1.0, 1.0, 1.0]
        };
        let total: f32 = weights.iter().sum();
        let mut r = self.rng.next_f32() * total;
        for (i, band) in Band::ALL.iter().enumerate() {
            if r < weights[i] {
                return *band;
            }
            r -= weights[i];
        }
        Band::Mid
    }

    /// 每帧推进粒子与锚定物。playing=false 时只保留环境飘动，不触发鼓点爆发。
    pub fn update(&mut self, dt: f32, frame: &AnalysisFrame, playing: bool) {
        self.time += dt;
        self.scroll += dt * (0.10 + frame.low * 0.55 + frame.mid * 0.20);
        self.beat_text_age = (self.beat_text_age - dt * 1.1).max(0.0);

        // ---- 鼓点：去重后触发爆发 + 对应频段锚定物重生 + 全体跳动 ----
        if let Some(beat) = &frame.beat {
            let is_new = self.last_beat_at.map_or(true, |t| t != beat.at);
            if is_new {
                self.last_beat_at = Some(beat.at);
                self.beat_text = beat.kind.label().to_string();
                self.beat_text_age = 1.0;
                let band = Band::from_beat(beat.kind);
                let strength = beat.strength;
                if playing {
                    use crate::analysis::BeatKind;
                    let (count, hue_lo, hue_hi, power) = match beat.kind {
                        BeatKind::Kick => (22usize, 0.00f32, 0.30f32, 1.00f32),
                        BeatKind::Snare => (14, 0.28, 0.66, 0.72),
                        BeatKind::Hihat => (9, 0.62, 1.00, 0.42),
                    };
                    // 从同频段的锚定物喷发，让几何体成为该频段的视觉锚点。
                    let candidates: Vec<usize> = self
                        .anchors
                        .iter()
                        .enumerate()
                        .filter(|(_, a)| a.band == band)
                        .map(|(i, _)| i)
                        .collect();
                    let (ox, oy) = if candidates.is_empty() {
                        (0.12, 0.5)
                    } else {
                        let pick = (self.rng.next_f32() * candidates.len() as f32) as usize;
                        let idx = candidates[pick.min(candidates.len() - 1)];
                        (self.anchors[idx].x, self.anchors[idx].y)
                    };
                    for _ in 0..count {
                        let mut p = Self::spawn_particle(&mut self.rng, false);
                        p.x = ox + self.rng.range(-0.02, 0.02);
                        p.y = (oy + self.rng.range(-0.05, 0.05)).clamp(0.0, 1.0);
                        p.vx = self.rng.range(0.28, 0.56) + strength * 0.26;
                        p.vy = self.rng.range(-0.18, 0.18);
                        p.hue = self.rng.range(hue_lo, hue_hi);
                        p.size = self.rng.range(1.8, 4.0);
                        p.life = self.rng.range(1.8, 3.8);
                        self.particles.push(p);
                    }
                    while self.particles.len() > MAX_PARTICLES {
                        self.particles.remove(0);
                    }
                    // 让该频段中寿命最接近终点的几何体「原地变形」：保留位置与拖尾，
                    // 只换形状/色段。避免频繁鼓点不断把几何体拉回左侧起点造成堆积。
                    if let Some(idx) = self
                        .anchors
                        .iter()
                        .enumerate()
                        .filter(|(_, a)| a.band == band)
                        .max_by(|(_, x), (_, y)| {
                            (x.age / x.life).partial_cmp(&(y.age / y.life))
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .map(|(i, _)| i)
                    {
                        let (keep_x, keep_y, keep_trail) = {
                            let a = &self.anchors[idx];
                            (a.x, a.y, a.trail.clone())
                        };
                        let mut fresh =
                            Self::spawn_anchor(&mut self.rng, band, strength, keep_x, 0.0);
                        fresh.y = keep_y;
                        fresh.trail = keep_trail;
                        fresh.bounce = 0.9;
                        self.anchors[idx] = fresh;
                    }
                    for a in &mut self.anchors {
                        a.bounce = (a.bounce + power * (0.65 + a.hue * 0.35)).min(1.35);
                    }
                }
            }
        }

        // ---- 环境发射：能量越高粒子越密 ----
        let rate = 5.0 + if playing {
            frame.low * 15.0 + frame.mid * 9.0 + frame.high * 6.0
        } else {
            0.0
        };
        self.emit_acc += dt * rate;
        while self.emit_acc >= 1.0 {
            self.emit_acc -= 1.0;
            if self.particles.len() < MAX_PARTICLES {
                self.particles.push(Self::spawn_particle(&mut self.rng, false));
            }
        }

        // ---- 推进粒子 ----
        let mut i = 0;
        while i < self.particles.len() {
            let p = &mut self.particles[i];
            p.age += dt;
            if p.age >= p.life || p.x > 1.16 {
                self.particles.remove(i);
                continue;
            }
            p.x += p.vx * dt;
            p.y += p.vy * dt;
            if p.y < 0.0 || p.y > 1.0 {
                p.vy = -p.vy;
                p.y = p.y.clamp(0.0, 1.0);
            }
            // 按 30ms 间隔采样拖尾：高帧率下不会挤成一团，拖尾覆盖约 0.42 秒路程。
            p.trail_acc += dt;
            if p.trail_acc >= 0.030 {
                p.trail_acc = 0.0;
                p.trail.push((p.x, p.y));
                if p.trail.len() > TRAIL_LEN {
                    p.trail.remove(0);
                }
            }
            i += 1;
        }

        // ---- 推进锚定物 ----
        for a in &mut self.anchors {
            let energy = a.band.energy(frame);
            a.age += dt;
            a.x += a.vx * dt;
            a.phase += dt * (1.2 + energy * 2.4); // 位置浮动也随频段能量加快
            a.rot += a.spin * dt * (1.0 + energy * 0.6);
            a.bounce *= (-dt * 3.4).exp();
            a.trail_acc += dt;
            if a.trail_acc >= 0.045 {
                a.trail_acc = 0.0;
                a.trail.push((a.x, a.y));
                if a.trail.len() > ANCHOR_TRAIL_LEN {
                    a.trail.remove(0);
                }
            }
        }
        // 寿命结束或出屏的锚定物：按当前频谱能量重新选择一个频段出现。
        let recycle: Vec<usize> = self
            .anchors
            .iter()
            .enumerate()
            .filter(|(_, a)| a.age >= a.life || a.x > 1.18)
            .map(|(i, _)| i)
            .collect();
        for idx in recycle {
            let band = self.choose_band(frame, playing);
            let energy = band.energy(frame);
            // 分散出生：在左半屏取几个候选，选离现有锚定物最远的位置，配合淡入自然出现。
            let mut best_x = 0.0;
            let mut best_gap = f32::NEG_INFINITY;
            for _ in 0..4 {
                let cand = self.rng.range(-0.08, 0.32);
                let gap = self
                    .anchors
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != idx)
                    .map(|(_, a)| (a.x - cand).abs())
                    .fold(f32::INFINITY, f32::min);
                if gap > best_gap {
                    best_gap = gap;
                    best_x = cand;
                }
            }
            self.anchors[idx] = Self::spawn_anchor(&mut self.rng, band, energy, best_x, 0.0);
        }
    }

    /// 把粒子场绘制到画布。
    pub fn draw(&self, p: &Painter, r: Rect, pal: &Palette, frame: &AnalysisFrame) {
        p.rect_filled(r, 0.0, rgb(&pal.bg));
        self.draw_speed_lines(p, r, pal, frame);

        // 普通粒子：先拖尾后本体，保证亮芯压在尾巴上。
        for pt in &self.particles {
            let alpha = (1.0 - pt.age / pt.life).clamp(0.0, 1.0);
            let col = rgb(&ramp_color(pal, pt.hue));
            let n = pt.trail.len();
            for (k, seg) in pt.trail.windows(2).enumerate() {
                let f = (k + 1) as f32 / n.max(1) as f32;
                let a = f * alpha * 0.85;
                if a < 0.012 { continue; }
                p.line_segment(
                    [to_screen(seg[0], r), to_screen(seg[1], r)],
                    Stroke::new(pt.size * (0.35 + f * 0.7), col.gamma_multiply(a)),
                );
            }
            let center = to_screen((pt.x, pt.y), r);
            let rad = pt.size * (0.7 + alpha * 0.6) * (1.0 + frame.pulse * 0.22);
            p.circle_filled(center, rad * 2.4, col.gamma_multiply(alpha * 0.11));
            p.circle_filled(center, rad, col.gamma_multiply(alpha * 0.95));
        }

        // 几何锚定物：生命周期淡入淡出 → 波动拖尾 → 外发光 → 主体 → 描边 → 内核。
        for a in &self.anchors {
            let col = rgb(&ramp_color(pal, a.hue));
            let energy = a.band.energy(frame);
            let life_t = (a.age / a.life).clamp(0.0, 1.0);
            let fade_in = (life_t / FADE_IN).clamp(0.0, 1.0);
            let fade_out = ((1.0 - life_t) / FADE_OUT).clamp(0.0, 1.0);
            let fade = fade_in.min(fade_out);
            let alpha = fade * fade * (3.0 - 2.0 * fade); // smoothstep，渐变更顺滑
            if alpha < 0.01 { continue; }

            // 拖尾按所属频段波动：低频慢而摆幅大，高频快而摆幅小；
            // 越靠近锚定物（f 越大）摆幅越大，尾部保持平直。
            let speed = a.band.wave_speed();
            let gain = a.band.wave_gain() * (0.30 + energy * 1.70);
            let n = a.trail.len();
            for (k, seg) in a.trail.windows(2).enumerate() {
                let f = (k + 1) as f32 / n.max(1) as f32;
                let amp = gain * (0.25 + f * 0.75);
                let y0 = wave_y(seg[0].1, self.time * speed, k as f32, a.wave_phase, amp);
                let y1 = wave_y(seg[1].1, self.time * speed, (k + 1) as f32, a.wave_phase, amp);
                let seg_alpha = f * alpha * (0.42 + energy * 0.50);
                if seg_alpha < 0.012 { continue; }
                p.line_segment(
                    [to_screen_xy(seg[0].0, y0, r), to_screen_xy(seg[1].0, y1, r)],
                    Stroke::new(
                        1.0 + f * a.base_size * 0.10 * (0.70 + energy * 0.70),
                        col.gamma_multiply(seg_alpha),
                    ),
                );
            }

            let cx = r.left() + a.x * r.width();
            let body_wave = wave_y(
                a.y + a.phase.sin() * 0.028,
                self.time * speed,
                0.0,
                a.wave_phase,
                gain * 1.2,
            );
            let cy = r.top() + body_wave * r.height();
            let size = a.base_size
                * (1.0 + a.bounce * 0.42 + frame.pulse * 0.10 + energy * 0.22);
            let outer = shape_points(a.kind, cx, cy, size * 1.35, a.rot);
            fill_poly(p, &outer, col.gamma_multiply((0.09 + a.bounce * 0.10) * alpha));

            let pts = shape_points(a.kind, cx, cy, size, a.rot);
            fill_poly(p, &pts, col.gamma_multiply((0.34 + a.bounce * 0.20) * alpha));
            let loop_pts = outline_points(&pts, a.kind);
            p.add(Shape::line(
                loop_pts,
                Stroke::new(1.8 + a.bounce * 1.6, col.gamma_multiply(0.92 * alpha)),
            ));
            let inner = shape_points(a.kind, cx, cy, size * 0.40, -a.rot * 0.6);
            fill_poly(p, &inner, col.gamma_multiply(0.78 * alpha));
        }

        // 鼓点类型提示（与粒子同步闪现）。
        if self.beat_text_age > 0.01 {
            p.text(
                Pos2::new(r.center().x, r.bottom() - 20.0),
                egui::Align2::CENTER_BOTTOM,
                &format!("● {}", self.beat_text),
                egui::FontId::proportional(15.0),
                rgb(&pal.accent).gamma_multiply(self.beat_text_age),
            );
        }
    }

    /// 自右向左掠过的速度线：粒子右行时背景相对后退，强化前行感。
    fn draw_speed_lines(&self, p: &Painter, r: Rect, pal: &Palette, frame: &AnalysisFrame) {
        let lanes = 17usize;
        for i in 0..lanes {
            let lane = (i as f32 + 0.5) / lanes as f32;
            let y = r.top() + lane * r.height();
            let speed = 0.30 + (i % 4) as f32 * 0.09 + frame.low * 0.3;
            let phase = (self.scroll * speed + i as f32 * 0.137) % 1.0;
            let x = r.right() - phase * (r.width() + 180.0) + 90.0;
            let len = 34.0 + (i % 5) as f32 * 20.0;
            let col = rgb(&ramp_color(pal, lane));
            p.line_segment(
                [Pos2::new(x, y), Pos2::new(x + len, y)],
                Stroke::new(1.0, col.gamma_multiply(0.10 + (i % 3) as f32 * 0.035)),
            );
        }
    }
}

/// 锚定物拖尾点的频率波动偏移（归一化 y）。
fn wave_y(base_y: f32, phase: f32, idx: f32, wave_phase: f32, amp: f32) -> f32 {
    base_y + (phase + idx * 0.42 + wave_phase).sin() * amp
}

fn to_screen(t: (f32, f32), r: Rect) -> Pos2 {
    Pos2::new(r.left() + t.0 * r.width(), r.top() + t.1 * r.height())
}

fn to_screen_xy(x: f32, y: f32, r: Rect) -> Pos2 {
    Pos2::new(r.left() + x * r.width(), r.top() + y * r.height())
}

fn rgb(c: &[u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

/// 生成几何形状顶点。星形第一个点为中心（作为三角扇锚点），轮廓绘制时跳过。
fn shape_points(kind: ShapeKind, cx: f32, cy: f32, radius: f32, rot: f32) -> Vec<Pos2> {
    let mut pts = Vec::new();
    if kind == ShapeKind::Star {
        pts.push(pos2(cx, cy));
        for i in 0..10 {
            let a = rot + i as f32 * TAU / 10.0;
            let rr = if i % 2 == 0 { radius } else { radius * 0.44 };
            pts.push(pos2(cx + rr * a.cos(), cy + rr * a.sin()));
        }
        return pts;
    }
    let n = match kind {
        ShapeKind::Triangle => 3,
        ShapeKind::Square | ShapeKind::Diamond => 4,
        ShapeKind::Pentagon => 5,
        ShapeKind::Hexagon => 6,
        ShapeKind::Star => unreachable!(),
    };
    let offset = if kind == ShapeKind::Diamond { TAU / 4.0 } else { -TAU / 2.0 };
    for i in 0..n {
        let a = rot + offset + i as f32 * TAU / n as f32;
        pts.push(pos2(cx + radius * a.cos(), cy + radius * a.sin()));
    }
    pts
}

/// 轮廓点序列（闭合）；星形跳过中心的三角扇锚点。
fn outline_points<'a>(pts: &'a [Pos2], kind: ShapeKind) -> Vec<Pos2> {
    let src = if kind == ShapeKind::Star { &pts[1..] } else { pts };
    if src.is_empty() { return Vec::new(); }
    let mut out = src.to_vec();
    out.push(src[0]);
    out
}

/// 用三角扇填充多边形（星形以中心点为扇根，凸形以首顶点为扇根）。
fn fill_poly(p: &Painter, pts: &[Pos2], col: Color32) {
    for i in 1..pts.len().saturating_sub(1) {
        p.add(Shape::convex_polygon(
            vec![pts[0], pts[i], pts[i + 1]],
            col,
            Stroke::NONE,
        ));
    }
}
