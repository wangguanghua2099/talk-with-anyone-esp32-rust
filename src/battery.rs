// battery.rs —— 电池电压采样（ADC2_CH6 = GPIO17）+ 充电检测（GPIO38）
// 对应 C++ display_ui.cpp 的 pollBattery()
//
// 关于"ADC2 与 WiFi 冲突"：那是**经典 ESP32** 的限制，S3 上两者可以共存。
// 依据（本机可查）：
//   · IDF adc.h 的 "If Wi-Fi is started … always fail with ESP_ERR_TIMEOUT" 写在
//     `@note ESP32:` 段落下；S2/S3 段落写的是硬件仲裁器（Wi-Fi > RTC > Digital），
//     即读得到，只是被射频抢占的那一拍数据无效。
//   · esp-hal 1.2 里 ADC2 与 radio 的互斥锁（try_claim_adc2 / ADC2_IN_USE）全部带
//     #[cfg(esp32)]，S3 走的是 xtensa.rs 这条无门禁的路径。
// C++ 版在 WiFi 已连接时能打印出 level，也实测印证了这一点。
//
// 因此这里照常读，只针对"那一拍无效数据"加固：5 次采样取中位数
// （C++ 是 2 次平均，抗抢占能力弱），并给每次转换加自旋上限——
// esp-hal 的 read_blocking 是 `while !is_done() {}` 死等，射频长期占用会冻住主循环。

use esp_hal::analog::adc::{Adc, AdcConfig, AdcPin, Attenuation};
use esp_hal::gpio::Input;
use esp_hal::peripherals::{ADC2, GPIO17};
use esp_hal::Blocking;
use esp_println::println;

use crate::config;

/// 卖家源码标定的 ADC 原始值 → 电量百分比（12bit 原始值 / 11dB 衰减，与 C++ L[] 表一致）
const CAL: [(u16, i16); 6] = [
    (1970, 0),
    (2062, 20),
    (2154, 40),
    (2246, 60),
    (2338, 80),
    (2430, 100),
];
/// 单拍采样次数（取中位数）
const SAMPLES: usize = 5;
/// 单次转换的自旋上限：正常几微秒就完成，卡到这里说明被射频长期抢占，放弃本轮
const SPIN_LIMIT: u32 = 200_000;

pub struct Battery<'d> {
    adc: Adc<'d, ADC2<'d>, Blocking>,
    pin: AdcPin<GPIO17<'d>, ADC2<'d>>,
    chg: Input<'d>,
    /// None = 还没读到过有效值（顶栏显示 --%）
    level: Option<u8>,
    charging: bool,
    last_poll_ms: u32,
}

impl<'d> Battery<'d> {
    /// chg 由调用方配好（C++ 是 `pinMode(INPUT)`，即无上下拉）
    pub fn new(adc2: ADC2<'d>, bat_pin: GPIO17<'d>, chg: Input<'d>) -> Self {
        let mut cfg = AdcConfig::new();
        // 11dB 衰减：与 C++ adc2_config_channel_atten(ADC2_CHANNEL_6, ADC_ATTEN_DB_11)
        // 对齐，标定表才是同一套量程
        let pin = cfg.enable_pin(bat_pin, Attenuation::_11dB);
        Self {
            adc: Adc::new(adc2, cfg),
            pin,
            chg,
            level: None,
            charging: false,
            // 让首次 tick 立刻采一次，不必干等 30 秒
            last_poll_ms: 1u32.wrapping_sub(config::BAT_POLL_MS),
        }
    }

    /// 主循环每圈调用；距上次采样满 BAT_POLL_MS 才真正动 ADC。
    /// ADC 转换本身只占几十微秒，但会阻塞主循环，所以刻意保持 30s 低频。
    pub fn tick(&mut self, now_ms: u32) {
        if now_ms.wrapping_sub(self.last_poll_ms) < config::BAT_POLL_MS {
            return;
        }
        self.last_poll_ms = now_ms;

        let Some(raw) = self.read_raw() else {
            println!("[电量] ADC 读取超时（ADC2 被射频长期占用），保持上次读数");
            return;
        };
        let lvl = raw_to_level(raw);
        // 与 C++ 同：满电时不画充电标志
        let chg = self.chg.is_high() && lvl < 100;
        self.level = Some(lvl);
        self.charging = chg;
        println!(
            "[电量] adc={} level={} charging={}",
            raw,
            lvl,
            chg as u8
        );
    }

    pub fn level(&self) -> Option<u8> {
        self.level
    }

    pub fn charging(&self) -> bool {
        self.charging
    }

    fn read_raw(&mut self) -> Option<u16> {
        let mut v = [0u16; SAMPLES];
        for slot in v.iter_mut() {
            *slot = self.read_once()?;
        }
        v.sort_unstable();
        Some(v[SAMPLES / 2])
    }

    fn read_once(&mut self) -> Option<u16> {
        for _ in 0..SPIN_LIMIT {
            match self.adc.read_oneshot(&mut self.pin) {
                Ok(val) => return Some(val),
                // WouldBlock / Other：继续自旋，由 SPIN_LIMIT 兜住
                Err(_) => core::hint::spin_loop(),
            }
        }
        None
    }
}

/// 标定表分段线性插值（对应 C++ pollBattery 的 L[] 查表）
fn raw_to_level(raw: u16) -> u8 {
    let last = CAL.len() - 1;
    if raw <= CAL[0].0 {
        return CAL[0].1 as u8;
    }
    if raw >= CAL[last].0 {
        return CAL[last].1 as u8;
    }
    let mut i = 0;
    while i + 1 < last && raw >= CAL[i + 1].0 {
        i += 1;
    }
    let (a, b) = (CAL[i], CAL[i + 1]);
    (a.1 as i32 + (raw - a.0) as i32 * (b.1 - a.1) as i32 / (b.0 - a.0) as i32) as u8
}
