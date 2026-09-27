// audio_output.rs —— 环形缓冲 + 线性插值重采样 + 软件音量(平方曲线) + I2S 32bit 连续流输出
// （对应 C++ audio_out.cpp。输出规格与卖家源码一致：32bit、左槽有效、样本<<16、16kHz 锁定）
//
// DMA 策略（v2，重要）：**连续流 TX（DmaTxStreamBuf）**。v1 按"块"驱动——每 128ms
// write()/wait() 一轮，esp-hal 在传输建立与结束（含 Drop）都会 tx_stop/tx_start 外设，
// 块与块之间还有 1~2ms 的重新武装间隙：BCLK/LRCK 停摆、输出保持最后电平，每个块边界
// 一次台阶跳变——听感就是贯穿整段回复的"噗、噗、噗"（2048 样本 ≈ 7.8Hz），叠在人声上
// 即"破音 + 点状杂音"。C++ 版用 IDF legacy 驱动的 8×512 描述符连续环形 DMA
// （tx_desc_auto_clear=true），外设从不重启，所以干净。
//
// v2 语义对齐 C++：DmaTxStreamBuf 的描述符链在推送时自动重链接成环，DMA 全程不停；
// tick() 每次把环形缓冲的数据灌进"已被 DMA 消费"的描述符（数据不足用静音补齐，
// 等价 tx_desc_auto_clear），外设只在打断/收尾时显式停止。is_done() 读 I2S 的
// tx_idle 位——只有描述符链走完（TotalEof：真欠载或自然排空）才会变真。
//
// 环形缓冲用堆分配：全局堆先注册 64KB 内部 RAM 再注册 PSRAM，3MB 大块自然
// 落入 PSRAM；PSRAM 不可用时逐级回退到更小的内部 RAM 缓冲。
// 注意：3MB 不是 2 的幂，指针运算必须用 (x + cap - y) % cap 形式。

use alloc::vec::Vec;
use esp_hal::{
    dma::DmaTxStreamBuf,
    i2s::master::{I2sTx, I2sTxDmaTransfer},
    time::{Duration, Instant},
    Blocking,
};
use esp_println::println;

use crate::config;

/// 流式 TX 缓冲总容量：16KB = 8 描述符 × 2KB ≈ 256ms@64KB/s。
/// 容量决定欠载容忍度（服务端分批推送的批间抖动被它吸收）；播放延迟只由
/// "已推送量"决定，与总容量无关。
pub const TX_STREAM_BYTES: usize = 16 * 1024;
/// 描述符块大小（≤4095；宏按它均分缓冲）。2KB = 32ms@64KB/s，推送粒度足够细。
pub const TX_STREAM_CHUNK: usize = 2048;
/// 每个输出样本的字节数（32bit，单有效声道）
const SAMPLE_BYTES: usize = 4;
/// 启动/重启前至少预填的量（esp-hal 要求至少填满 2 个描述符）
const MIN_PREFILL_BYTES: usize = 2 * TX_STREAM_CHUNK;

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

    // ---- I2S（连续流：在途传输经 View 持续推送，绝不停启外设）----
    tx: Option<I2sTx<'static, Blocking>>,
    /// 空闲的流缓冲。用过的缓冲带着残留数据/视图状态，启动前必须经
    /// [`Self::fresh_stream`] 重置。
    stream: Option<DmaTxStreamBuf>,
    /// 在途流传输：Some 期间 DMA 连续消费描述符，新数据经 DerefMut 的 View 推送
    xfer: Option<I2sTxDmaTransfer<'static, Blocking, DmaTxStreamBuf>>,

    // ---- 统计 ----
    total_wr: usize,
    last_log: Instant,
    underruns: u32,
}

impl AudioOutput {
    /// 创建输出对象。`tx`/`stream` 由 main 按板级引脚构建后传入；随后调用 [`Self::begin`]。
    pub fn new(tx: I2sTx<'static, Blocking>, stream: DmaTxStreamBuf) -> Self {
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
            stream: Some(stream),
            xfer: None,
            total_wr: 0,
            last_log: Instant::now(),
            underruns: 0,
        }
    }

    /// 初始化检查（对应 C++ begin 的成功路径）。DMA 在首个数据块时启动。
    pub fn begin(&mut self) -> bool {
        let ok = !self.ring.is_empty() && self.stream.is_some();
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
        // 上一轮若还有在途数据，先停掉：否则清空缓冲后它仍会把旧音频的尾巴播完
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

    /// 打断播放：立刻停在途传输 + 清空缓冲（传输被 tx_stop 硬停，无尾巴）
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

    /// 主循环调用（对应 C++ AudioOut::loop）：水位线起播、向在途流持续推送、收尾判定。
    ///
    /// **只推送、不停启外设**：流一旦启动，tick 只负责"把环形缓冲的数据（不足则补
    /// 静音）灌进已被 DMA 消费的描述符"。主循环 1ms 节拍下，每拍可推的量远大于
    /// 64B/ms 的消耗速率，描述符链永远不会被 DMA 走完 —— 外设从起播到收尾零重启，
    /// 块边界跳变（v1 的"噗"声源）不复存在。esp-hal 阻塞模式的 `wait()` 是
    /// `while !is_done() {}`，绝不能在等待数据时调用（会饿死同执行器的网络任务）。
    ///
    /// 返回 true = 本次**启动**了一个新的流传输（启动是低频事件；常规推送返回
    /// false，调用方照常 1ms 节拍睡眠，网络任务得以轮转）。
    pub fn tick(&mut self) -> bool {
        if self.ring.is_empty() && self.xfer.is_none() {
            return false;
        }

        // 1. 在途流：推送新数据 / 检测排空
        if let Some(mut tr) = self.xfer.take() {
            if tr.is_done() {
                // 描述符链走完（TotalEof）：正常排空，或 tick 被饿过头的真欠载
                let (_res, tx, stream) = tr.wait();
                self.tx = Some(tx);
                self.stream = Some(stream);

                if self.streaming && self.done_flag && self.buffered() == 0 {
                    // 音频全部播完 + 服务端已收尾 → 本轮结束（此刻电平已在静音尾上）
                    self.started = false;
                    self.streaming = false;
                    println!("[AudioOut] 播放完成");
                    return false;
                }
                // 真欠载：统计并立即重启（下方预填，静音续播）
                self.underruns += 1;
                if self.underruns <= 3 || self.underruns % 50 == 0 {
                    println!("[AudioOut] 欠载 #{}（静音续播）", self.underruns);
                }
                // fall through 到 3) 预填重启
            } else {
                // 在途且未排空：持续推送
                let head = self.head;
                let volume = self.volume;
                // 收尾排空阶段（服务端已收尾且缓冲已空）不再推进，让 DMA 走到链尾
                let keep = self.streaming && !(self.done_flag && self.buffered() == 0);
                let mut audio = 0usize;
                if keep {
                    let view = &mut *tr; // DerefMut → DmaTxStreamBufView
                    if view.available_bytes() > 0 {
                        let _ = view.push_with(|dst| {
                            let n = fill_from_ring(&self.ring, head, &mut self.tail, volume, dst);
                            dst[n..].fill(0); // 不足部分静音，维持 DMA 连续
                            audio = n;
                            dst.len()
                        });
                    }
                }
                self.xfer = Some(tr);
                if audio > 0 {
                    self.total_wr += audio;
                    self.maybe_log();
                }
                return false;
            }
        }

        // 2. 水位线起播：缓冲足够（或服务端已收尾）才开始驱动 DMA
        if !self.streaming {
            return false;
        }
        if !self.started
            && (self.done_flag || self.buffered() >= config::PLAY_WATERMARK_BYTES)
        {
            self.started = true;
            println!("[AudioOut] 开始播放");
        }
        if !self.started {
            return false;
        }

        // 3. 预填 + 启动连续流（每次启动都用重置后的全新流缓冲）
        let Some(tx) = self.tx.take() else {
            return false;
        };
        let Some(stream) = self.stream.take() else {
            self.tx = Some(tx);
            return false;
        };
        let mut stream = Self::fresh_stream(stream);
        let head = self.head;
        let volume = self.volume;
        let mut audio = 0usize;
        stream.push_with(|dst| {
            let n = fill_from_ring(&self.ring, head, &mut self.tail, volume, dst);
            dst[n..].fill(0); // 尾部静音补齐：DMA 一次性拿到最多 16KB 的余量
            audio = n;
            dst.len()
        });
        if audio > 0 {
            self.total_wr += audio;
            self.maybe_log();
        }

        match tx.write(stream) {
            Ok(tr) => self.xfer = Some(tr),
            Err((e, tx, st)) => {
                self.tx = Some(tx);
                self.stream = Some(st);
                println!("[AudioOut] DMA 启动失败 {:?}", e);
                return false;
            }
        }
        true
    }

    /// 停止在途传输并收回资源（打断 / 提示音前调用，不等待）。
    /// 流缓冲以"脏"状态收回，启动前经 [`Self::fresh_stream`] 重置。
    fn reclaim(&mut self) {
        if let Some(tr) = self.xfer.take() {
            let (tx, stream) = tr.stop();
            self.tx = Some(tx);
            self.stream = Some(stream);
        }
    }

    /// 把用过的流缓冲重置为全新空态（丢弃残留数据与视图状态）。
    /// DmaTxStreamBuf 没有公开的 reset；split + new 用同一组静态切片原地重建。
    fn fresh_stream(stream: DmaTxStreamBuf) -> DmaTxStreamBuf {
        let (descs, buf) = stream.split();
        // 同一组切片、同参数重建必然成功（约束在首次构建时已验证）
        DmaTxStreamBuf::new(descs, buf).expect("流缓冲重建失败（同参数重建必成功）")
    }

    fn maybe_log(&mut self) {
        if self.total_wr > 0 && self.last_log.elapsed() >= Duration::from_secs(3) {
            self.last_log = Instant::now();
            println!("[AudioOut] 已输出 {}KB", self.total_wr / 1024);
        }
    }

    // ---------- 提示音 ----------

    /// 播放提示音。150ms 的音（2400 样本 = 9.6KB）整段塞进 16KB 流缓冲一次播出，
    /// 中间无任何 stop/start；更长的音分多轮，轮间有一次链尾重启（听感为音节间隔）。
    pub fn play_tone(&mut self, freq_hz: u32, duration_ms: u32) {
        if self.tx.is_none() || self.stream.is_none() {
            return;
        }
        self.reclaim(); // 与原 write_chunk 行为一致：先停掉在途播放
        let rate = config::SPK_SAMPLE_RATE as usize;
        let n = rate * duration_ms as usize / 1000;
        let volume = self.volume;
        let mut i = 0usize;
        while i < n {
            let Some(tx) = self.tx.take() else {
                return;
            };
            let Some(stream) = self.stream.take() else {
                self.tx = Some(tx);
                return;
            };
            let mut stream = Self::fresh_stream(stream);
            let bytes = stream.push_with(|dst| {
                let k = dst.len() / SAMPLE_BYTES;
                let mut w = 0usize;
                for j in 0..k {
                    let idx = i + j;
                    if idx >= n {
                        break;
                    }
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
                    let y = apply_volume_curve(smp as i16, volume);
                    dst[w * SAMPLE_BYTES..w * SAMPLE_BYTES + 4]
                        .copy_from_slice(&y.to_le_bytes());
                    w += 1;
                }
                w * SAMPLE_BYTES
            });
            let made = bytes / SAMPLE_BYTES;
            if made == 0 {
                // 保险：全新缓冲必有空间，不该发生；还回资源避免卡死
                self.tx = Some(tx);
                self.stream = Some(stream);
                break;
            }
            match tx.write(stream) {
                Ok(tr) => {
                    // 忙等排空（提示音场景无并发，与原 write_chunk 的 wait 等价）
                    let (_res, tx, stream) = tr.wait();
                    self.tx = Some(tx);
                    self.stream = Some(stream);
                }
                Err((_e, tx, st)) => {
                    self.tx = Some(tx);
                    self.stream = Some(st);
                    println!("[AudioOut] DMA 写入失败");
                    return;
                }
            }
            i += made;
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
