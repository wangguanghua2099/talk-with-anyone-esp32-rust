// main.rs —— Talk With Anyone ESP32 前端（Rust 版）
//
// 架构（esp-rtos 调度器 + embassy 异步任务）：
//   · wifi_conn 任务：WiFi 连接/断线重连
//   · net_task 任务：embassy-net 网络栈驱动
//   · voice_task 任务：WS 客户端——收服务端事件（RoundTracker 守卫）、
//     麦克风帧上行、interrupt 发送、client_stats 上报、tts 兜底
//   · main：三态状态机（IDLE/LISTENING/PLAYING）+ 音频泵 + 按键 + 显示 tick
//
// 任务间通信：
//   · MIC_CH 队列：主循环读麦克风 → voice_task 发上行帧（32ms/块）
//   · INT_REQ 信号：主循环按 BOOT 打断 → voice_task 发 interrupt
//   · WS_UP / MIC_ACTIVE 原子量：连接与收音状态（free-read）
//   · AudioOutput/AudioInput/Display 放互斥锁里，voice_task 与主循环共用
#![no_std]
#![no_main]
// 渐进式移植：display 渲染层等先就位待接线，接线完成后移除本豁免
#![allow(dead_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use embassy_executor::Spawner;
use embassy_net::{Runner, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Input, InputConfig, Pull},
    i2s::master::{Channels, DataFormat, I2s, PdmConfig, PdmRxConfig, PdmSlotMode, TdmConfig},
    main,
    rng::Rng,
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_println::println;

mod audio_input;
mod audio_output;
mod config;
mod config_private;
mod display;
mod font;
mod ntp;
mod render;
mod screen;
mod voice_client;
mod ws;

use audio_input::AudioInput;
use audio_output::AudioOutput;
use display::{Display, DisplayState};
use embassy_time::Timer;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::time::Instant as HalInstant;
use render::Renderer;
use screen::Screen;
use ws::WsClient;

// ESP-IDF 应用描述符：espflash 4.x 烧录校验必需
esp_bootloader_esp_idf::esp_app_desc!();

// ---------- 任务间共享 ----------

/// WS 主通道是否已连接（voice_task 写，主循环读）
static WS_UP: AtomicBool = AtomicBool::new(false);
/// 麦克风是否应采上行（协议事件与状态机共同决定）
static MIC_ACTIVE: AtomicBool = AtomicBool::new(false);
/// 最近一次 VAD 说话事件的毫秒时标（0 = 从未收到）
static LAST_VAD_MS: AtomicU32 = AtomicU32::new(0);
/// BOOT 打断请求（主循环 → voice_task）
static INT_REQ: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// 麦克风上行块：512 采样 = 1024B（~32ms），与 C++ RECORD_CHUNK_SAMPLES 对齐
#[derive(Clone, Copy)]
struct MicChunk {
    len: usize,
    data: [u8; 1024],
}
static MIC_CH: Channel<CriticalSectionRawMutex, MicChunk, 6> = Channel::new();

type Shared<T> = Mutex<CriticalSectionRawMutex, T>;

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

/// 状态机三态（对应 C++ ST_IDLE/ST_LISTENING/ST_PLAYING）
#[derive(Clone, Copy, PartialEq)]
enum AppState {
    Idle,
    Listening,
    Playing,
}

#[main]
async fn main(spawner: Spawner) -> ! {
    // 堆：内部 RAM 128KB（WiFi 栈 + 字符串）先注册，PSRAM 兜底大块（3MB 环形缓冲）
    esp_alloc::heap_allocator!(size: 128 * 1024);

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    println!("\n=== TWA · 鹿小班三代 ESP32 前端 (Rust) ===");
    println!(
        "WiFi: {}  Server: ws://{}:{}",
        config::WIFI_SSID,
        config::SERVER_HOST,
        config::SERVER_PORT
    );
    println!("{}", esp_alloc::HEAP.stats());

    // 调度器（esp-radio 依赖它跑内部任务）
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let delay = Delay::new();

    // ---- 扬声器：I2S1 TDM Philips 32bit 左槽 16k（DMA_CH1）----
    let tx_buf = match esp_hal::dma_tx_buffer!(audio_output::TX_CHUNK_BYTES) {
        Ok(b) => b,
        Err(e) => fail(&delay, "TX DMA 缓冲创建失败", &e),
    };
    let i2s_out = match I2s::new(
        peripherals.I2S1,
        peripherals.DMA_CH1,
        TdmConfig::new_tdm_philips()
            .with_sample_rate(Rate::from_hz(config::SPK_SAMPLE_RATE))
            .with_data_format(DataFormat::Data32Channel32)
            .with_channels(Channels::LEFT),
    ) {
        Ok(i) => i,
        Err(e) => fail(&delay, "I2S1 初始化失败", &e),
    };
    let spk_tx = i2s_out
        .i2s_tx
        .with_bclk(peripherals.GPIO15)
        .with_ws(peripherals.GPIO16)
        .with_dout(peripherals.GPIO7)
        .build();
    let mut audio_out = AudioOutput::new(spk_tx, tx_buf);

    // ---- 麦克风：I2S0 PDM RX 16k 单声道（DMA_CH0，CLK=IO2 DATA=IO3）----
    let rx_buf = esp_hal::dma_rx_stream_buffer!(audio_input::RX_QUEUE_BYTES, audio_input::RX_CHUNK_BYTES);
    let i2s_in = match I2s::new_pdm(
        peripherals.I2S0,
        peripherals.DMA_CH0,
        PdmConfig::rx_only(PdmRxConfig::new_pcm_default(
            Rate::from_hz(config::MIC_SAMPLE_RATE),
            PdmSlotMode::Mono,
        )),
    ) {
        Ok(i) => i,
        Err(e) => fail(&delay, "I2S0 PDM 初始化失败", &e),
    };
    let mic_rx = match i2s_in
        .i2s_rx
        .with_clk(peripherals.GPIO2)
        .with_din_line(0, peripherals.GPIO3)
    {
        Ok(r) => r.build(),
        Err(e) => fail(&delay, "PDM 引脚配置失败", &e),
    };
    let mut audio_in = AudioInput::new(mic_rx, rx_buf);

    // ---- 屏幕：ST7789（SPI2+DMA，手工 CS，20MHz）----
    let _backlight = Output::new(peripherals.GPIO13, Level::High, OutputConfig::default());
    // SpiDma 的切片接口需要内部收发缓冲（不注册会静默返回 BufferTooSmall）
    let scr_rxb = match esp_hal::dma_rx_buffer!(4096) {
        Ok(b) => b,
        Err(e) => fail(&delay, "屏幕 RX DMA 缓冲创建失败", &e),
    };
    let scr_txb = match esp_hal::dma_tx_buffer!(4096) {
        Ok(b) => b,
        Err(e) => fail(&delay, "屏幕 TX DMA 缓冲创建失败", &e),
    };
    let mut screen = match Screen::new(
        peripherals.SPI2,
        peripherals.DMA_CH2,
        scr_rxb,
        scr_txb,
        peripherals.GPIO9,
        peripherals.GPIO10,
        peripherals.GPIO14,
        peripherals.GPIO8,
        peripherals.GPIO18,
        &delay,
    ) {
        Some(s) => s,
        None => fail(&delay, "屏幕 SPI 初始化失败", &"spi"),
    };
    screen.self_test_bars();
    let mut renderer = Renderer::new();

    // ---- 音频 bring-up（此时调度器已在跑，短暂阻塞无碍）----
    audio_out.begin();
    audio_out.self_test(&delay);

    // ---- 按键 ----
    let btn_talk = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));
    let btn_volup = Input::new(peripherals.GPIO39, InputConfig::default().with_pull(Pull::Up));
    let btn_voldn = Input::new(peripherals.GPIO40, InputConfig::default().with_pull(Pull::Up));

    // ---- WiFi + 网络栈 ----
    let station_config = esp_radio::wifi::Config::Station(
        esp_radio::wifi::sta::StationConfig::default()
            .with_ssid(config::WIFI_SSID.try_into().unwrap())
            .with_authentication(esp_radio::wifi::AuthenticationMethodConfig::Wpa2Personal(
                config::WIFI_PASSWORD.try_into().unwrap(),
            )),
    );
    let wifi_interface = esp_radio::wifi::Interface::station();
    let controller = esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        esp_radio::wifi::ControllerConfig::default().with_initial_config(station_config),
    )
    .unwrap();
    println!("[WiFi] 已配置，连接中: {}", config::WIFI_SSID);

    let mut rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        embassy_net::Config::dhcpv4(Default::default()),
        // SocketResources 的常量 = SocketSet 容量 = 可同时存在的 socket 数。
        // 现在是 2 个消费者（语音 WS + NTP UDP），而 WsClient::connect 握手失败重试
        // 期间会短暂持有旧 socket；3 曾把 smoltcp 撑爆
        // （panicked at socket_set.rs: adding a socket to a full SocketSet，
        //   整个 embassy 执行器随之停摆，屏幕就冻在最后一次绘制的内容上）。
        mk_static!(StackResources<6>, StackResources::<6>::new()),
        seed,
    );
    let stack = mk_static!(Stack<'static>, stack);

    spawner.spawn(wifi_conn(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    // 顶栏时钟：等 IP 的活儿交给 ntp_task 自己去做。
    // 【务必不要在这里 await 网络】主任务一旦在 `wait_config_up()` 上停住，
    // app_loop 就没起来，渲染层接管不到屏幕 → 设备永远停在开机四条色带上，
    // 看起来像死机，其实是主循环压根没开始跑（DHCP 慢/路由器抽风即可复现）。
    spawner.spawn(ntp::ntp_task(*stack).unwrap());

    // ---- 共享对象入静态 ----
    let audio = mk_static!(
        Shared<core::cell::RefCell<AudioOutput>>,
        Mutex::new(core::cell::RefCell::new(audio_out))
    );
    let audio_in = mk_static!(
        Shared<core::cell::RefCell<AudioInput>>,
        Mutex::new(core::cell::RefCell::new(audio_in))
    );
    let display = mk_static!(
        Shared<core::cell::RefCell<Display>>,
        Mutex::new(core::cell::RefCell::new(Display::new()))
    );
    let payload_buf = mk_static!([u8; WS_PAYLOAD_CAP], [0u8; WS_PAYLOAD_CAP]);
    let ws_rx = mk_static!([u8; 8192], [0u8; 8192]);
    let ws_tx = mk_static!([u8; 4096], [0u8; 4096]);

    // ---- 服务端地址（配置为局域网 IPv4；域名解析留待后续按需加 DNS）----
    let server_ip: Ipv4Addr = match config::SERVER_HOST.parse() {
        Ok(ip) => ip,
        Err(_) => fail(&delay, "SERVER_HOST 不是合法 IPv4（域名解析暂未实现）", &config::SERVER_HOST),
    };

    spawner
        .spawn(voice_task(
            *stack,
            server_ip,
            audio,
            audio_in,
            display,
            payload_buf,
            ws_rx,
            ws_tx,
            rng.random(),
        )
        .unwrap());

    println!("就绪。通道建立后自动进入聆听。");

    // 主循环：三态状态机 + 音频泵 + 按键 + 字幕绘制
    app_loop(
        audio,
        audio_in,
        display,
        btn_talk,
        btn_volup,
        btn_voldn,
        &mut screen,
        &mut renderer,
    )
    .await
}

// ---------- 主循环（状态机，对应 C++ main.cpp loop） ----------

#[allow(clippy::too_many_arguments)]
async fn app_loop(
    audio: &'static Shared<core::cell::RefCell<AudioOutput>>,
    audio_in: &'static Shared<core::cell::RefCell<AudioInput>>,
    display: &'static Shared<core::cell::RefCell<Display>>,
    btn_talk: Input<'static>,
    btn_volup: Input<'static>,
    btn_voldn: Input<'static>,
    screen: &mut Screen,
    renderer: &mut Renderer,
) -> ! {
    use AppState::*;

    let mut state = Idle;
    let mut cooldown_until = 0u32;
    let mut listen_started = 0u32;
    let mut quiet_since: Option<u32> = None;
    let mut prev_talk = false;
    let mut prev_up = false;
    let mut prev_dn = false;
    let mut peak: i32 = 0;
    let mut empty_streak = 0u32;
    let mut peak_win = 0u32;
    let mut pass = 0u32;
    let mut mic_buf = [0i16; config::RECORD_CHUNK_SAMPLES];

    loop {
        let now = HalInstant::now().duration_since_epoch().as_millis() as u32;

        // 音频泵（pumped = 本次是否真的武装了 DMA 块）
        let pumped = audio.lock().await.borrow_mut().tick();
        let ws_up = WS_UP.load(Ordering::Relaxed);

        // 打字机推进 + 字幕/顶栏局部重绘（屏幕只由主循环持有，无需加锁）
        {
            let d = display.lock().await;
            // 顶栏状态字：WS 断开时优先显示 OFFLINE 红字（验收清单 §7.9），
            // 链路恢复后回到状态机当前状态对应的状态字
            let shown = if !ws_up {
                DisplayState::Offline
            } else {
                match state {
                    Idle => DisplayState::Idle,
                    Listening => DisplayState::Listening,
                    Playing => DisplayState::Playing,
                }
            };
            if d.borrow().state() != shown {
                d.borrow_mut().set_state(shown);
            }
            d.borrow_mut().tick(now);
            renderer.tick(&d.borrow(), screen, now);
        }

        let talk = btn_talk.is_low();
        let talk_edge = talk && !prev_talk;
        prev_talk = talk;

        // ---- 状态机 ----
        match state {
            Idle => {
                if config::AUTO_LISTEN_ON_BOOT
                    && ws_up
                    && now > 5000
                    && now.wrapping_sub(cooldown_until) < u32::MAX / 2
                {
                    // 开机/冷却后自动进入聆听
                    MIC_ACTIVE.store(true, Ordering::Relaxed);
                    tracker_mic_start().await;
                    audio_in.lock().await.borrow_mut().start();
                    listen_started = now;
                    state = Listening;
                    display.lock().await.borrow_mut().set_state(DisplayState::Listening);
                    println!("[状态] 聆听中，请说话");
                } else if talk_edge {
                    // C++ 在按下沿消抖后重读；这里用小延时等抖动过去
                    Timer::after_millis(30).await;
                    if btn_talk.is_low() && ws_up {
                        MIC_ACTIVE.store(true, Ordering::Relaxed);
                        tracker_mic_start().await;
                        audio_in.lock().await.borrow_mut().start();
                        listen_started = now;
                        state = Listening;
                        display.lock().await.borrow_mut().set_state(DisplayState::Listening);
                        println!("[状态] 聆听中，请说话");
                    }
                }
            }
            Listening => {
                // 每 16 圈读一次麦克风（~16ms×1ms 节拍 ≈ 32ms 一块，与 C++ 节奏一致）
                if pass % 16 == 0 && MIC_ACTIVE.load(Ordering::Relaxed) {
                    let n = audio_in.lock().await.borrow_mut().read(&mut mic_buf);
                    // PDM 流式 DMA 溢出后不会自愈（实测：主循环被长任务占住一段
                    // 时间后就恒读到 0，表现为"峰值=0、说话没反应"）→ 重新装填
                    if n == 0 {
                        empty_streak += 1;
                        if empty_streak >= 32 {
                            empty_streak = 0;
                            println!("[AudioIn] 连续无数据，重启 PDM DMA");
                            let ai = audio_in.lock().await;
                            ai.borrow_mut().stop();
                            ai.borrow_mut().start();
                        }
                    } else {
                        empty_streak = 0;
                    }
                    if n > 0 {
                        let mut chunk = MicChunk { len: n * 2, data: [0u8; 1024] };
                        for (i, s) in mic_buf.iter().take(n).enumerate() {
                            let b = s.to_le_bytes();
                            chunk.data[i * 2] = b[0];
                            chunk.data[i * 2 + 1] = b[1];
                        }
                        // 队列满 = voice_task 卡顿：丢弃本块（VAD 容忍毫秒级空洞）
                        let _ = MIC_CH.try_send(chunk);
                        for s in mic_buf.iter().take(n) {
                            let a = (*s as i32).abs();
                            if a > peak {
                                peak = a;
                            }
                        }
                    }
                    peak_win += 1;
                    if peak_win >= 2 {
                        peak_win = 0;
                        println!("[麦克风] 峰值={}", peak);
                        peak = 0;
                    }
                }

                // 回复音频到达 → 暂停收音防回声（C++ buffered()>0 → PLAYING）
                if audio.lock().await.borrow_mut().buffered() > 0 {
                    MIC_ACTIVE.store(false, Ordering::Relaxed);
                    tracker_mic_pause().await;
                    audio_in.lock().await.borrow_mut().stop();
                    state = Playing;
                    quiet_since = None;
                    display.lock().await.borrow_mut().set_state(DisplayState::Playing);
                    println!("[状态] AI 回复播放中...（按一下打断）");
                }

                // BOOT：手动退出聆听
                if talk_edge {
                    Timer::after_millis(30).await;
                    if btn_talk.is_low() {
                        MIC_ACTIVE.store(false, Ordering::Relaxed);
                        tracker_mic_pause().await;
                        audio_in.lock().await.borrow_mut().stop();
                        display.lock().await.borrow_mut().flush_typing();
                        state = Idle;
                        cooldown_until = now.wrapping_add(config::LISTEN_COOLDOWN_MS);
                        display.lock().await.borrow_mut().set_state(DisplayState::Idle);
                        println!("[状态] 手动退出聆听");
                    }
                }

                // 聆听超时（无人说话 3 分钟）
                let vad = LAST_VAD_MS.load(Ordering::Relaxed);
                let since_vad = if vad == 0 { u32::MAX } else { now.wrapping_sub(vad) };
                if since_vad > config::LISTEN_TIMEOUT_MS
                    && now.wrapping_sub(listen_started) > config::LISTEN_TIMEOUT_MS
                {
                    MIC_ACTIVE.store(false, Ordering::Relaxed);
                    tracker_mic_pause().await;
                    audio_in.lock().await.borrow_mut().stop();
                    state = Idle;
                    cooldown_until = now.wrapping_add(config::LISTEN_COOLDOWN_MS);
                    display.lock().await.borrow_mut().set_state(DisplayState::Idle);
                    println!("[状态] 聆听超时，待机");
                }
            }
            Playing => {
                // BOOT：打断 → 立即继续聆听
                if talk_edge {
                    Timer::after_millis(30).await;
                    if btn_talk.is_low() {
                        INT_REQ.signal(());
                        display.lock().await.borrow_mut().flush_typing();
                        // 等待松开（C++ 同款）
                        while btn_talk.is_low() {
                            Timer::after_millis(10).await;
                        }
                        MIC_ACTIVE.store(true, Ordering::Relaxed);
                        tracker_mic_start().await;
                        audio_in.lock().await.borrow_mut().start();
                        listen_started = now;
                        state = Listening;
                        display.lock().await.borrow_mut().set_state(DisplayState::Listening);
                        println!("[状态] 已打断，继续聆听");
                    }
                }

                // 播完自动继续聆听（静默 400ms 判定）
                if !audio.lock().await.borrow_mut().is_active() {
                    match quiet_since {
                        None => quiet_since = Some(now),
                        Some(t0) => {
                            if now.wrapping_sub(t0) > config::RESUME_LISTEN_SILENCE_MS {
                                MIC_ACTIVE.store(true, Ordering::Relaxed);
                                tracker_mic_start().await;
                                audio_in.lock().await.borrow_mut().start();
                                listen_started = now;
                                state = Listening;
                                display.lock().await.borrow_mut().set_state(DisplayState::Listening);
                                println!("[状态] 播完，继续聆听");
                            }
                        }
                    }
                } else {
                    quiet_since = None;
                }
            }
        }

        // ---- 音量键（按下沿触发，±10，反馈音）----
        let up = btn_volup.is_low();
        if up && !prev_up {
            Timer::after_millis(20).await;
            if btn_volup.is_low() {
                let v = (audio.lock().await.borrow_mut().get_volume() + 10).min(100);
                audio.lock().await.borrow_mut().set_volume(v);
                // 顶栏音量临时条（1.5s 后自动收起）
                display.lock().await.borrow_mut().set_volume(v, now);
                println!("[音量] {}%", audio.lock().await.borrow_mut().get_volume());
                audio.lock().await.borrow_mut().play_tone(1800, 60);
                // TODO: 音量 NVS 持久化
            }
        }
        prev_up = up;

        let dn = btn_voldn.is_low();
        if dn && !prev_dn {
            Timer::after_millis(20).await;
            if btn_voldn.is_low() {
                let v = (audio.lock().await.borrow_mut().get_volume() - 10).max(0);
                audio.lock().await.borrow_mut().set_volume(v);
                // 顶栏音量临时条（1.5s 后自动收起）
                display.lock().await.borrow_mut().set_volume(v, now);
                println!("[音量] {}%", audio.lock().await.borrow_mut().get_volume());
                audio.lock().await.borrow_mut().play_tone(900, 60);
            }
        }
        prev_dn = dn;

        // ---- 心跳 ----
        pass += 1;
        if pass % 30_000 == 0 {
            println!(
                "[心跳] 运行正常 状态={} WS={} 麦克风={}",
                match state {
                    Idle => "IDLE",
                    Listening => "LISTEN",
                    Playing => "PLAY",
                },
                ws_up,
                MIC_ACTIVE.load(Ordering::Relaxed)
            );
        }

        // 泵过 DMA 就不要再叠 1ms 节拍：write+wait 本身就是 64ms 的节拍，
        // 叠加会让泵速低于 I2S 消耗速、把环形缓冲抽干（下溢爆音）。
        // 没泵数据时才睡，避免空转饿死 voice_task。
        if !pumped {
            Timer::after_millis(1).await;
        }
    }
}

async fn tracker_mic_start() {
    WS_TRACKER.lock().await.borrow_mut().mic_start();
    MIC_ACTIVE.store(true, Ordering::Relaxed);
}
async fn tracker_mic_pause() {
    WS_TRACKER.lock().await.borrow_mut().mic_pause();
    MIC_ACTIVE.store(false, Ordering::Relaxed);
}

// RoundTracker 需要跨主循环与 voice_task 使用（mic_start/pause 与协议事件同源）
static WS_TRACKER: Shared<core::cell::RefCell<voice_client::RoundTracker>> =
    Mutex::new(core::cell::RefCell::new(voice_client::RoundTracker::new()));

// ---------- voice_task：WS 客户端 + 协议事件分发 ----------

/// WS 载荷接收缓冲容量：audio.chunk = 4096 样本@24k → base64 ≈ 10.9KB，取 16KB
const WS_PAYLOAD_CAP: usize = 16 * 1024;

#[embassy_executor::task]
async fn voice_task(
    stack: Stack<'static>,
    server_ip: Ipv4Addr,
    audio: &'static Shared<core::cell::RefCell<AudioOutput>>,
    audio_in: &'static Shared<core::cell::RefCell<AudioInput>>,
    display: &'static Shared<core::cell::RefCell<Display>>,
    payload: &'static mut [u8],
    ws_rx: &'static mut [u8],
    ws_tx: &'static mut [u8],
    mut mask_seed: u32,
) -> ! {
    let path = voice_client::ws_path_with_token(config::WS_VOICE_PATH);

    // 首次连接（缓冲随 WsClient 移交，只此一次；失败则原地重试）
    // 首次连接（缓冲随 WsClient 移交；connect 内部自带重试，永不失败）
    let mut ws = WsClient::connect(
        stack,
        server_ip,
        config::SERVER_PORT,
        &path,
        ws_rx,
        ws_tx,
        mask_seed,
    )
    .await;

    loop {
        // 会话开始：重置轮次状态并发 session.start
        WS_TRACKER.lock().await.borrow_mut().reset();
        let mut out = String::new();
        voice_client::build_session_start(&mut out);
        if ws.send_text(&out).await.is_err() {
            println!("[Voice] session.start 发送失败");
            WS_UP.store(false, Ordering::Relaxed);
        } else {
            println!("[Voice] 已连接 /ws/voice，已发送 session.start");
            WS_UP.store(true, Ordering::Relaxed);

            let mut pcm: Vec<u8> = Vec::new();
            let _ = pcm.try_reserve(8192);

            'session: loop {
                let now = HalInstant::now().duration_since_epoch().as_millis() as u32;

                // 1. BOOT 打断请求：停播放 + 字幕定格 + 发 interrupt（C++ sendInterrupt 三件事）
                if INT_REQ.try_take().is_some() {
                    let flush = WS_TRACKER.lock().await.borrow_mut().finalize_partial();
                    if flush {
                        display.lock().await.borrow_mut().flush_typing();
                    }
                    audio.lock().await.borrow_mut().stop_playback();
                    let mut o = String::new();
                    voice_client::build_interrupt(&mut o);
                    if ws.send_text(&o).await.is_err() {
                        break 'session;
                    }
                    println!("[Voice] 已发送 interrupt");
                }

                // 2. 麦克风上行（队列里的块全部发出）
                while MIC_ACTIVE.load(Ordering::Relaxed) {
                    match MIC_CH.try_receive() {
                        Ok(chunk) => {
                            if ws.send_binary(&chunk.data[..chunk.len]).await.is_err() {
                                break 'session;
                            }
                        }
                        Err(_) => break,
                    }
                }

                // 3. 收服务端帧（50ms 超时回轮：保证中断/麦克风节拍）
                let frame = ws.next_frame(payload);
                match embassy_futures::select::select(frame, Timer::after_millis(50)).await {
                    embassy_futures::select::Either::First(Ok((kind, n))) => match kind {
                        ws::WsFrameKind::Text => {
                            let ev = voice_client::parse_server_message(&payload[..n]);
                            let done = handle_server_event(
                                &ev,
                                &mut ws,
                                &mut pcm,
                                &WS_TRACKER,
                                audio,
                                audio_in,
                                display,
                                now,
                            )
                            .await;
                            if done {
                                break 'session;
                            }
                        }
                        _ => {}
                    },
                    embassy_futures::select::Either::First(Err(ws::WsError::TooLarge)) => {
                        // 单帧超出 payload 容量：整帧已丢弃，流仍同步，继续收
                        println!("[Voice] 帧超出 {}B，已丢弃", WS_PAYLOAD_CAP);
                    }
                    embassy_futures::select::Either::First(Err(e)) => {
                        println!("[Voice] /ws/voice 断开: {e:?}");
                        break 'session;
                    }
                    embassy_futures::select::Either::Second(_) => {
                        // 50ms 心跳到点：回到循环顶部处理中断/麦克风
                    }
                }
            }

            WS_UP.store(false, Ordering::Relaxed);
            MIC_ACTIVE.store(false, Ordering::Relaxed);
        }

        // 断线重连（socket abort 后复用，缓冲不丢）
        println!("[Voice] 3s 后重连...");
        Timer::after_millis(3000).await;
        let mut retried = 0u32;
        loop {
            match ws.reconnect(server_ip, config::SERVER_PORT, &path).await {
                Ok(()) => break,
                Err(e) => {
                    retried += 1;
                    if retried % 10 == 1 {
                        println!("[Voice] 重连失败: {e:?}");
                    }
                    Timer::after_millis(3000).await;
                }
            }
        }
    }
}

use core::cell::RefCell;

/// 处理一条服务端事件（对应 C++ onVoiceEvent）；返回 true = 连接已断需重连
async fn handle_server_event(
    ev: &voice_client::ServerEvent,
    ws: &mut WsClient,
    pcm: &mut Vec<u8>,
    tracker: &Shared<RefCell<voice_client::RoundTracker>>,
    audio: &Shared<RefCell<AudioOutput>>,
    audio_in: &Shared<RefCell<AudioInput>>,
    display: &Shared<RefCell<Display>>,
    now: u32,
) -> bool {
    use voice_client::ServerEvent;
    match ev {
        ServerEvent::ServerReady | ServerEvent::SessionReady | ServerEvent::SessionClosed => {
            // 连接建立/会话结束：固件暂不特殊处理
            false
        }
        ServerEvent::VadSpeaking(speaking) => {
            if *speaking {
                LAST_VAD_MS.store(now, Ordering::Relaxed);
            }
            false
        }
        ServerEvent::AsrResult(text) => {
            let action = tracker.lock().await.borrow_mut().on_asr_result();
            if action.stop_playback {
                audio.lock().await.borrow_mut().stop_playback();
            }
            if action.pause_mic {
                MIC_ACTIVE.store(false, Ordering::Relaxed);
                tracker.lock().await.borrow_mut().mic_pause();
                audio_in.lock().await.borrow_mut().stop();
                println!("[Voice] 收音暂停（识别完成，出声前不再上行噪声）");
            }
            println!("[ASR][t={}] {}", now, text);
            display.lock().await.borrow_mut().add_user_line(text);
            false
        }
        ServerEvent::AssistantDelta(text) => {
            let (accept, first) = tracker.lock().await.borrow_mut().on_assistant_delta(now);
            if accept {
                if first {
                    println!("[Voice][t={}] LLM 开始流式输出", now);
                }
                display.lock().await.borrow_mut().append_reply_delta(text);
            }
            false
        }
        ServerEvent::AudioStart(rate) => {
            if tracker.lock().await.borrow_mut().on_voice_audio_start() {
                audio
                    .lock()
                    .await
                    .borrow_mut()
                    .begin_stream(rate.unwrap_or(config::DEFAULT_STREAM_INPUT_RATE));
            }
            false
        }
        ServerEvent::AudioChunk(b64) => {
            let (accept, stat) = tracker.lock().await.borrow_mut().on_audio_chunk(now);
            if accept {
                pcm.clear();
                voice_client::decode_base64(b64.as_bytes(), pcm);
                if !pcm.is_empty() {
                    audio.lock().await.borrow_mut().feed(pcm);
                }
                if let Some(lat) = stat {
                    println!("[Voice][t={}] LLM首token→首音频块: {} ms", now, lat);
                    let mut o = String::new();
                    voice_client::build_client_stats(lat, &mut o);
                    if ws.send_text(&o).await.is_err() {
                        return true;
                    }
                }
            }
            false
        }
        ServerEvent::AudioDone => {
            if tracker.lock().await.borrow_mut().on_audio_done() {
                audio.lock().await.borrow_mut().finish_stream();
                println!("[Voice][t={}] 回复音频推送完毕，播完自动继续聆听", now);
            }
            false
        }
        ServerEvent::AssistantCompleted(text) => {
            let need_fallback = tracker.lock().await.borrow_mut().on_completed();
            display.lock().await.borrow_mut().set_reply_full_text(text);
            println!("[Voice] AI回复完成({}字)", text.chars().count());
            if need_fallback {
                // 整轮没收到音频：走 /ws/tts-stream 兜底（见 tts_fallback）
                tts_fallback(ws, text, audio).await;
            }
            false
        }
        ServerEvent::AssistantError(msg) => {
            println!("[Voice] 错误: {}", msg);
            let flush = tracker.lock().await.borrow_mut().finalize_partial();
            if flush {
                display.lock().await.borrow_mut().flush_typing();
            }
            // 回复出错：恢复收音（asr.result 时已暂停）
            if tracker.lock().await.borrow_mut().resume_mic_on_error() {
                audio_in.lock().await.borrow_mut().start();
                MIC_ACTIVE.store(true, Ordering::Relaxed);
            }
            false
        }
        ServerEvent::InterruptAck => {
            let flush = tracker.lock().await.borrow_mut().finalize_partial();
            if flush {
                display.lock().await.borrow_mut().flush_typing();
            }
            false
        }
        ServerEvent::AudioFile(_) => false, // edge 引擎文件：忽略（completed 兜底）
        ServerEvent::Error(msg) => {
            println!("[Voice] 错误: {}", msg);
            if tracker.lock().await.borrow_mut().resume_mic_on_error() {
                audio_in.lock().await.borrow_mut().start();
                MIC_ACTIVE.store(true, Ordering::Relaxed);
            }
            false
        }
        ServerEvent::Unknown => false,
    }
}

/// /ws/tts-stream 兜底：整轮没收到音频时把全文送去合成（旧行为）。
/// 独立第二条连接，收 audio.start/chunk/done 喂同一扬声器。
async fn tts_fallback(
    main_ws: &mut WsClient,
    text: &str,
    audio: &Shared<RefCell<AudioOutput>>,
) {
    // Phase 1 简化：流式服务端不会触发本路径；先记录，待验证主链路后补第二条连接
    let _ = (main_ws, text, audio);
    println!("[TTS] 整轮无音频，兜底路径待接入（流式服务端不触发）");
}

/// WiFi 连接/断线重连（官方示例模式）
#[embassy_executor::task]
async fn wifi_conn(mut controller: esp_radio::wifi::WifiController<'static>) {
    loop {
        println!("[WiFi] 连接中...");
        match controller.connect_async().await {
            Ok(info) => {
                println!("[WiFi] 已连接: {:?}", info);
                let info = controller.wait_for_disconnect_async().await.ok();
                println!("[WiFi] 断开: {:?}，5s 后重连", info);
            }
            Err(e) => {
                println!("[WiFi] 连接失败: {e:?}");
            }
        }
        Timer::after_millis(5000).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, esp_radio::wifi::Interface>) {
    runner.run().await
}

/// 硬件初始化失败：打印后挂起
fn fail(delay: &Delay, msg: &str, e: &dyn core::fmt::Debug) -> ! {
    println!("[FATAL] {} {:?}", msg, e);
    loop {
        delay.delay_millis(1000);
    }
}
