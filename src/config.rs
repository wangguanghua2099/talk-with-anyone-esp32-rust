// config.rs —— 板级引脚与连接配置（对应 C++ 版 config.h）
// WiFi 密码、服务器 IP 等私密信息在 config_private.rs 中配置。

// 引入私密配置
pub use crate::config_private::*;

// ===== WebSocket 路径 =====
pub const WS_VOICE_PATH: &str = "/ws/voice";
pub const WS_TTS_PATH: &str = "/ws/tts-stream";

// ===== 音频参数（与服务端约定一致，勿改）=====
pub const MIC_SAMPLE_RATE: u32 = 16000; // 上行 PCM16 单声道
pub const SPK_SAMPLE_RATE: u32 = 16000; // 下行（鹿小班同款 16k；服务端 audio.start 会覆盖）

// 麦克风：PDM 数字麦！GPIO2=CLK, GPIO3=DATA
pub const PIN_MIC_PDM_CLK: u8 = 2;
pub const PIN_MIC_PDM_DIN: u8 = 3;
/// PDM 解调后软件左移增益（卖家源码同款：<<5 约 32 倍）
pub const MIC_GAIN_SHIFT: u8 = 5;

// 功放 NS4168：标准 I2S，DOUT=7, BCLK=15, LRCK=16
pub const PIN_SPK_DOUT: u8 = 7;
pub const PIN_SPK_BCLK: u8 = 15;
pub const PIN_SPK_LRCK: u8 = 16;

// ===== 按键（BOOT=对话/打断，音量+/−）=====
pub const PIN_BTN_TALK: u8 = 0;
pub const PIN_BTN_VOLUP: u8 = 39;
pub const PIN_BTN_VOLDN: u8 = 40;

// ===== 屏幕（ST7789 240x240 SPI）=====
pub const DISPLAY_SDA: u8 = 10; // MOSI
pub const DISPLAY_SCL: u8 = 9; // SCLK
pub const DISPLAY_DC: u8 = 8;
pub const DISPLAY_CS: u8 = 14;
pub const DISPLAY_RES: u8 = 18;
pub const DISPLAY_BACKLIGHT: u8 = 13;

// ===== 电源 =====
/// 充电检测引脚：高电平=充电中（电池电压走 ADC2_CH6 = GPIO17）
pub const PIN_BAT_CHG: u8 = 38;

// ===== 音质处理（对应 C++ audio_out.cpp）=====
/// 输出预增益：音量50%≈原来100%的响度
pub const SPK_INPUT_GAIN: f32 = 4.0;
/// 软限幅拐点（v 域），峰值压缩防破音，渐近上限 2×该值=32000
pub const SPK_SOFTCLIP_K: f32 = 16000.0;

// ===== 顶栏时钟（SNTP）=====
/// NTP 服务器 IP 直连列表（按顺序尝试）。刻意不走 DNS：少一个失败点，
/// 局域网设备上 DNS 可用性比这几个 IP 更不稳定。
pub const NTP_SERVERS: &[core::net::Ipv4Addr] = &[
    // ntp.aliyun.com
    core::net::Ipv4Addr::new(120, 25, 115, 20),
    // cn.pool.ntp.org
    core::net::Ipv4Addr::new(119, 28, 183, 184),
    // time.google.com
    core::net::Ipv4Addr::new(216, 239, 35, 8),
];
/// 重新校准间隔（秒）
pub const NTP_RESYNC_SECS: u64 = 12 * 3600;
/// 时区偏移（小时）。C++ 用 configTime 的 UTC+8，这里保持一致
pub const TZ_OFFSET_HOURS: i32 = 8;

// ===== 行为参数 =====
pub const AUTO_LISTEN_ON_BOOT: bool = true;
/// 每次发送 512 采样 = 1024 字节 (~32ms)
pub const RECORD_CHUNK_SAMPLES: usize = 512;
/// 松开后补发静音，让服务端 VAD 判定语句结束
pub const RELEASE_SILENCE_MS: u32 = 700;
/// 缓冲到该水量才开始播放，降低卡顿（32KB ≈ 0.5s@64KB/s）
pub const PLAY_WATERMARK_BYTES: usize = 32 * 1024;
/// 聆听状态无人说话自动退出（3分钟）
pub const LISTEN_TIMEOUT_MS: u32 = 3 * 60 * 1000;
/// 播放结束后静默多久再回到聆听（C++ 为 400ms）
pub const RESUME_LISTEN_SILENCE_MS: u32 = 400;
/// 手动退出聆听/超时退出后的重听冷却
pub const LISTEN_COOLDOWN_MS: u32 = 8000;

// ===== 流式协议参数（交接文档 §3/§4.5）=====
/// 打字机揭示节奏：143ms/单位 ≈ 7 单位/秒，与 TTS 朗读速度对齐
/// （中文按"字"、英文按"词"=单词+紧连标点+词后空格）
pub const TYPE_INTERVAL_MS: u32 = 143;
/// audio.start 未带 sample_rate 时的默认引擎采样率（流式协议主通道）
pub const DEFAULT_STREAM_INPUT_RATE: u32 = 24000;
