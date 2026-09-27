// font.rs —— 字库的解码与数据全部来自 crates.io，这里只留两个薄封装
//
// 解码器：https://crates.io/crates/lovyangfx-fonts （仓库 lovyangfx-fonts-rs，
// 带 PC 侧回归测试）；14px 中文点阵数据：https://crates.io/crates/lovyangfx-fonts-efont-cn
// （纯数据包，blob 与 LovyanGFX 上游数组逐字节一致，/efont 与 LovyanGFX 声明随包）。
// 主 crate 0.2 的 `fonts` 模块经 optional 依赖桥接数据包，固件只开一个 feature
// 就拿到句柄——本仓库不再自带 blob（历史上是 `lgyf-gen` 抽出后随仓库分发的）。
//
// 保留 `font::text_width` / `font::for_each_pixel` 这两个自由函数的意义：
// display.rs 与 render.rs 因此不必随身携带 `Font` 句柄，也就保持"除了字库谁都不
// 依赖"，能脱离硬件在 PC 上离线跑回归。`Font::new` 只解析 23 字节头（十几次带边界
// 检查的取字节），相对一次字形扫描可以忽略，所以每次调用现取句柄，不引入 static
// 可变性 / OnceLock 这类额外 machinery。

/// 14px 常规 eFont CN 的零拷贝句柄（`Font` 只含头部字段与 blob 引用，`Copy`，无堆无锁）。
/// 数据随固件编译进 Flash，短于 23 字节头这件事在编译期就不成立，数据包测试另有金标准覆盖。
fn font() -> lovyangfx_fonts::Font<'static> {
    lovyangfx_fonts::fonts::efont_cn_14()
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
