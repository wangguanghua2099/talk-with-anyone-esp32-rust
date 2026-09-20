// font.rs —— u8g2 格式中文字库（efont CN 14px）解码与渲染
//
// 数据来源：LovyanGFX 的 lgfx_efont_cn.c（与 C++ 固件同一字库，视觉完全一致），
// 提取为纯二进制 blob 后 include_bytes! 进固件（262KB，8MB Flash 无压力）。
// 解码算法逐行对照 LovyanGFX lgfx_fonts.cpp 的 U8g2font（u8g2 官方格式）：
//   · 字形查找：ASCII 直查段 + unicode 两级 LUT 段
//   · 度量：按位宽读取 w/h/xoffset/yoffset/xadvance
//   · 位图：RLE（bg 运行/fg 运行交替 + 续传位；续传位=1 时同对运行重复，
//     用于压缩同构行——已用 !/.-/_/|/0 等字形离线验证形状正确）

pub static FONT: &[u8] = include_bytes!("fonts/efont_cn_14.bin");

/// 字形度量（相对绘制原点）
#[derive(Debug, Clone, Copy)]
pub struct GlyphMetric {
    pub width: usize,
    pub height: usize,
    pub xoffset: i32,
    /// 从行文本顶到字形顶的行数（基线对齐推导：ascent - char_y - h）
    pub ytop: i32,
    pub xadvance: i32,
}

fn u16be(o: usize) -> usize {
    ((FONT[o] as usize) << 8) | FONT[o + 1] as usize
}

fn i8at(o: usize) -> i32 {
    FONT[o] as i8 as i32
}

/// 字体级参数
pub struct FontInfo;
impl FontInfo {
    pub fn max_width() -> i32 {
        i8at(9)
    }
    pub fn max_height() -> i32 {
        i8at(10)
    }
    /// 文本顶到基线的像素数（ascent）
    pub fn ascent() -> i32 {
        i8at(10) + i8at(12)
    }
}

/// 按位读取器（跨字节小端序位累积，与 LovyanGFX get_unsigned_bits 一致）
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, bit: 0 }
    }

    fn unsigned(&mut self, cnt: u32) -> u32 {
        let mut val = (self.data[self.pos] >> self.bit) as u32;
        let mut total = self.bit + cnt;
        if total >= 8 {
            total -= 8;
            self.pos += 1;
            val |= (self.data[self.pos] as u32) << (8 - self.bit);
        }
        self.bit = total;
        val & ((1 << cnt) - 1)
    }

    fn signed(&mut self, cnt: u32) -> i32 {
        self.unsigned(cnt) as i32 - (1 << (cnt - 1))
    }
}

/// 字形数据查找（对应 LovyanGFX U8g2font::getGlyph）
fn get_glyph(uni: usize) -> Option<&'static [u8]> {
    let mut font = 23usize; // 跳过 23 字节字体头
    if uni <= 255 {
        if uni >= 0x61 {
            font += u16be(19); // start_pos_lower_a
        } else if uni >= 0x41 {
            font += u16be(17); // start_pos_upper_A
        }
        loop {
            let step = FONT[font + 1] as usize;
            if step == 0 {
                return None;
            }
            if FONT[font] as usize == uni {
                return Some(&FONT[font + 2..]);
            }
            font += step;
        }
    }
    font += u16be(21); // start_pos_unicode
    let mut lut = font;
    loop {
        font += u16be(lut);
        let end = u16be(lut + 2);
        lut += 4;
        if end >= uni {
            break;
        }
    }
    loop {
        let e = u16be(font);
        if e == 0 {
            return None;
        }
        if e == uni {
            return Some(&FONT[font + 3..]);
        }
        font += FONT[font + 2] as usize;
    }
}

/// 解码字形度量（对应 U8g2font::updateFontMetric + getDefaultMetric）
fn glyph_metric(glyph: &[u8]) -> GlyphMetric {
    let mut d = BitReader::new(glyph);
    let width = d.unsigned(FONT[4] as u32) as usize;
    let height = d.unsigned(FONT[5] as u32) as usize;
    let xoffset = d.signed(FONT[6] as u32);
    let char_y = d.signed(FONT[7] as u32);
    let xadvance = d.signed(FONT[8] as u32);
    // 字形顶 = 文本顶 + ascent - char_y - h（基线对齐推导，见模块注释）
    let ytop = FontInfo::ascent() - char_y - height as i32;
    GlyphMetric { width, height, xoffset, ytop, xadvance }
}

/// 一次前向扫描同时解出度量与点阵（度量位和点阵位共用同一条比特流，
/// 分两次从头读会把度量位误当成游程数据），plot 收到的是行内坐标。
fn draw_glyph(glyph: &[u8], mut plot: impl FnMut(i32, i32)) -> GlyphMetric {
    let mut d = BitReader::new(glyph);
    let w = d.unsigned(FONT[4] as u32) as usize;
    let h = d.unsigned(FONT[5] as u32) as usize;
    let xoffset = d.signed(FONT[6] as u32);
    let char_y = d.signed(FONT[7] as u32);
    let xadvance = d.signed(FONT[8] as u32);
    // 字形顶 = 文本顶 + ascent - char_y - h（基线对齐推导，见模块注释）
    let ytop = FontInfo::ascent() - char_y - h as i32;
    let m = GlyphMetric { width: w, height: h, xoffset, ytop, xadvance };

    let b0 = FONT[2] as u32;
    let b1 = FONT[3] as u32;
    let mut lx = 0usize;
    let mut ly = 0usize;
    if w != 0 {
        loop {
            let ab0 = d.unsigned(b0);
            let ab1 = d.unsigned(b1);
            let mut i = 0u32;
            loop {
                let mut length = if i == 0 { ab0 } else { ab1 };
                while length > 0 {
                    let ln = length.min((w - lx) as u32);
                    length -= ln;
                    if i == 1 && ly < h {
                        for x in lx..lx + ln as usize {
                            plot((x + m.xoffset as usize) as i32, (ly as i32) + ytop);
                        }
                    }
                    lx += ln as usize;
                    if lx == w {
                        lx = 0;
                        ly += 1;
                    }
                }
                i ^= 1;
                // 续传位=1：同一对运行重复（同构行压缩）
                if i == 0 && d.unsigned(1) == 0 {
                    break;
                }
            }
            if ly >= h {
                break;
            }
        }
    }
    m
}

/// 量取字符串宽度（像素）。无法解码的字形按最大宽兜底。
pub fn text_width(s: &str) -> i32 {
    let mut total = 0i32;
    for c in s.chars() {
        let uni = c as usize;
        match get_glyph(uni) {
            Some(g) => total += glyph_metric(g).xadvance,
            None => total += FontInfo::max_width(),
        }
    }
    total
}

/// 把字符串的前景色像素画进 `plot`（坐标系原点是整行文本的左上角，
/// 字形已按 ascent 做基线对齐，背景由调用方预填）。
/// 超出 `x_limit` 像素宽度的部分截断（对应 C++ textW > SUB_W 的行为）。
pub fn for_each_pixel(s: &str, x_limit: i32, mut plot: impl FnMut(i32, i32)) {
    let mut x = 0i32;
    for c in s.chars() {
        if x >= x_limit {
            break;
        }
        let uni = c as usize;
        if let Some(g) = get_glyph(uni) {
            let pen = x;
            let m = draw_glyph(g, |px, py| {
                let sx = pen + px;
                if sx >= 0 && sx < x_limit {
                    plot(sx, py);
                }
            });
            x += m.xadvance;
        } else {
            x += FontInfo::max_width();
        }
    }
}

pub fn has_glyph(c: char) -> bool {
    get_glyph(c as usize).is_some()
}
