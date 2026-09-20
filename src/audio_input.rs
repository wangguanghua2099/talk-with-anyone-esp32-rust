// audio_input.rs —— PDM 麦克风采集（对应 C++ audio_in.cpp）
//
// PDM 模式：GPIO2 输出时钟(CLK)，GPIO3 输入数据(DATA)，硬件 PDM2PCM 解调输出
// 16bit PCM；软件左移 MIC_GAIN_SHIFT 位补增益并限幅（与 C++ 一致）。
//
// 与 C++ 的实现差异（行为等价）：C++ 每次聆听会话安装/卸载 I2S 驱动；
// 这里驱动常驻，仅启停 DMA 传输（start/stop），开销更低。

use esp_hal::{
    dma::DmaRxStreamBuf,
    i2s::master::{I2sRxDmaTransfer, I2sRx},
    Blocking,
};
use esp_println::println;

use crate::config;

/// RX DMA 流式缓冲（内部 RAM）：16KB ≈ 0.5s@32KB/s，主循环轮询不到也不会丢
pub const RX_QUEUE_BYTES: usize = 16 * 1024;
/// DMA 描述符 chunk 大小
pub const RX_CHUNK_BYTES: usize = 1024;

pub struct AudioInput {
    rx: Option<I2sRx<'static, Blocking>>,
    rx_buf: Option<DmaRxStreamBuf>,
    transfer: Option<I2sRxDmaTransfer<'static, Blocking, DmaRxStreamBuf>>,
    scratch: heapless::Vec<u8, RX_CHUNK_BYTES>,
    first_printed: bool,
}

impl AudioInput {
    /// 创建输入对象。`rx`/`rx_buf` 由 main 按板级引脚构建后传入。
    pub fn new(rx: I2sRx<'static, Blocking>, rx_buf: DmaRxStreamBuf) -> Self {
        Self {
            rx: Some(rx),
            rx_buf: Some(rx_buf),
            transfer: None,
            scratch: heapless::Vec::new(),
            first_printed: false,
        }
    }

    /// 开始采集（对应 C++ AudioIn::begin + 聆听开始）
    pub fn start(&mut self) -> bool {
        if self.transfer.is_some() {
            return true;
        }
        let Some(rx) = self.rx.take() else {
            return false;
        };
        let Some(buf) = self.rx_buf.take() else {
            return false;
        };
        self.first_printed = false;
        println!("[AudioIn] 正在启动 PDM RX DMA...");
        match rx.read(buf) {
            Ok(transfer) => {
                println!(
                    "[AudioIn] PDM 麦克风开始采集 (CLK=IO{} DATA=IO{}, {}k/16bit/mono)",
                    config::PIN_MIC_PDM_CLK,
                    config::PIN_MIC_PDM_DIN,
                    config::MIC_SAMPLE_RATE / 1000
                );
                self.transfer = Some(transfer);
                true
            }
            Err((_err, rx, buf)) => {
                self.rx = Some(rx);
                self.rx_buf = Some(buf);
                println!("[AudioIn] PDM 采集启动失败");
                false
            }
        }
    }

    /// 停止采集（对应 C++ AudioIn::end / pauseMic）
    pub fn stop(&mut self) {
        if let Some(t) = self.transfer.take() {
            let (rx, buf) = t.stop();
            self.rx = Some(rx);
            self.rx_buf = Some(buf);
            println!("[AudioIn] 采集已停止");
        }
    }

    pub fn is_running(&self) -> bool {
        self.transfer.is_some()
    }

    /// 读取当前可用的音频样本（已应用 <<MIC_GAIN_SHIFT 增益并限幅）。
    /// 非阻塞：返回实际读到的样本数（≤ out.len()）；无数据返回 0。
    pub fn read(&mut self, out: &mut [i16]) -> usize {
        if out.is_empty() {
            return 0;
        }
        let Some(t) = &mut self.transfer else {
            return 0;
        };
        let avail = t.available_bytes();
        if avail == 0 {
            return 0;
        }
        // 按字节读，样本数对齐 2 字节；单次最多 out.len() 个样本
        let want_bytes = (avail.min(out.len() * 2).min(self.scratch.capacity())) & !1;
        if want_bytes == 0 {
            return 0;
        }
        self.scratch.clear();
        self.scratch
            .resize(want_bytes, 0)
            .expect("scratch 容量固定");
        let got = t.pop(&mut self.scratch);
        let n = got / 2;

        // 首块原始值打印（bring-up 对账用，对应 C++ 的 CHK-dev 日志）
        if !self.first_printed && n >= 4 {
            self.first_printed = true;
            println!(
                "[AudioIn][CHK] 前4原始样本: {} {} {} {}",
                i16::from_le_bytes([self.scratch[0], self.scratch[1]]),
                i16::from_le_bytes([self.scratch[2], self.scratch[3]]),
                i16::from_le_bytes([self.scratch[4], self.scratch[5]]),
                i16::from_le_bytes([self.scratch[6], self.scratch[7]]),
            );
        }

        let shift = config::MIC_GAIN_SHIFT as i32;
        for i in 0..n {
            let raw = i16::from_le_bytes([self.scratch[i * 2], self.scratch[i * 2 + 1]]) as i32;
            let v = (raw << shift).clamp(-32767, 32767); // C++ 同款：±INT16_MAX 限幅
            out[i] = v as i16;
        }
        n
    }
}
