// font.rs —— 字库解码交给 lovyangfx-fonts，这里只留"blob 从哪来"和两个薄封装
//
// 解码器本体（u8g2 线格式、RLE 游程、基线对齐、坏 blob 不 panic 不死循环）已抽成
// 独立 crate：https://crates.io/crates/lovyangfx-fonts （仓库 lovyanfx-fonts-rs，
// 那边带 PC 侧回归测试）。
//
// 字形数据不在 crate 里：本仓库继续自带 `src/fonts/efont_cn_14.bin`（262 kB），
// 它由该 crate 的 `lgyf-gen` 从各人自己的 LovyanGFX 检出里抽出，上游版权声明见
// LICENSE-fonts.md。
//
// 保留 `font::text_width` / `font::for_each_pixel` 这两个自由函数的意义：
// display.rs 与 render.rs 因此不必随身携带 `Font` 句柄，也就保持"除了这个 blob
// 谁都不依赖"，能脱离硬件在 PC 上离线跑回归。`Font::new` 只解析 23 字节头（十几次
// 带边界检查的取字节），相对一次字形扫描可以忽略，所以每次调用现取句柄，不引入
// static 可变性 / OnceLock 这类额外 machinery。

use lovyangfx_fonts::Font;

/// efont CN 14px 点阵 blob，编译期进固件（8MB Flash 无压力）
static FONT: &[u8] = include_bytes!("fonts/efont_cn_14.bin");

/// 句柄即取即用：`Font` 只含头部字段与 blob 引用（`Copy`，无堆无锁）
fn font() -> Font<'static> {
    // blob 随固件一起编译进来、长度固定，读不满头部这件事在编译期就不成立
    Font::new(FONT).expect("字库 blob 短于 23 字节头")
}

/// 量取字符串宽度（像素）。缺字形按 `max_width()` 兜底（与 LovyanGFX 一致）。
pub fn text_width(s: &str) -> i32 {
    font().text_width(s)
}

/// 把字符串的前景色像素画进 `plot`（坐标系原点是整行文本的左上角，字形已按 ascent
/// 做基线对齐，背景由调用方预填）。超出 `x_limit` 像素宽度的部分丢弃。
pub fn for_each_pixel(s: &str, x_limit: i32, plot: impl FnMut(i32, i32)) {
    font().for_each_pixel(s, x_limit, plot);
}
