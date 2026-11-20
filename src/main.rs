//! MP3 图形化播放器（原型）
//! Rust + eframe/egui + symphonia + rodio + rustfft
//! 核心特色：对鼓点节奏做专项采集、计算与图形化呈现。

mod analysis;
mod app;
mod audio;
mod lyrics;
mod particles;
mod recorder;
mod ribbon;
mod theme;
mod ui_style;

use app::Mp3VizApp;
use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("MP3 可视化播放器 · 原型"),
        ..Default::default()
    };
    eframe::run_native(
        "MP3 可视化播放器",
        options,
        Box::new(|cc| {
            // 注入中文字体，避免中文显示为方块（egui 默认字体不含 CJK 字形）。
            setup_chinese_font(cc);
            Mp3VizApp::new()
                .map(|app| Box::new(app) as Box<dyn eframe::App>)
                .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(e))
        }),
    )
}

/// 从系统加载中文字体并设为默认族。
fn setup_chinese_font(cc: &eframe::CreationContext<'_>) {
    // 优先用黑体/宋体等标准 TTF（ab_glyph 对可变字体与 TTC 支持有限）。
    let candidates = [
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/simsun.ttc",
        "C:/Windows/Fonts/NotoSansSC-VF.ttf",
        "C:/Windows/Fonts/msyh.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cjk".to_owned(),
                egui::FontData::from_owned(bytes).into(),
            );
            // 放到 Proportional 与 Monospace 族最前，确保中文优先用它渲染。
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_owned());
            cc.egui_ctx.set_fonts(fonts);
            return;
        }
    }
    eprintln!("警告：未找到中文字体，界面中文可能显示为方块");
}
