// ntp.rs —— 极简 SNTP 客户端（48 字节报文，不引新依赖）
//
// 为什么不用现成 crate：本项目的网络栈是 embassy-net（no_std），能用的 SNTP
// crate 要么依赖 std/Tokio，要么依赖 embedded-io 适配层，版本矩阵的风险不值得
// ——SNTP 本身就是"发 48 字节、收 48 字节、取第 40..44 字节"的事。
//
// 服务器用 IP 直连（见 config::NTP_SERVERS），逐个尝试；不走 DNS，少一个失败点。
// 同步成功后只存"unix 秒 + 当时的启动毫秒"，之后靠本地时基推算，12 小时再校准一次。
// 顶栏只需要 HH:MM，所以不做日历换算（UTC+8 偏移后直接取时分）。

use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_net::Stack;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_time::{Duration, Timer};
use esp_println::println;

use crate::config;

/// 同步到的 unix 秒（UTC），仅在 SYNCED=true 时有意义
static SYNC_UNIX: AtomicU32 = AtomicU32::new(0);
/// 同步那一刻的启动毫秒（用于推算当前时间；u32 回绕用 wrapping 差值即可）
static SYNC_BASE_MS: AtomicU32 = AtomicU32::new(0);
static SYNCED: AtomicBool = AtomicBool::new(false);

/// NTP 纪元(1900-01-01) 到 Unix 纪元(1970-01-01) 的秒数
const NTP_TO_UNIX: u64 = 2_208_988_800;

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

/// 当前本地时间（UTC+8）的 (时, 分)；未同步返回 None
pub fn clock_hm(now_ms: u32) -> Option<(u32, u32)> {
    if !SYNCED.load(Ordering::Relaxed) {
        return None;
    }
    let base = SYNC_UNIX.load(Ordering::Relaxed) as u64;
    let elapsed_s = now_ms.wrapping_sub(SYNC_BASE_MS.load(Ordering::Relaxed)) as u64 / 1000;
    let local = base + elapsed_s + (config::TZ_OFFSET_HOURS as u64) * 3600;
    Some((((local / 3600) % 24) as u32, ((local / 60) % 60) as u32))
}

/// "HH:MM" 或未同步时的 "--:--"
pub fn clock_text(now_ms: u32) -> String {
    use core::fmt::Write;
    match clock_hm(now_ms) {
        Some((h, m)) => {
            let mut s = String::new();
            let _ = write!(s, "{:02}:{:02}", h, m);
            s
        }
        None => String::from("--:--"),
    }
}

/// 对时任务：启动后循环同步，失败换下一个服务器
#[embassy_executor::task]
pub async fn ntp_task(stack: Stack<'static>) -> ! {
    // 等 DHCP 拿到地址再动——SNTP 是 UDP 单播/组播，没 IP 发出去也是白发的超时。
    // 这个等待原来挂在主任务上，把"渲染层接管屏幕"一起拖住了（见 main.rs 注释）。
    stack.wait_config_up().await;
    if let Some(cfg) = stack.config_v4() {
        println!("[WiFi] 已连接, IP: {}", cfg.address);
    }

    let rx_meta = mk_static!([PacketMetadata; 1], [PacketMetadata::EMPTY; 1]);
    let rx_buf = mk_static!([u8; 64], [0u8; 64]);
    let tx_meta = mk_static!([PacketMetadata; 1], [PacketMetadata::EMPTY; 1]);
    let tx_buf = mk_static!([u8; 64], [0u8; 64]);
    let mut sock = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);
    // 端口 0 = 让协议栈随机分配本地端口
    if sock.bind(0).is_err() {
        println!("[NTP] 本地端口绑定失败");
    }

    loop {
        for ip in config::NTP_SERVERS.iter() {
            match sync_once(&mut sock, *ip).await {
                Some(unix) => {
                    let now = embassy_time::Instant::now().as_millis() as u32;
                    SYNC_UNIX.store(unix as u32, Ordering::Relaxed);
                    SYNC_BASE_MS.store(now, Ordering::Relaxed);
                    SYNCED.store(true, Ordering::Relaxed);
                    println!("[NTP] 对时成功 {} unix={}", ip, unix);
                    break;
                }
                None => println!("[NTP] {} 无响应，换下一个", ip),
            }
        }
        Timer::after(Duration::from_secs(config::NTP_RESYNC_SECS)).await;
    }
}

/// 一次查询：发 48 字节请求，3 秒内等回复；成功返回 unix 秒
async fn sync_once(sock: &mut UdpSocket<'static>, server: core::net::Ipv4Addr) -> Option<u64> {
    // LI=0, VN=3, Mode=3(client) → 0x1B，其余字段留零（服务器不校验）
    let mut pkt = [0u8; 48];
    pkt[0] = 0x1B;
    if sock.send_to(&pkt, (server, 123)).await.is_err() {
        return None;
    }

    let mut rx = [0u8; 48];
    let recv = sock.recv_from(&mut rx);
    match embassy_futures::select::select(recv, Timer::after_millis(3000)).await {
        embassy_futures::select::Either::First(Ok((n, _from))) => {
            if n < 48 {
                return None;
            }
            // 第 0 字节低 3 位 = mode，4=server 5=broadcast
            let mode = rx[0] & 0x07;
            if mode != 4 && mode != 5 {
                return None;
            }
            // Transmit Timestamp（服务器发送时刻）秒部分在 40..44，大端
            let ntp = u32::from_be_bytes([rx[40], rx[41], rx[42], rx[43]]) as u64;
            if ntp < NTP_TO_UNIX {
                return None; // 未同步的服务器会回 0
            }
            Some(ntp - NTP_TO_UNIX)
        }
        _ => None,
    }
}
