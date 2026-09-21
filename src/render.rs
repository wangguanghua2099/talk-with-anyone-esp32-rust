// render.rs —— 字幕/顶栏的屏幕绘制层（对应 C++ display_ui.cpp 的绘制部分）
//
// 布局与 C++ 完全一致：顶栏 0~21px，字幕区 24~239px 共 12 行 × 18px，
// 文本起点 x=4、行内上边距 2px，行宽上限 SUB_W=232px，历史 80 行滚动。
//
// 防闪烁策略照搬 C++ 的"逐行差异比对 + 前缀延长快路径"：
//   · 每圈把 12 个可见行的**期望内容**与上次绘制快照比对，没变的行一字节都不发；
//   · 打字常态下新行是旧行的前缀延长 → 只把新增字符那一段矩形推给面板，
//     旧像素一个字节不动，画面除新增字外完全静止；
//   · 结构性变化（滚动/新消息/光标）才整行清底重绘。
//
// 节拍：每圈最多推 MAX_PUSH 行（一行 240x18=8640B @20MHz ≈ 3.5ms 阻塞写），
// 且只在内容有变化时推。开机诊断色带保留 0.8 秒，随后渲染层**逐行**清屏接管。

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::display::{Display, DisplayState, SUB_W};
use crate::font;
use crate::screen::Screen;
use esp_println::println;

pub const SCREEN_W: usize = 240;
pub const LINE_H: usize = 18;
pub const BAR_H: usize = 22;
pub const SUB_Y: usize = 24;
/// 可见行数（(240 - 24) / 18 = 12 行）
pub const VISIBLE: usize = (240 - SUB_Y) / LINE_H;
const TEXT_X: i32 = 4;
const TEXT_TOP: i32 = 2;
const BLACK: u16 = 0x0000;
/// 聆听光标颜色（C++ CARET_COLOR #40FFA0 的合法 RGB565）
const CARET_COLOR: u16 = 0x47F4;
/// 顶栏底色（C++ #104010 的合法 RGB565）
const BAR_BG: u16 = 0x1202;
/// 单圈最多推送的矩形数（每矩形一行/顶栏）。DMA 下一行 ≈3.5ms，两行 7ms 对
/// 麦克风（16KB 流缓冲 ≈512ms）和音频（3MB 环）都无压力。
const MAX_PUSH: usize = 2;
/// 开机诊断色带的保持时长（ms），之后渲染层清屏接管
const BAR_HOLD_MS: u32 = 800;
/// 面板写失败后的重 init 间隔（ms）。一次失败就把面板永久标死，
/// 表现为"屏幕停在最后一帧"且串口再也不出东西，没法判断卡在哪。
const RECOVER_INTERVAL_MS: u32 = 2000;
/// 是否启用"前缀延长"快路径（只推新增字符那段矩形）。
/// 排障用：真机出现"越打字越噪点"时先关掉，改成每行整行重绘，
/// 用来区分"局部矩形的几何算错"和"别的原因"。
const EXT_FAST_PATH: bool = true;

/// 一行的已绘制快照（差异比对基准）
#[derive(Clone, Default)]
struct Slot {
    txt: String,
    col: u16,
    caret: bool,
}

pub struct Renderer {
    drawn: Vec<Slot>,
    /// 顶栏内容键（状态字/音量/分钟数打包成 u64，变了才重绘）。
    /// 用整数而不是字符串：顶栏每圈都要判断，字符串签名等于每毫秒一次堆分配。
    bar_key: u64,
    /// 绘制暂存带，RGB565 **大端字节**，长度 SCREEN_W * BAR_H * 2
    band: Vec<u8>,
    /// 开机色条保持期的起点时刻（0 = 还没开始计时）
    start_ms: u32,
    /// 开机清屏进度：下一次要清的起始行（240 = 已接管）
    clear_row: u16,
    /// 是否已接管屏幕
    taken: bool,
    /// 上次面板恢复尝试时刻（ms）
    recover_at: u32,
}

impl Renderer {
    pub fn new() -> Self {
        let mut band: Vec<u8> = Vec::new();
        let _ = band.try_reserve_exact(SCREEN_W * BAR_H * 2);
        band.resize(SCREEN_W * BAR_H * 2, 0);
        Self {
            drawn: allocvec(VISIBLE),
            bar_key: u64::MAX,
            band,
            start_ms: 0,
            clear_row: 0,
            taken: false,
            recover_at: 0,
        }
    }

    /// 主循环每圈调用。
    pub fn tick(&mut self, disp: &Display, screen: &mut Screen, now_ms: u32) {
        if !screen.is_ready() {
            // 面板报过写错误：不能就此放弃，每隔 RECOVER_INTERVAL_MS 重发一次初始化
            // 序列并留一行日志。瞬时错误能自愈，真坏了也至少持续给出线索。
            if now_ms.wrapping_sub(self.recover_at) >= RECOVER_INTERVAL_MS {
                self.recover_at = now_ms;
                screen.try_recover();
            }
            return;
        }

        // 开机色条自检保留期内不接管，方便肉眼判断面板时序
        if self.start_ms == 0 {
            self.start_ms = now_ms;
        }
        if now_ms.wrapping_sub(self.start_ms) < BAR_HOLD_MS {
            return;
        }
        if !self.taken {
            // 清屏**逐行摊开**：一次 fill_area(240x240) 会连续占住主循环上百毫秒，
            // 足以让 PDM 麦克风的流式 DMA 溢出、之后 read() 恒返回 0（实测踩过）。
            let h = LINE_H.min(240 - self.clear_row as usize);
            let n = SCREEN_W * h * 2;
            self.band[..n].fill(0);
            screen.push_band(0, self.clear_row, SCREEN_W as u16, h as u16, &self.band[..n]);
            self.clear_row += h as u16;
            if self.clear_row >= 240 {
                for s in self.drawn.iter_mut() {
                    *s = Slot::default();
                }
                self.taken = true;
                self.bar_key = u64::MAX;
                // 里程碑：日志里出现这一行 = 主循环活着、面板写入也活着。
                // 缺这一行而屏幕停在色带上 = 卡在本函数之前（或面板判死）。
                println!("[UI] 渲染层已接管屏幕（开机色带已清除）");
            }
            return;
        }

        let mut pushed = 0usize;
        if self.bar_dirty(disp, now_ms) {
            if !self.push_bar(disp, screen, now_ms) {
                return;
            }
            pushed += 1;
        }
        for slot in 0..VISIBLE {
            if pushed >= MAX_PUSH {
                return;
            }
            let Some((txt, col, caret)) = desired_row(disp, slot) else {
                // 该行应为空：快照非空则要擦掉
                if self.drawn[slot].txt.is_empty() && !self.drawn[slot].caret {
                    continue;
                }
                if !self.paint_row(screen, slot, "", BLACK, false, 0, SCREEN_W as i32) {
                    return;
                }
                self.drawn[slot] = Slot::default();
                pushed += 1;
                continue;
            };

            let d = &self.drawn[slot];
            // 内容完全一致 → 一字节都不发
            if d.txt == txt && d.col == col && d.caret == caret {
                continue;
            }
            let old_w = font::text_width(&d.txt);
            let new_w = font::text_width(&txt);
            // 前缀延长快路径：只续画新增的那一段矩形
            let can_extend = EXT_FAST_PATH && !caret
                && !d.caret
                && d.col == col
                && !d.txt.is_empty()
                && txt.starts_with(d.txt.as_str())
                && new_w > old_w
                && new_w <= SUB_W;
            let painted = if can_extend {
                // 窄矩形：屏幕 x 从 4+old_w 到 4+new_w（new_w ≤ SUB_W，必不越界）
                self.paint_row(screen, slot, &txt, col, false, TEXT_X + old_w, TEXT_X + new_w)
            } else {
                self.paint_row(screen, slot, &txt, col, caret, 0, SCREEN_W as i32)
            };
            if !painted {
                return;
            }
            self.drawn[slot] = Slot { txt, col, caret };
            pushed += 1;
        }
    }

    // ---------- 单行绘制 ----------

    /// 绘制第 `slot` 行的屏幕像素段 [sx0, sx1)。**坐标一律用屏幕 x**：
    /// 之前这里把"文本相对坐标"和"屏幕坐标"混着用，整行重绘算出 x=4、w=240
    /// → 右边界 243 超出屏宽，ST7789 写指针绕到下一行行首，每行错位几个像素
    /// 逐行累积 = 文字形状的密集噪点。
    /// 返回 false = 面板异常。
    fn paint_row(
        &mut self,
        screen: &mut Screen,
        slot: usize,
        text: &str,
        color: u16,
        caret: bool,
        sx0: i32,
        sx1: i32,
    ) -> bool {
        let (sx0, sx1) = (sx0.max(0), sx1.min(SCREEN_W as i32));
        let w = (sx1 - sx0).max(1) as usize;
        let band = &mut self.band[..w * LINE_H * 2];
        fill_color(band, BLACK);
        // 文本原点在第 sx0 列右侧 TEXT_X-sx0 处
        let shift = TEXT_X - sx0;
        raster(band, w, text, color, shift);
        if caret {
            raster(band, w, "_", CARET_COLOR, font::text_width(text) + shift);
        }
        let y = (SUB_Y + slot * LINE_H) as u16;
        screen.push_band(sx0 as u16, y, w as u16, LINE_H as u16, band)
    }

    // ---------- 顶栏 ----------

    fn bar_dirty(&self, disp: &Display, now_ms: u32) -> bool {
        self.bar_key != bar_key(disp, now_ms)
    }

    fn push_bar(&mut self, disp: &Display, screen: &mut Screen, now_ms: u32) -> bool {
        let band = &mut self.band[..SCREEN_W * BAR_H * 2];
        fill_color(band, BAR_BG);
        let state = disp.state();
        raster(band, SCREEN_W, state.as_str(), state.color(), 6);

        if show_volume(disp, now_ms) {
            let s = format_vol(disp.volume());
            raster(band, SCREEN_W, &s, 0x07FF, 176);
        } else {
            // 时间（UTC+8，SNTP 未同步时 "--:--"）
            let t = crate::ntp::clock_text(now_ms);
            raster(band, SCREEN_W, &t, 0xFFFF, 150);
            // WiFi 三格信号柱
            let wc = if state != DisplayState::Offline {
                0x07FF
            } else {
                0x630C
            };
            fill_rect(band, SCREEN_W, 190, 10, 3, 5, wc);
            fill_rect(band, SCREEN_W, 195, 7, 3, 8, wc);
            fill_rect(band, SCREEN_W, 200, 4, 3, 11, wc);
            // 电量百分比（右对齐），充电中且未满时左侧画小闪电（对应 C++ drawTopBar）
            let bt = batt_text(disp.batt_level());
            let bw = font::text_width(&bt);
            raster(band, SCREEN_W, &bt, 0xFFFF, 237 - bw);
            if disp.batt_charging() {
                let bx = 237 - bw - 8;
                draw_line(band, SCREEN_W, bx + 2, 3, bx, 8, 0xFFE0);
                draw_line(band, SCREEN_W, bx, 8, bx + 3, 8, 0xFFE0);
                draw_line(band, SCREEN_W, bx + 3, 8, bx + 1, 13, 0xFFE0);
            }
        }
        if !screen.push_band(0, 0, SCREEN_W as u16, BAR_H as u16, band) {
            return false;
        }
        // 只有推成功才更新键，否则下一圈重试
        self.bar_key = bar_key(disp, now_ms);
        true
    }
}

/// 顶栏内容键：任何会改变顶栏像素的状态都编进这一个 u64。
/// 位分配：状态字 3 | 音量临时条显示中 1 | 音量 7 | 分钟数(0..1439) 11 | 未同步 1
/// | 电量 7（未读到=127）| 充电 1
fn bar_key(disp: &Display, now_ms: u32) -> u64 {
    let state = match disp.state() {
        DisplayState::Idle => 0u64,
        DisplayState::Listening => 1,
        DisplayState::Playing => 2,
        DisplayState::Offline => 3,
    };
    let vol_shown = show_volume(disp, now_ms);
    let mut k = state | (if vol_shown { 1 << 3 } else { 0 }) | ((disp.volume() as u64 & 0x7F) << 4);
    if vol_shown {
        return k; // 音量条期间时间/信号柱/电量都不显示，不必掺进来
    }
    match crate::ntp::clock_hm(now_ms) {
        Some((h, m)) => k |= (((h * 60 + m) as u64) & 0x7FF) << 11,
        None => k |= 1 << 22, // 未同步
    }
    k |= (disp.batt_level().map_or(127u64, |v| v as u64) << 24) | ((disp.batt_charging() as u64) << 31);
    k
}

/// 音量临时条：调节后 1.5 秒内显示（C++ s_volShowUntil）
fn show_volume(disp: &Display, now_ms: u32) -> bool {
    let until = disp.volume_until_ms();
    until != 0 && now_ms.wrapping_sub(until) < u32::MAX / 2
}

fn format_vol(v: i32) -> String {
    use core::fmt::Write;
    let mut s = String::from("VOL ");
    let _ = write!(s, "{}%", v);
    s
}

/// 顶栏电量文字（None = 还没采样到，与 C++ s_battLevel<0 的 "--%" 一致）
fn batt_text(level: Option<u8>) -> String {
    use core::fmt::Write;
    let mut s = String::new();
    match level {
        Some(v) => {
            let _ = write!(s, "{}%", v);
        }
        None => s.push_str("--%"),
    }
    s
}

/// 视图序列 = 历史行(尾部 VISIBLE 个) + 当前打字行；聆听光标跟随最后一行。
/// 对应 C++ desiredRow()。返回 None = 该行应为空。
fn desired_row(disp: &Display, slot: usize) -> Option<(String, u16, bool)> {
    let hist = disp.history();
    let typing = disp.is_typing();
    let total = hist.len() + if typing { 1 } else { 0 };
    let listen = disp.state() == DisplayState::Listening;
    if total == 0 {
        // 空屏：仅 LISTEN 时首行画光标提示
        return if listen && slot == 0 {
            Some((String::new(), BLACK, true))
        } else {
            None
        };
    }
    let first = total.saturating_sub(VISIBLE);
    let abs = first + slot;
    if abs >= total {
        return None;
    }
    let (txt, col) = if typing && abs == total - 1 {
        (disp.cur_line().to_string(), disp.cur_color())
    } else {
        (hist[abs].text.clone(), hist[abs].color)
    };
    let caret = listen && abs == total - 1;
    Some((txt, col, caret))
}

// ---------- 光栅化 ----------

/// 整块填同一颜色（大端 RGB565）
fn fill_color(band: &mut [u8], color: u16) {
    let (hi, lo) = ((color >> 8) as u8, color as u8);
    for i in 0..band.len() / 2 {
        band[i * 2] = hi;
        band[i * 2 + 1] = lo;
    }
}

fn put(band: &mut [u8], stride_px: usize, x: i32, y: i32, color: u16) {
    if x < 0 || y < 0 {
        return;
    }
    let rows = band.len() / (stride_px * 2);
    if y as usize >= rows || x as usize >= stride_px {
        return;
    }
    let o = (y as usize * stride_px + x as usize) * 2;
    band[o] = (color >> 8) as u8;
    band[o + 1] = color as u8;
}

/// 把文本的前景色像素写进 band（band 底色由调用方预先填好）。
/// `shift` = 文本坐标到 band 列的平移：band 列 = 文本 x + shift。
///   · 顶栏：band 覆盖屏幕 x=0..239，文字从屏幕 x=6 起 → shift = +6；
///   · 字幕行：band 覆盖屏幕 x=TEXT_X+x0 起，文字原点在屏幕 x=TEXT_X → shift = -x0。
/// 越界像素丢弃。
fn raster(band: &mut [u8], w: usize, text: &str, color: u16, shift: i32) {
    if text.is_empty() {
        return;
    }
    font::for_each_pixel(text, SUB_W, |x, y| {
        let bx = x + shift;
        if bx < 0 || bx >= w as i32 {
            return;
        }
        put(band, w, bx, y + TEXT_TOP, color);
    });
}

fn fill_rect(band: &mut [u8], stride: usize, x: i32, y: i32, rw: i32, rh: i32, c: u16) {
    for yy in y..y + rh {
        for xx in x..x + rw {
            put(band, stride, xx, yy, c);
        }
    }
}

/// Bresenham 直线：只给顶栏充电小闪电那三段 3~11px 的折线用（C++ 侧是 drawLine）
fn draw_line(band: &mut [u8], stride: usize, x0: i32, y0: i32, x1: i32, y1: i32, c: u16) {
    let (dx, dy) = ((x1 - x0).abs(), (y1 - y0).abs());
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let (mut x, mut y) = (x0, y0);
    let mut err = dx - dy;
    loop {
        put(band, stride, x, y, c);
        if x == x1 && y == y1 {
            return;
        }
        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            x += sx;
        }
        if e2 < dx {
            err += dx;
            y += sy;
        }
    }
}

fn allocvec<T: Default>(n: usize) -> Vec<T> {
    let mut v = Vec::new();
    let _ = v.try_reserve_exact(n);
    while v.len() < n {
        v.push(T::default());
    }
    v
}
