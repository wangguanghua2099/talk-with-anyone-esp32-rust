// audio_output.rs —— 环形缓冲 + 线性插值重采样 + 软件音量(平方曲线) + I2S 32bit 输出
// （对应 C++ audio_out.cpp。输出规格与卖家源码一致：32bit、左槽有效、样本<<16、16kHz 锁定）
//
// DMA 策略（重要）：按"块"驱动——每块数据走一次 `I2sTx::write()` + `wait()`，
// 每块都完整重新武装 DMA。这与 C++ `i2s_write`（IDF 驱动每次调用重启动 DMA）
// 的语义一致：队列被抽干后通道停下，下一块到来时自动重启，**不存在挂死**。
// 欠载时 DMA 空闲、I2S FIFO 输出静音（等价 C++ 的 tx_desc_auto_clear）。
//
// 环形缓冲用堆分配：全局堆先注册 64KB 内部 RAM 再注册 PSRAM，3MB 大块自然
// 落入 PSRAM；PSRAM 不可用时逐级回退到更小的内部 RAM 缓冲。
// 注意：3MB 不是 2 的幂，指针运算必须用 (x + cap - y) % cap 形式。

use alloc::vec::Vec;
use esp_hal::{
    dma::DmaTxBuf,
    i2s::master::{I2sTx, I2sTxDmaTransfer},
    time::{Duration, Instant},
    Blocking,
};
use esp_println::println;

use crate::config;

/// 每块样本数：8192B = 2048 样本 = 128ms@64KB/s。
/// 块越大，重新武装 DMA（esp-hal 的 write 每块都会 tx_start/tx_stop 一次外设）
/// 的次数越少；配合"只探测不忙等"的泵（见 tick），重武装延迟约 1~2ms/128ms。
pub const TX_CHUNK_SAMPLES: usize = 2048;
pub const TX_CHUNK_BYTES: usize = TX_CHUNK_SAMPLES * 4;
/// 每个输出样本的字节数（32bit，单有效声道）
const SAMPLE_BYTES: usize = 4;

/// 环形缓冲容量候选：3MB → 1.5MB → 512KB（PSRAM 8MB 下优先 3MB）。
/// 流式回复中服务端按"批"推送音频（一批约 48 字 ≈ 340KB@24k），而播放只消耗
/// 32KB/s：512KB 小环在两批在途时就会溢出丢样，长文本后半段出现跳字卡顿/破音。
/// 3MB ≈ 96 秒音频，可整段吸收 400+ 字回复。
const RING_CAP_CANDIDATES: [usize; 3] = [3 * 1024 * 1024, 1536 * 1024, 512 * 1024];

pub struct AudioOutput {
    // ---- 环形缓冲（mono16 服务端字节序）----
    ring: Vec<u8>,
    head: usize,
    tail: usize,

    // ---- 流状态 ----
    streaming: bool,
    started: bool,
    done_flag: bool,
    in_rate: u32,
    volume: i32,

    // ---- 重采样状态 ----
    r_frac: f32,
    r_prev: i16,

    // ---- I2S（按块驱动：武装一块后只探测完成位，绝不忙等）----
    tx: Option<I2sTx<'static, Blocking>>,
    tx_buf: Option<DmaTxBuf>,
    /// 在途传输：Some 表示 DMA 正把这一块吐出去，tx/tx_buf 暂时被它持有
    xfer: Option<I2sTxDmaTransfer<'static, Blocking, DmaTxBuf>>,

    // ---- 统计 ----
    total_wr: usize,
    last_log: Instant,
}

impl AudioOutput {
    /// 创建输出对象。`tx`/`tx_buf` 由 main 按板级引脚构建后传入；随后调用 [`Self::begin`]。
    pub fn new(tx: I2sTx<'static, Blocking>, tx_buf: DmaTxBuf) -> Self {
        // 环形缓冲：优先 3MB（落 PSRAM），失败逐级回退内部 RAM
        let mut ring = Vec::new();
        let mut chosen = 0usize;
        for &cap in RING_CAP_CANDIDATES.iter() {
            if ring.try_reserve_exact(cap).is_ok() {
                chosen = cap;
                break;
            }
        }
        if chosen == 0 {
            println!("[AudioOut] 环形缓冲分配失败！");
        } else {
            ring.resize(chosen, 0);
            // 与 C++ 版同名观测点（交接文档 §6）：确认 3MB 大环生效
            println!("[AudioOut] 环形缓冲 {}KB", chosen / 1024);
        }

        Self {
            ring,
            head: 0,
            tail: 0,
            streaming: false,
            started: false,
            done_flag: false,
            in_rate: 24000,
            volume: 80,
            r_frac: 0.0,
            r_prev: 0,
            tx: Some(tx),
            tx_buf: Some(tx_buf),
            xfer: None,
            total_wr: 0,
            last_log: Instant::now(),
        }
    }

    /// 初始化检查（对应 C++ begin 的成功路径）。DMA 在首个数据块时启动。
    pub fn begin(&mut self) -> bool {
        let ok = !self.ring.is_empty() && self.tx_buf.is_some();
        if ok {
            println!("[AudioOut] I2S1 扬声器就绪 (16k/32bit/mono)");
        }
        ok
    }

    // ---------- 环形缓冲 ----------

    fn cap(&self) -> usize {
        self.ring.len()
    }

    pub fn buffered(&self) -> usize {
        (self.head + self.cap() - self.tail) % self.cap()
    }

    fn space(&self) -> usize {
        (self.tail + 2 * self.cap() - self.head - 1) % self.cap()
    }

    // ---------- 音量 ----------

    pub fn set_volume(&mut self, v: i32) {
        self.volume = v.clamp(0, 100);
    }

    pub fn get_volume(&self) -> i32 {
        self.volume
    }

    // ---------- 流式接口 ----------

    /// 服务端 audio.start：开始一条新音频流（input_rate 由服务端指定）
    pub fn begin_stream(&mut self, input_rate: u32) {
        // 上一轮若还有在途块，先停掉：否则清空缓冲后它仍会把旧音频的尾巴播完
        self.reclaim();
        self.in_rate = if (8000..=48000).contains(&input_rate) {
            input_rate
        } else {
            24000
        };
        self.head = 0;
        self.tail = 0;
        self.r_frac = 0.0;
        self.r_prev = 0;
        self.done_flag = false;
        self.started = false;
        self.streaming = true;
        println!(
            "[AudioOut] 新音频流 输入{}Hz→输出{}k",
            self.in_rate,
            config::SPK_SAMPLE_RATE / 1000
        );
    }

    /// 服务端 audio.done
    pub fn finish_stream(&mut self) {
        self.done_flag = true;
    }

    /// 打断播放：立刻停在途块（最长 128ms 已武装的音频不能继续放）+ 清空缓冲
    pub fn stop_playback(&mut self) {
        self.reclaim();
        self.streaming = false;
        self.started = false;
        self.done_flag = false;
        self.head = 0;
        self.tail = 0;
        self.r_frac = 0.0;
        self.r_prev = 0;
    }

    pub fn is_active(&self) -> bool {
        self.streaming || self.buffered() > 0
    }

    /// 追加服务端 mono16 数据；采样率与输出一致时直通，否则线性插值重采样。
    /// 返回写入环形缓冲的字节数。行为与 C++ feed() 一致：缓冲满则放弃本块剩余。
    pub fn feed(&mut self, mono16: &[u8]) -> usize {
        if self.ring.is_empty() || mono16.is_empty() || !self.streaming {
            return 0;
        }
        if self.in_rate == config::SPK_SAMPLE_RATE {
            let space = self.space();
            let n = mono16.len().min(space);
            let cap = self.cap();
            for i in 0..n {
                self.ring[self.head] = mono16[i];
                self.head = (self.head + 1) % cap;
            }
            return n;
        }

        // 线性插值重采样：in_rate → 16k
        let ratio = self.in_rate as f32 / config::SPK_SAMPLE_RATE as f32;
        let n_in = mono16.len() / 2;
        let cap = self.cap();
        for i in 0..n_in {
            let s_cur = i16::from_le_bytes([mono16[i * 2], mono16[i * 2 + 1]]);
            let mut t = self.r_frac;
            let mut full = false;
            while t < 1.0 {
                let out =
                    (self.r_prev as f32 + (s_cur as f32 - self.r_prev as f32) * t) as i32 as i16;
                if self.space() < 2 {
                    full = true;
                    break;
                }
                let b = out.to_le_bytes();
                self.ring[self.head] = b[0];
                self.ring[(self.head + 1) % cap] = b[1];
                self.head = (self.head + 2) % cap;
                t += ratio;
            }
            self.r_frac = if t >= 1.0 { t - 1.0 } else { t };
            self.r_prev = s_cur;
            if full {
                break; // 满则放弃剩余（正常不发生）
            }
        }
        n_in * 2
    }

    // ---------- 主循环泵 ----------

    /// 主循环调用（对应 C++ AudioOut::loop）：水位线起播、按块武装 DMA、收尾判定。
    ///
    /// **只探测、不忙等**：一块武装出去后就把它存在 `xfer` 里，之后每圈只读一次
    /// 硬件完成位就立即返回。esp-hal 阻塞模式的 `wait()` 是 `while !is_done() {}`
    /// 死循环，在网络任务同核的情况下会把它活活饿死（实测音频喂不进来、
    /// 环形缓冲抽干、播放停在开头）。让出 CPU 后由主循环 1ms 节拍轮询。
    /// 返回 true = 本次武装了一块（调用方据此决定要不要睡）。
    pub fn tick(&mut self) -> bool {
        if self.ring.is_empty() {
            return false;
        }

        // 1. 在途传输：没吐完就直接返回（这一步是整个防卡顿的关键）
        if let Some(tr) = self.xfer.take() {
            if !tr.is_done() {
                self.xfer = Some(tr);
                return false;
            }
            // 已完成：wait() 不会再转圈，收回外设与缓冲
            let (_res, tx, buf) = tr.wait();
            self.tx = Some(tx);
            self.tx_buf = Some(buf);
        }

        // 2. 水位线起播：缓冲足够（或服务端已收尾）才开始驱动 DMA
        if self.streaming
            && !self.started
            && (self.done_flag || self.buffered() >= config::PLAY_WATERMARK_BYTES)
        {
            self.started = true;
            println!("[AudioOut] 开始播放");
        }
        if !self.started {
            return false;
        }
        let Some(tx) = self.tx.take() else { return false };
        let Some(mut buf) = self.tx_buf.take() else {
            self.tx = Some(tx);
            return false;
        };

        // 3. 填一块：欠载时填**整块静音**维持 DMA。esp-hal 没有 cyclic 通道，
        //    不推的话外设停下后 I2S 会把 FIFO 残留播出来 —— 就是那个"滋滋声"。
        let avail = self.buffered();
        let starved_done = avail == 0 && self.streaming && self.done_flag;
        let written;
        let dst = buf.as_mut_slice();
        if avail == 0 {
            dst.fill(0);
            written = 0;
        } else {
            written = fill_from_ring(&self.ring, self.head, &mut self.tail, self.volume, dst);
            // 尾块不足部分补静音（write 发整块）
            dst[written..].fill(0);
        }

        match tx.write(buf) {
            Ok(tr) => self.xfer = Some(tr),
            Err((e, tx, buf)) => {
                self.tx = Some(tx);
                self.tx_buf = Some(buf);
                println!("[AudioOut] DMA 写入失败 {:?}", e);
                return false;
            }
        }
        self.total_wr += written;

        // 4. 收尾：缓冲排空且服务端已 done → 本轮播完（这块静音吐完后自然静默）
        if starved_done {
            self.started = false;
            self.streaming = false;
            println!("[AudioOut] 播放完成");
        }
        if self.total_wr > 0 && self.last_log.elapsed() >= Duration::from_secs(3) {
            self.last_log = Instant::now();
            println!("[AudioOut] 已输出 {}KB", self.total_wr / 1024);
        }
        true
    }

    /// 停在途传输并立刻收回 DMA 资源（打断 / 提示音前调用，不等待）
    fn reclaim(&mut self) {
        if let Some(tr) = self.xfer.take() {
            let (tx, buf) = tr.stop();
            self.tx = Some(tx);
            self.tx_buf = Some(buf);
        }
    }

    /// 阻塞写一块（仅提示音用：自检/音量键没有并发问题，可以忙等）
    fn write_chunk(&mut self) -> bool {
        self.reclaim();
        let Some(tx) = self.tx.take() else {
            return false;
        };
        let Some(buf) = self.tx_buf.take() else {
            self.tx = Some(tx);
            return false;
        };
        match tx.write(buf) {
            Ok(tr) => {
                let (res, tx, buf) = tr.wait();
                self.tx = Some(tx);
                self.tx_buf = Some(buf);
                res.is_ok()
            }
            Err((_e, tx, buf)) => {
                self.tx = Some(tx);
                self.tx_buf = Some(buf);
                println!("[AudioOut] DMA 写入失败");
                false
            }
        }
    }

    // ---------- 提示音 ----------

    /// 播放提示音（阻塞逐块写出；与 C++ playTone 的 i2s_write 语义一致）。
    pub fn play_tone(&mut self, freq_hz: u32, duration_ms: u32) {
        if self.tx.is_none() || self.tx_buf.is_none() {
            return;
        }
        let rate = config::SPK_SAMPLE_RATE as usize;
        let n = rate * duration_ms as usize / 1000;
        let samples_per_chunk = TX_CHUNK_BYTES / SAMPLE_BYTES;
        let mut i = 0usize;
        while i < n {
            let k = (n - i).min(samples_per_chunk);
            {
                let Some(buf) = &mut self.tx_buf else { return };
                let dst = buf.as_mut_slice();
                for j in 0..k {
                    let idx = i + j;
                    let t = idx as f32 / rate as f32;
                    // ~10ms 淡入淡出防爆音
                    let mut env = 1.0f32;
                    if idx < 160 {
                        env = idx as f32 / 160.0;
                    }
                    if idx > n - 160 {
                        env = (n - idx) as f32 / 160.0;
                    }
                    let smp = libm::sinf(2.0 * core::f32::consts::PI * freq_hz as f32 * t)
                        * 2600.0
                        * env;
                    let y = apply_volume_curve(smp as i16, self.volume);
                    let off = j * SAMPLE_BYTES;
                    dst[off..off + 4].copy_from_slice(&y.to_le_bytes());
                }
                // 尾块不足部分补静音（write 发整块）
                dst[k * SAMPLE_BYTES..].fill(0);
            }
            self.write_chunk();
            i += k;
        }
    }

    /// 开机自检：两短音（对应 C++ selfTest）
    pub fn self_test(&mut self, delay: &esp_hal::delay::Delay) {
        println!("[AudioOut] 扬声器自检：应听到两短音");
        println!("[AudioOut] 自检 1/2 (1200Hz)");
        self.play_tone(1200, 150);
        delay.delay_millis(120);
        println!("[AudioOut] 自检 2/2 (1600Hz)");
        self.play_tone(1600, 150);
        println!("[AudioOut] 自检完毕");
    }
}

/// 音量平方曲线 + 输入预增益 + 软限幅（与 C++ applyVolume 逐行对应）：
/// SPK_INPUT_GAIN 使 50% 音量达到原先 100% 的响度；大信号经软限幅压缩，
/// 增益后峰值渐近 2×SPK_SOFTCLIP_K=32000。
fn apply_volume_curve(v: i16, volume: i32) -> i32 {
    let factor = (volume as f32 / 100.0) * (volume as f32 / 100.0);
    let x = v as f32 * config::SPK_INPUT_GAIN * factor;
    let mut a = x.abs();
    if a > config::SPK_SOFTCLIP_K {
        let k = config::SPK_SOFTCLIP_K;
        a = k + (a - k) * k / a;
    }
    let y = if x < 0.0 { -a } else { a };
    (y as i32) << 16
}

/// 从环形缓冲读最多 out.len() 字节，应用音量并转成 32bit 小端样本。
/// 32bit 样本整样本读取，不足一个样本的字节留在缓冲里。
/// （独立函数以便与 AudioOutput 的字段借用解耦：head 共享、tail 可变）
fn fill_from_ring(
    ring: &[u8],
    head: usize,
    tail: &mut usize,
    volume: i32,
    out: &mut [u8],
) -> usize {
    if ring.is_empty() {
        return 0;
    }
    let cap = ring.len();
    let buffered = (head + cap - *tail) % cap;
    let avail_samples = buffered / 2; // ring 里是 mono16
    let want_samples = out.len() / SAMPLE_BYTES;
    let n = avail_samples.min(want_samples);
    for i in 0..n {
        let lo = ring[*tail];
        let hi = ring[(*tail + 1) % cap];
        *tail = (*tail + 2) % cap;
        let v = i16::from_le_bytes([lo, hi]);
        let y = apply_volume_curve(v, volume);
        out[i * 4..i * 4 + 4].copy_from_slice(&y.to_le_bytes());
    }
    n * SAMPLE_BYTES
}
