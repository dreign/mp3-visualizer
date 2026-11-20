//! 配色色系（对应 FR-3.5 / FR-3.6）。
//! 每套色系除背景/网格/频段强调色外，还提供一条 7 停靠点的渐变色带（ramp），
//! 供双盘放射条、电平表、喇叭芯等整体图形着色，切换色系即整屏换色。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeId {
    Rainbow, // 经典彩虹（参考图原样：红→橙→黄→绿→青→蓝→紫）
    Neon,    // 霓虹青紫
    Aurora,  // 极光碧蓝
    Ember,   // 暖焰橙红
    Mono,    // 简约单色
}

impl ThemeId {
    pub const ALL: [ThemeId; 5] = [
        ThemeId::Rainbow,
        ThemeId::Neon,
        ThemeId::Aurora,
        ThemeId::Ember,
        ThemeId::Mono,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThemeId::Rainbow => "经典彩虹",
            ThemeId::Neon => "霓虹",
            ThemeId::Aurora => "极光",
            ThemeId::Ember => "暖焰",
            ThemeId::Mono => "单色",
        }
    }
}

/// 一组 sRGB 颜色（0-255）。`ramp` 为 7 停靠点渐变色带，t∈[0,1] 时从 ramp[0] 过渡到 ramp[6]。
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: [u8; 3],
    pub grid: [u8; 3],
    pub low: [u8; 3],    // 低频/底鼓
    pub mid: [u8; 3],
    pub high: [u8; 3],
    pub accent: [u8; 3], // 文字/节拍指示
    pub ramp: [[u8; 3]; 7],
}

/// 在渐变色带上取色（线性插值），供整体图形着色。
pub fn ramp_color(pal: &Palette, t: f32) -> [u8; 3] {
    let ramp = &pal.ramp;
    let n = ramp.len();
    let x = t.clamp(0.0, 1.0) * (n - 1) as f32;
    let i = (x as usize).min(n - 2);
    let f = x - i as f32;
    let a = ramp[i];
    let b = ramp[i + 1];
    [
        (a[0] as f32 + (b[0] as f32 - a[0] as f32) * f) as u8,
        (a[1] as f32 + (b[1] as f32 - a[1] as f32) * f) as u8,
        (a[2] as f32 + (b[2] as f32 - a[2] as f32) * f) as u8,
    ]
}

pub fn palette(id: ThemeId) -> Palette {
    match id {
        // 经典彩虹：1:1 还原参考图的红→紫 7 色放射条
        ThemeId::Rainbow => Palette {
            bg: [10, 12, 24],
            grid: [40, 44, 72],
            low: [60, 220, 230],
            mid: [120, 210, 100],
            high: [255, 77, 195],
            accent: [120, 230, 255],
            ramp: [
                [255, 64, 64], [255, 150, 40], [245, 230, 60], [80, 220, 100],
                [60, 220, 230], [70, 120, 255], [190, 70, 230],
            ],
        },
        // 霓虹：青蓝紫品红的冷色霓虹
        ThemeId::Neon => Palette {
            bg: [8, 10, 26],
            grid: [36, 40, 80],
            low: [0, 229, 255],
            mid: [138, 92, 255],
            high: [255, 77, 195],
            accent: [120, 230, 255],
            ramp: [
                [0, 240, 255], [30, 150, 255], [90, 90, 255], [150, 70, 255],
                [220, 60, 230], [255, 70, 180], [255, 120, 220],
            ],
        },
        // 极光：绿青蓝靛紫的冷艳极光
        ThemeId::Aurora => Palette {
            bg: [6, 16, 20],
            grid: [30, 60, 64],
            low: [40, 210, 130],
            mid: [50, 160, 240],
            high: [150, 70, 220],
            accent: [100, 230, 200],
            ramp: [
                [10, 120, 90], [40, 210, 130], [40, 230, 210], [50, 160, 240],
                [90, 90, 230], [150, 70, 220], [210, 80, 230],
            ],
        },
        // 暖焰：暗红橙金到白炽的火焰
        ThemeId::Ember => Palette {
            bg: [22, 10, 8],
            grid: [74, 38, 28],
            low: [255, 92, 48],
            mid: [255, 166, 0],
            high: [255, 130, 70],
            accent: [255, 190, 130],
            ramp: [
                [140, 24, 12], [230, 50, 20], [255, 110, 20], [255, 180, 40],
                [255, 230, 120], [255, 200, 90], [255, 245, 210],
            ],
        },
        // 单色：白灰阶，冷静克制
        ThemeId::Mono => Palette {
            bg: [14, 14, 16],
            grid: [52, 52, 58],
            low: [235, 235, 240],
            mid: [180, 180, 190],
            high: [130, 130, 140],
            accent: [220, 220, 230],
            ramp: [
                [245, 245, 250], [210, 210, 220], [175, 175, 186], [140, 140, 152],
                [105, 105, 116], [72, 72, 82], [44, 44, 52],
            ],
        },
    }
}
