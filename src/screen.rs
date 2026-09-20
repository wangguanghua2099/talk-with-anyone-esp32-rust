// screen.rs —— ST7789 240x240 SPI 面板驱动（1.54 寸小智屏）
//
// 初始化命令序列逐条照抄实测过的 LovyanGFX Panel_ST7789 list0 + 本板配置
// （invert=true / rgb_order=false / panel=memory=240x240 即无偏移）。
// MADCTL 用 0x08（BGR=1）：实测本模组 0xF800 显示成蓝、青黄互换而灰阶正常，
// 三条诊断带互相印证是 R/B 交换（BGR 位），不是字节序。
//
// 【卡住五轮烧录的 bug，改这里务必守住】
// RAMWR(0x2C) 是**命令**，必须 DC=0 发出，之后才能抬 DC=1 送像素流。之前
// set_window 写成 `dc.set_high(); write(RAMWR)`，等于把 0x2C 当像素数据塞进 GRAM：
// CASET/RASET 都对、初始化也都对（它们走 cmd，DC=0 正确），但面板从未进入
// "接收像素流"状态 → 所有像素被丢弃 → 屏幕永远停在未初始化 GRAM 的雪花上。
// bug 在 DC 语义层而不是"怎么发字节"层，所以换 DMA/CPU 写、硬件/手工 CS、
// 40/10/1MHz、mode3/mode0、SPI2/SPI3 现象全都一模一样，排查被整个带偏。
//
// 【两个 esp-hal 约束】
//  1. `SpiDma` 的切片接口（write）对短/非对齐数据要拷进 dma_state 的内部缓冲，
//     **必须先 with_buffers() 注册**，否则 chunk_size()==0 → Err(BufferTooSmall)。
//  2. SPI 写错误一律打印，不许 `let _ =` 吞掉 —— 吞掉后"屏毫无反应"无法诊断。
//
// CS 用普通 GPIO 手工拉：一次逻辑事务（命令+参数+像素流）内 CS 全程有效，与
// LGFX 一致；esp-hal 的硬件 CS 会按每次 write 独立拉放。

use alloc::vec::Vec;

use esp_hal::{
    delay::Delay,
    dma::{DmaRxBuf, DmaTxBuf},
    gpio::{Level, NoPin, Output, OutputConfig},
    spi::{
        Mode,
        master::{Config, Spi, SpiDma},
    },
    time::Rate,
    Blocking,
};
use esp_println::println;

use crate::render::SCREEN_W;

const CMD_SWRESET: u8 = 0x01;
const CMD_SLPOUT: u8 = 0x11;
const CMD_NORON: u8 = 0x13;
const CMD_INVON: u8 = 0x21;
const CMD_CASET: u8 = 0x2A;
const CMD_RASET: u8 = 0x2B;
const CMD_RAMWR: u8 = 0x2C;
const CMD_COLMOD: u8 = 0x3A;
const CMD_MADCTL: u8 = 0x36;
const CMD_DISPON: u8 = 0x29;
const CMD_IDMOFF: u8 = 0x38;

/// SPI 时钟：bit-bang 验证通路时是 500kHz，这里回到硬件 SPI。
/// C++/LGFX 用 40MHz，先取 20MHz 保守一档，稳定后再提。
const SPI_MHZ: u32 = 20;

pub struct Screen {
    spi: SpiDma<'static, Blocking>,
    dc: Output<'static>,
    cs: Output<'static>,
    /// 单行暂存（fill_area 逐行推送用），RGB565 大端
    row: Vec<u8>,
    ok: bool,
    /// SPI 写错误打印节流（1ms 一圈的主循环会把串口刷爆，反而看不到现场）
    err_printed: u32,
    delay: Delay,
}

impl Screen {
    /// 建 SPI2 + DMA（含 with_buffers）+ DC/CS 引脚，并初始化面板。
    /// 返回 None = SPI 建不起来（Spi2 实例在 Err 分支被消费，无法重试）。
    pub fn new(
        spi2: esp_hal::peripherals::SPI2<'static>,
        dma_ch: esp_hal::peripherals::DMA_CH2<'static>,
        rx_buf: DmaRxBuf,
        tx_buf: DmaTxBuf,
        sclk: esp_hal::peripherals::GPIO9<'static>,
        mosi: esp_hal::peripherals::GPIO10<'static>,
        cs: esp_hal::peripherals::GPIO14<'static>,
        dc: esp_hal::peripherals::GPIO8<'static>,
        res: esp_hal::peripherals::GPIO18<'static>,
        delay: &Delay,
    ) -> Option<Self> {
        let dc = Output::new(dc, Level::Low, OutputConfig::default());
        let cs_pin = Output::new(cs, Level::High, OutputConfig::default());
        // 硬件复位（esp-hal 的 Output/Flex/AnyPin 未实现 Drop，rst 离开作用域后
        // GPIO18 仍保持已配置的输出高电平）
        let mut rst = Output::new(res, Level::High, OutputConfig::default());
        delay.delay_millis(20);
        rst.set_low();
        delay.delay_millis(20);
        rst.set_high();
        delay.delay_millis(150);

        // 注意：不调 with_cs —— CS 由上面的 GPIO 手工控制
        let spi = match Spi::new(
            spi2,
            Config::default()
                .with_frequency(Rate::from_mhz(SPI_MHZ))
                .with_mode(Mode::_3),
        ) {
            Ok(s) => s
                .with_sck(sclk)
                .with_mosi(mosi)
                .with_miso(NoPin)
                .with_dma(dma_ch)
                .with_buffers(rx_buf, tx_buf),
            Err(e) => {
                println!("[UI] SPI 配置失败 {:?}", e);
                return None;
            }
        };

        let mut row: Vec<u8> = Vec::new();
        let _ = row.try_reserve_exact(SCREEN_W * 2);
        row.resize(SCREEN_W * 2, 0);

        let mut s = Self {
            spi,
            dc,
            cs: cs_pin,
            row,
            ok: true,
            err_printed: 0,
            delay: Delay::new(),
        };
        s.init_panel(delay);
        Some(s)
    }

    pub fn is_ready(&self) -> bool {
        self.ok
    }

    // ---------- 事务 / 命令 / 数据 ----------

    fn begin_txn(&mut self) {
        self.cs.set_low();
    }

    fn end_txn(&mut self) {
        self.cs.set_high();
    }

    /// 一次 SPI 写。**错误必须可见**，但只可见前几条：主循环 1ms 一圈，
    /// 每条都打印会把串口刷满，真正的现场反而看不到了。
    fn write_bytes(&mut self, data: &[u8]) {
        if let Err(e) = self.spi.write(data) {
            if self.err_printed < 3 {
                self.err_printed += 1;
                println!("[UI] SPI 写失败 {:?} (len={})", e, data.len());
                if self.err_printed == 3 {
                    println!("[UI] 同类错误不再逐条打印，改为每次面板重 init 时提示");
                }
            }
            self.ok = false;
        }
    }

    /// DC=0 发命令字节、DC=1 发参数（须在 begin_txn/end_txn 之间调用）
    fn cmd(&mut self, c: u8, args: &[u8]) {
        self.dc.set_low();
        self.write_bytes(&[c]);
        if !args.is_empty() {
            self.dc.set_high();
            self.write_bytes(args);
        }
    }

    /// 自成一个事务的一条命令
    fn cmd_alone(&mut self, c: u8, args: &[u8]) {
        if !self.ok {
            return;
        }
        self.begin_txn();
        self.cmd(c, args);
        self.end_txn();
    }

    fn init_panel(&mut self, delay: &Delay) {
        // 与 LGFX 一致：SWRESET → SLPOUT
        self.cmd_alone(CMD_SWRESET, &[]);
        delay.delay_millis(120);
        self.cmd_alone(CMD_SLPOUT, &[]);
        delay.delay_millis(120);

        // ↓ 与 Panel_ST7789 list0 一一对应（PORCTRL/FRCTR2 按 LGFX 注释跳过：
        //   该两寄存器在不同型号上规格冲突，原厂也注释掉了）
        self.cmd_alone(0xB7, &[0x35]); // GCTRL
        self.cmd_alone(0xBB, &[0x28]); // VCOMS
        self.cmd_alone(0xC0, &[0x0C]); // LCMCTRL
        self.cmd_alone(0xC2, &[0x01, 0xFF]); // VDVVRHEN
        self.cmd_alone(0xC3, &[0x10]); // VRHS
        self.cmd_alone(0xC4, &[0x20]); // VDVSET
        self.cmd_alone(0xD0, &[0xA4, 0xA1]); // PWCTRL1
        self.cmd_alone(0xB0, &[0x00, 0xC0]); // RAMCTRL
        self.cmd_alone(
            0xE0,
            &[0xD0, 0x00, 0x02, 0x07, 0x0A, 0x28, 0x32, 0x44, 0x42, 0x06, 0x0E, 0x12, 0x14, 0x17],
        ); // PVGAMCTRL
        self.cmd_alone(
            0xE1,
            &[0xD0, 0x00, 0x02, 0x07, 0x0A, 0x28, 0x31, 0x54, 0x47, 0x0E, 0x1C, 0x17, 0x1B, 0x1E],
        ); // NVGAMCTRL
        self.cmd_alone(CMD_IDMOFF, &[]);
        self.cmd_alone(CMD_COLMOD, &[0x55]);
        self.cmd_alone(CMD_MADCTL, &[0x08]); // BGR=1（实测本模组需要）
        self.cmd_alone(CMD_INVON, &[]);
        self.cmd_alone(CMD_NORON, &[]);
        self.cmd_alone(CMD_DISPON, &[]);
        if self.ok {
            println!(
                "[UI] 屏幕就绪 ST7789 240x240 (SPI2+DMA mode3 {}MHz · 手工 CS · BGR=1)",
                SPI_MHZ
            );
        }
    }

    /// 写失败后的恢复尝试：把初始化序列重发一遍（SWRESET+SLPOUT 约 250ms 阻塞），
    /// 由渲染层限流调用。瞬时错误（DMA 忙等）能自愈；面板真坏了也只是每隔若干秒
    /// 多一行日志——总比屏幕永远停在开机画面上、一点线索都不留强。
    pub fn try_recover(&mut self) {
        println!("[UI] 面板异常，重试初始化…");
        self.ok = true;
        self.err_printed = 0;
        let delay = self.delay;
        self.init_panel(&delay);
    }

    fn set_window(&mut self, x: u16, y: u16, w: u16, h: u16) {
        let (x0, x1) = (x, x + w - 1);
        let (y0, y1) = (y, y + h - 1);
        self.cmd(
            CMD_CASET,
            &[(x0 >> 8) as u8, x0 as u8, (x1 >> 8) as u8, x1 as u8],
        );
        self.cmd(
            CMD_RASET,
            &[(y0 >> 8) as u8, y0 as u8, (y1 >> 8) as u8, y1 as u8],
        );
        // RAMWR 用 DC=0 发（见文件头），发完抬 DC=1 才送像素
        self.dc.set_low();
        self.write_bytes(&[CMD_RAMWR]);
        self.dc.set_high();
    }

    /// 推送一块 RGB565 大端字节流。一行 240x18=8640B @20MHz ≈ 3.5ms（阻塞写）。
    pub fn push_band(&mut self, x: u16, y: u16, w: u16, h: u16, bytes: &[u8]) -> bool {
        if !self.ok {
            return false;
        }
        // 几何自检：窗口像素数必须与字节数严格一致，否则多出来的像素会顺着
        // GRAM 写指针溢出到别的行（真机"老行变噪点"就是这个形状）
        if bytes.len() != (w as usize) * (h as usize) * 2 {
            println!(
                "[UI] 长度不匹配: win={}x{} bytes={}",
                w,
                h,
                bytes.len()
            );
            return false;
        }
        self.begin_txn();
        self.set_window(x, y, w, h);
        self.write_bytes(bytes);
        self.end_txn();
        self.ok
    }

    /// 实心矩形（逐行推送，只占一行缓冲）。诊断色带用。
    pub fn fill_area(&mut self, x: u16, y: u16, w: u16, h: u16, color: u16) -> bool {
        if !self.ok {
            return false;
        }
        let n = w as usize * 2;
        if n > self.row.len() {
            return false; // 调用方保证 w ≤ SCREEN_W
        }
        let (hi, lo) = ((color >> 8) as u8, color as u8);
        for i in 0..n / 2 {
            self.row[i * 2] = hi;
            self.row[i * 2 + 1] = lo;
        }
        self.begin_txn();
        self.set_window(x, y, w, h);
        // 把 row 临时移出 self：否则 write_bytes(&self.row[..]) 会同时可变借用
        // self 与不可变借用 self.row
        let row = core::mem::take(&mut self.row);
        for _ in 0..h {
            self.write_bytes(&row[..n]);
        }
        self.row = row;
        self.end_txn();
        self.ok
    }

    /// 诊断色带：① 白/红/绿/蓝块 ② 红绿竖条 ③ 16 档灰阶 ④ 左青右黄。
    /// 每带各自回答一个故障类别（聋/字节序/BGR），开机跑一次便于回归。
    pub fn self_test_bars(&mut self) {
        if !self.ok {
            return;
        }
        for (i, c) in [0xFFFFu16, 0xF800, 0x07E0, 0x001F].iter().enumerate() {
            self.fill_area((i * 60) as u16, 0, 60, 60, *c);
        }
        for i in 0..8u16 {
            let c = if i % 2 == 0 { 0xF800 } else { 0x07E0 };
            self.fill_area(i * 30, 60, 30, 60, c);
        }
        for i in 0..16u16 {
            let v = i * 2; // 5bit 灰阶
            self.fill_area(i * 15, 120, 15, 60, (v << 11) | (v << 6) | v);
        }
        self.fill_area(0, 180, 120, 60, 0x07FF);
        self.fill_area(120, 180, 120, 60, 0xFFE0);
        println!("[UI] 诊断图形已推送：白/红/绿/蓝、红绿竖条、灰阶、左青右黄");
    }
}
