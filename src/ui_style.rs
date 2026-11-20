//! UI 设计 token（shadcn/ui 骨架 + transitions.dev 动效规范，移植到 egui）。
//!
//! 移植原则：
//! - 语义化配色（surface / border / foreground / muted / primary），主题色只从 Palette 派生一次；
//! - 单源圆角 RADIUS，其余全部按比例派生；
//! - 平面分层用 1px 边框，不靠阴影；
//! - 动效时长与缓动集中为常量，禁止散落魔法数字。

use crate::theme::Palette;
use eframe::egui::{self, Color32, Rounding, Stroke, Vec2};

/// 单源圆角（对应 --radius: 0.625rem）。
pub const RADIUS: f32 = 6.0;
pub const RADIUS_SM: f32 = RADIUS * 0.6;
pub const RADIUS_MD: f32 = RADIUS * 0.8;
pub const RADIUS_XL: f32 = RADIUS * 1.4;

/// 动效时长（秒）：进慢出快，与 transitions.dev 的 quick / fast 对齐。
pub const DUR_QUICK: f32 = 0.15; // 关闭、文字切换
pub const DUR_FAST: f32 = 0.25;  // 打开、模式/主题切换
pub const DUR_SLOW: f32 = 0.40;  // 面板入场

/// cubic-bezier(0.22, 1, 0.36, 1) 的标量近似：快出慢收，用于所有进向过渡。
pub fn ease_smooth_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// 语义化颜色集合。全部在 `tokens()` 里从当前 Palette 派生一次。
#[derive(Clone, Copy)]
pub struct UiColors {
    pub surface: Color32,        // 面板底
    pub surface_alt: Color32,    // 控件底 / 次级表面
    pub border: Color32,         // 1px 分层边框
    pub foreground: Color32,     // 正文
    pub muted: Color32,          // 次级文字
    pub primary: Color32,        // 主题强调色（按钮、选中态、焦点）
    pub primary_soft: Color32,   // hover 底色
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> Color32 {
    Color32::from_rgb(
        (a[0] as f32 + (b[0] as f32 - a[0] as f32) * t) as u8,
        (a[1] as f32 + (b[1] as f32 - a[1] as f32) * t) as u8,
        (a[2] as f32 + (b[2] as f32 - a[2] as f32) * t) as u8,
    )
}

pub fn tokens(pal: &Palette) -> UiColors {
    UiColors {
        surface: mix(pal.bg, [255, 255, 255], 0.065),
        surface_alt: mix(pal.bg, [255, 255, 255], 0.115),
        border: mix(pal.grid, [255, 255, 255], 0.22),
        foreground: Color32::from_rgb(232, 235, 244),
        muted: Color32::from_rgb(146, 153, 176),
        primary: Color32::from_rgb(pal.accent[0], pal.accent[1], pal.accent[2]),
        primary_soft: mix(pal.accent, pal.bg, 0.78),
    }
}

/// 每帧把 UI token 写进 egui 全局样式；主题切换时控件颜色随之整体变化。
pub fn apply(ctx: &egui::Context, pal: &Palette) {
    let t = tokens(pal);
    let mut style = (*ctx.style()).clone();

    // 间距：紧凑但透气；slider_width 为一般控件默认值，进度条在 draw_controls 里临时放大后恢复。
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(11.0, 5.0);
    style.spacing.slider_width = 108.0;
    style.spacing.combo_width = 104.0;

    let w = &mut style.visuals.widgets;
    // 非交互：弱前景 + 抬升表面，用于 label / 分隔区。
    w.noninteractive.bg_fill = t.surface_alt;
    w.noninteractive.weak_bg_fill = t.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.border);
    w.noninteractive.fg_stroke.color = t.muted;
    w.noninteractive.rounding = Rounding::same(RADIUS_MD);

    // 静止控件：1px 边框 + 抬升底色，正文高对比。
    w.inactive.bg_fill = t.surface_alt;
    w.inactive.weak_bg_fill = t.surface_alt;
    w.inactive.bg_stroke = Stroke::new(1.0, t.border);
    w.inactive.fg_stroke.color = t.foreground;
    w.inactive.rounding = Rounding::same(RADIUS_MD);

    // hover：只用浅主题色底，不变形、不发光（克制的反馈）。
    w.hovered.bg_fill = t.primary_soft;
    w.hovered.weak_bg_fill = t.primary_soft;
    w.hovered.bg_stroke = Stroke::new(1.0, t.primary.gamma_multiply(0.55));
    w.hovered.fg_stroke.color = t.foreground;
    w.hovered.rounding = Rounding::same(RADIUS_MD);

    // active：主题色填充，主操作语义。
    w.active.bg_fill = t.primary;
    w.active.weak_bg_fill = t.primary;
    w.active.bg_stroke = Stroke::new(1.0, t.primary);
    w.active.fg_stroke.color = Color32::from_rgb(16, 18, 28);
    w.active.rounding = Rounding::same(RADIUS_MD);

    w.open = w.hovered;

    // 选中态：主题色低透明度，不用高饱和整块。
    style.visuals.selection.bg_fill = t.primary.gamma_multiply(0.28);
    style.visuals.selection.stroke = Stroke::new(1.0, t.primary.gamma_multiply(0.7));
    style.visuals.extreme_bg_color = t.surface;
    style.visuals.faint_bg_color = t.surface_alt;
    style.visuals.window_fill = t.surface;
    style.visuals.window_stroke = Stroke::new(1.0, t.border);
    style.visuals.window_rounding = Rounding::same(RADIUS);

    ctx.set_style(style);
}

/// 面板 Frame：fill + 1px border + 单源圆角，阴影仅留浮层（这里都不用）。
pub fn panel_frame(t: &UiColors, side: PanelSide) -> egui::Frame {
    let rounding = match side {
        PanelSide::Top => Rounding {
            nw: 0.0, ne: 0.0, sw: RADIUS, se: RADIUS,
        },
        PanelSide::Bottom => Rounding {
            nw: RADIUS, ne: RADIUS, sw: 0.0, se: 0.0,
        },
        PanelSide::Left => Rounding {
            nw: 0.0, ne: RADIUS, sw: 0.0, se: RADIUS,
        },
    };
    egui::Frame {
        inner_margin: egui::Margin::symmetric(14.0, 9.0),
        outer_margin: egui::Margin::ZERO,
        rounding,
        shadow: egui::Shadow::NONE,
        fill: t.surface,
        stroke: Stroke::new(1.0, t.border),
    }
}

#[derive(Clone, Copy)]
pub enum PanelSide { Top, Bottom, Left }
