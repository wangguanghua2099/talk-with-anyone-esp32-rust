// ws.rs —— 极简 RFC6455 WebSocket 客户端（embassy-net TcpSocket 之上，支持 ws/wss）
//
// 面向 talk-with-anyone 的需要裁剪：
// · 客户端→服务端帧必须掩码（RFC6455 要求），服务端→客户端帧不掩码
// · 收方向：文本（JSON 控制流）/二进制/ping→自动回 pong/close/分片累积
// · 传输层二选一：ws（明文 TCP）或 wss（TLS 1.3，由 config::USE_WSS 切换）
//
// **取消安全（这里的命门，改代码务必守住）**
// voice_task 每圈用 `select(next_frame, Timer::50ms)` 保证按键/麦克风的上行节拍，
// 所以 next_frame 随时可能在任意 await 点被整体丢弃。被取消时：已从 socket 环形
// 缓冲取走的字节不会退回，因此所有跨 await 的中间状态只能落在 self 的字段里
// （stage / hdr+hdr_have / payload_have / dbuf+dbuf_have / discard_left /
// tx_pending+tx_sent），或落在调用方每轮传入的**同一块** out 缓冲里。
// 局部变量存半截进度 = 一被取消就丢字节 = 残帧的尾巴被当成新帧头解析 = 长度成
// 天文数字 = TooLarge/Io → 断线重连死循环。11KB 的 audio.chunk 需要多个 TCP 段
// 才能收齐、几乎必然跨过一次 50ms 超时，所以小 JSON 文字帧能过、音频帧必炸。
//
// wss 把同一套取消语义下沉到 TLS 记录层：TLS 读写 future 同样在 select 里被丢弃，
// 所以本仓库用打过"取消安全"补丁的 .deps/embedded-tls（读头/读体断点续读、
// 密文记录断点续发；见其 PATCH-NOTES.md）。
//
// **TLS 写路径的隐含约束（改动出站帧尺寸/缓冲前先读这段）**：
// 为规避 embedded-io-async `Write` 在"取消丢返回值"上的固有歧义，单次 send_raw
// 的整帧长度必须远小于 TLS 写缓冲的明文块容量（TLS_WRITE_BUF - 29 ≈ 4KB），
// 保证每次 tls.write 都能"一次性整体入账"（append 全收 + 返回值不丢），
// 不出现"一部分进了记录缓冲、下次又被重发"的中间态。当前所有出站帧：
// 麦克风 1024B+14、文本帧 <1KB，安全。
//
// 握手校验说明：Sec-WebSocket-Accept 需要 SHA-1，局域网自用场景跳过校验
// （与 C++ 版跳过证书校验同一取舍——wss 的 TLS 层同样用 UnsecureProvider
// 跳过证书链/主机名校验），只检查 101 状态码。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Timer};
use embedded_tls::{
    Aes128GcmSha256, FlushPolicy, TlsConfig, TlsConnection, TlsContext, TlsError,
    UnsecureProvider,
};
use esp_hal::rng::Rng;
use esp_println::println;

use crate::config;

/// WS 帧类型（载荷留在调用方传入的缓冲里，用返回的长度切片）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WsFrameKind {
    /// 服务端文本帧（JSON）
    Text,
    /// 服务端二进制帧（本协议下行不会出现）
    Binary,
    /// 服务端 ping（已自动回 pong）
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WsError {
    /// 对端关闭或 TCP 读到 0
    Closed,
    /// socket 层错误
    Io,
    /// 帧过大，超出接收缓冲（该帧已被整体丢弃，流保持同步，可继续收下一帧）
    TooLarge,
    /// 握手响应不是 101
    HandshakeFailed,
}

/// 收帧进度阶段：被取消后重入 next_frame 时靠它回到中断处，而不是从帧头重来
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    /// 读帧头（2 字节 + 可选 2/8 字节扩展长度）
    Hdr,
    /// 读控制帧载荷（ping/close，≤125B）
    Ctl,
    /// 读数据帧载荷到 out
    Payload,
    /// 过大数据帧：逐段丢掉载荷保持流同步
    Discard,
}

/// esp-hal Rng → embedded-tls 需要的 rand_core 0.6 `CryptoRngCore` 适配器。
/// esp-hal 的 `unstable` 特性（本工程已开）已为 `Rng` 实现 rand_core 0.6 的
/// `RngCore`（RNG_DATA 硬件寄存器），这里只差补一个 `CryptoRng` 标记。
struct TrngRng(Rng);

impl TrngRng {
    fn new() -> Self {
        Self(Rng::new())
    }
}

impl rand_core::RngCore for TrngRng {
    fn next_u32(&mut self) -> u32 {
        rand_core::RngCore::next_u32(&mut self.0)
    }
    fn next_u64(&mut self) -> u64 {
        rand_core::RngCore::next_u64(&mut self.0)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        rand_core::RngCore::fill_bytes(&mut self.0, dest)
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        rand_core::RngCore::try_fill_bytes(&mut self.0, dest)
    }
}

impl rand_core::CryptoRng for TrngRng {}

/// 传输层：明文 TCP（ws）或 TLS 1.3（wss）
enum Transport {
    /// 占位态（仅 TLS 换装/重连释放旧连接的瞬间出现，出现即内部错误）
    Empty,
    /// 明文 ws（config::USE_WSS = false）
    Plain(TcpSocket<'static>),
    /// wss：TLS 1.3 over TCP（Boxed：TlsConnection 本体放堆上，不占任务状态）
    Tls(Box<TlsConnection<'static, TcpSocket<'static>, Aes128GcmSha256>>),
}

impl Transport {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, WsError> {
        match self {
            Transport::Plain(socket) => socket.read(buf).await.map_err(|_| WsError::Io),
            // 取消安全由 .deps/embedded-tls 的补丁保证（见文件头）
            Transport::Tls(tls) => tls.read(buf).await.map_err(tls_err),
            Transport::Empty => Err(WsError::Io),
        }
    }

    async fn write(&mut self, data: &[u8]) -> Result<usize, WsError> {
        match self {
            Transport::Plain(socket) => socket.write(data).await.map_err(|_| WsError::Io),
            // 注意返回值语义：wss 下是"已收进 TLS 记录缓冲"的字节数（可能少于
            // data.len()）；记录本体要等 flush() 才上线路。取消安全的边界见文件头。
            Transport::Tls(tls) => tls.write(data).await.map_err(tls_err),
            Transport::Empty => Err(WsError::Io),
        }
    }

    /// 把 TLS 记录层缓冲里的密文推上线（明文模式 no-op）。
    /// flush_pending 每帧收尾必调：wss 下不调的话记录会一直躺在写缓冲里。
    async fn flush(&mut self) -> Result<(), WsError> {
        match self {
            Transport::Plain(_) => Ok(()),
            Transport::Tls(tls) => tls.flush().await.map_err(tls_err),
            Transport::Empty => Err(WsError::Io),
        }
    }
}

/// TLS 错误 → WS 错误：对端正常关闭（close_notify/EOF）映射 Closed，其余按 Io。
fn tls_err(e: TlsError) -> WsError {
    match e {
        TlsError::ConnectionClosed => WsError::Closed,
        _ => WsError::Io,
    }
}

/// 极简 base64 编码（握手 key 用，16 字节 → 24 字符）
fn b64_encode_short(data: &[u8], out: &mut String) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.clear();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
}

pub struct WsClient {
    /// 网络栈（重连重建 socket 用；Stack 是 Copy）
    stack: embassy_net::Stack<'static>,
    /// 当前传输（明文 TCP 或 TLS）
    transport: Transport,
    // 缓冲裸指针：socket/TLS 对象按生命周期"吞掉"了 &'static mut 借用，
    // 断线整体重建时需要重新取回。SAFETY 约定见 connect()/reconnect() 注释：
    // 单任务顺序执行；解引用前旧持有者（socket/TLS 对象）必须已 drop。
    ws_rx_ptr: *mut [u8],
    ws_tx_ptr: *mut [u8],
    tls_rx_ptr: *mut [u8],
    tls_tx_ptr: *mut [u8],
    stage: Stage,
    // 帧头（跨取消持久：扩展长度由已落盘的 hdr[0..2] 重算，无需额外字段）
    hdr: [u8; HDR_MAX],
    hdr_have: usize,
    opcode: u8,
    payload_len: usize,
    // 载荷接收进度（dst = 调用方每轮传入的同一块 out 缓冲）
    payload_have: usize,
    // 控制帧载荷 / 过大帧丢弃用的持久缓冲
    dbuf: [u8; DBUF],
    dbuf_have: usize,
    discard_left: usize,
    // 分片累积
    frag: Vec<u8>,
    frag_active: bool,
    // 发送掩码种子（LCG 演进；掩码只为混淆线路，随机性要求不高）
    mask_state: u32,
    // 未写完的出站帧：取消时保留，下次进函数先补发，避免线路上出现半截帧
    tx_pending: Vec<u8>,
    tx_sent: usize,
}

// 帧头最长：2 基础 + 8 扩展长度 = 10 字节（本端不收掩码帧，无 4 字节 key）
const HDR_MAX: usize = 10;
// 控制帧载荷上限 125B；这里兼作"过大帧丢弃"的分段缓冲
const DBUF: usize = 256;

impl WsClient {
    /// 发起连接（含 wss 时的 TLS 握手）并完成 WS 升级握手。永不失败：
    /// 握手未成功前内部重试（缓冲随本对象移交后，由 socket/TLS 对象反复重建；
    /// 每次重建前上一个对象已完全 drop，缓冲独占性恢复）。
    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        stack: embassy_net::Stack<'static>,
        ip: Ipv4Addr,
        port: u16,
        path: &str,
        ws_rx: &'static mut [u8],
        ws_tx: &'static mut [u8],
        tls_rx: &'static mut [u8],
        tls_tx: &'static mut [u8],
        mask_seed: u32,
    ) -> Self {
        // SAFETY：四根裸指针指向 mk_static 出来的 'static 缓冲；每次循环末尾
        // 整个 client（含 socket/TLS 连接）先 drop，再从指针重建 &'static mut，
        // 不存在存活的重叠引用。单任务顺序执行，无并发访问。
        let ws_rx_ptr: *mut [u8] = ws_rx;
        let ws_tx_ptr: *mut [u8] = ws_tx;
        let tls_rx_ptr: *mut [u8] = tls_rx;
        let tls_tx_ptr: *mut [u8] = tls_tx;
        loop {
            // SAFETY: see above
            let rx: &'static mut [u8] = unsafe { &mut *ws_rx_ptr };
            let tx: &'static mut [u8] = unsafe { &mut *ws_tx_ptr };
            let mut client = Self {
                stack,
                transport: Transport::Plain(TcpSocket::new(stack, rx, tx)),
                ws_rx_ptr,
                ws_tx_ptr,
                tls_rx_ptr,
                tls_tx_ptr,
                stage: Stage::Hdr,
                hdr: [0; HDR_MAX],
                hdr_have: 0,
                opcode: 0,
                payload_len: 0,
                payload_have: 0,
                dbuf: [0; DBUF],
                dbuf_have: 0,
                discard_left: 0,
                frag: Vec::new(),
                frag_active: false,
                mask_state: mask_seed,
                tx_pending: Vec::new(),
                tx_sent: 0,
            };
            match client.handshake(ip, port, path).await {
                Ok(()) => return client,
                Err(e) => {
                    println!("[Voice] 连接失败: {e:?}，3s 后重试");
                    Timer::after_millis(3000).await;
                }
            }
        }
    }

    /// 断线后重连：整体丢弃旧传输（TLS 包装 + TcpSocket，drop 即断开），
    /// 用裸指针重建明文 socket 后重新握手（含 TLS）。
    pub async fn reconnect(&mut self, ip: Ipv4Addr, port: u16, path: &str) -> Result<(), WsError> {
        // 旧连接在这里整体 drop（TLS 对象 → TcpSocket → smoltcp socket 被移除）
        self.transport = Transport::Empty;
        self.tx_pending.clear();
        self.tx_sent = 0;
        self.frag.clear();
        self.frag_active = false;
        self.reset_frame();

        // SAFETY：上一行的赋值已 drop 旧 socket/TLS 对象，两根 WS 缓冲恢复独占；
        // 单任务顺序执行，无并发访问。
        let rx: &'static mut [u8] = unsafe { &mut *self.ws_rx_ptr };
        let tx: &'static mut [u8] = unsafe { &mut *self.ws_tx_ptr };
        self.transport = Transport::Plain(TcpSocket::new(self.stack, rx, tx));
        self.handshake(ip, port, path).await
    }

    /// TCP 连接 +（wss 时）TLS 1.3 握手 + WS 升级握手。整段不在 select 里跑
    /// （不会被取消），所以这里的中间状态允许用局部变量。
    async fn handshake(&mut self, ip: Ipv4Addr, port: u16, path: &str) -> Result<(), WsError> {
        // ---- TCP 连接（明文/TLS 共用）----
        match &mut self.transport {
            Transport::Plain(socket) => {
                socket.set_timeout(Some(Duration::from_secs(10)));
                socket.connect((ip, port)).await.map_err(|_| WsError::Io)?;
            }
            _ => return Err(WsError::Io),
        }

        // ---- wss：TLS 1.3 握手（跳过证书校验，与 C++ 版 beginSSL(nullptr) 同策略）----
        if config::USE_WSS {
            // SAFETY：TLS 记录缓冲的裸指针此刻独占可用——重连路径上旧 TLS 连接
            // 已随 transport 整体 drop；首次连接路径上这两块缓冲从未被借出。
            let tls_rx: &'static mut [u8] = unsafe { &mut *self.tls_rx_ptr };
            let tls_tx: &'static mut [u8] = unsafe { &mut *self.tls_tx_ptr };

            let socket = match core::mem::replace(&mut self.transport, Transport::Empty) {
                Transport::Plain(s) => s,
                _ => return Err(WsError::Io),
            };
            let mut tls = Box::new(TlsConnection::new(socket, tls_rx, tls_tx));
            // ⭐ 根因修复（2026-10-19）：默认 FlushPolicy::Strict 会让每次
            //    tls.flush() 收尾调 delegate.flush() = TcpSocket::flush()，
            //    而它的语义是"等发送缓冲被对端 ACK 清空才返回"。麦克风上行
            //    ~56 帧/秒逐帧 flush，被 Linux 延迟 ACK 钉死在 ~25-40 帧/秒
            //    → voice_task 排水不及 → MIC_CH（容量 6）溢出、主循环静默丢块
            //    （实测每轮丢 26~48% 音频）→ 上行 PCM 满是 32ms 空洞：
            //    ASR 近音字翻车、VAD 尾静音累积变慢（字幕明显比 ws 慢）。
            //    Relaxed：密文照常写入 TCP 发送缓冲（协议栈异步上线，与明文
            //    ws 路径行为一致），只是不再空等 ACK。
            tls.set_flush_policy(FlushPolicy::Relaxed);
            // enable_rsa_signatures：服务端是 RSA-2048 自签证书（generate_cert.py），
            // 必须在 ClientHello 里通告 RSA 签名算法，否则对端选不出签名套件。
            let tls_config = TlsConfig::new().enable_rsa_signatures();
            let provider = UnsecureProvider::new::<Aes128GcmSha256>(TrngRng::new());
            match tls
                .open(TlsContext::new(&tls_config, provider))
                .await
            {
                Ok(()) => self.transport = Transport::Tls(tls),
                Err(e) => {
                    // 失败：TLS 对象连同内部 TcpSocket 一起 drop（连接即断），
                    // transport 停留在 Empty；外层 connect 循环/重连逻辑会重建
                    println!("[Voice] TLS 握手失败: {e:?}");
                    drop(tls);
                    return Err(WsError::HandshakeFailed);
                }
            }
        }

        // ---- 升级请求（wss 下经 TLS 记录层）----
        let mut key = [0u8; 16];
        let mut seed = self.mask_state;
        for w in key.chunks_mut(4) {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let r = seed.to_le_bytes();
            w.copy_from_slice(&r[..w.len()]);
        }
        let mut key_b64 = String::new();
        b64_encode_short(&key, &mut key_b64);
        let req = alloc::format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            path, ip, key_b64
        );
        write_all(&mut self.transport, req.as_bytes()).await?;
        // wss：请求还在 TLS 写缓冲里，必须 flush 才上线（明文模式为 no-op）
        self.transport.flush().await?;

        // ---- 读响应头到 \r\n\r\n ----
        let mut resp = Vec::with_capacity(512);
        let mut byte = [0u8; 1];
        loop {
            let n = self.transport.read(&mut byte).await?;
            if n == 0 {
                return Err(WsError::Closed);
            }
            resp.push(byte[0]);
            if resp.ends_with(b"\r\n\r\n") {
                break;
            }
            if resp.len() > 4096 {
                return Err(WsError::HandshakeFailed);
            }
        }
        let head = core::str::from_utf8(&resp).map_err(|_| WsError::HandshakeFailed)?;
        let ok = head.starts_with("HTTP/1.1 101") || head.starts_with("HTTP/1.0 101");
        if !ok {
            println!("[WS] 握手失败: {}", head.lines().next().unwrap_or(""));
            return Err(WsError::HandshakeFailed);
        }
        self.reset_frame();
        Ok(())
    }

    /// 读取下一帧，载荷写入 `out`，返回 (帧类型, 载荷长度)。
    /// **`out` 必须是每轮同一块缓冲**：被取消时已收的半截载荷就写在里面，
    /// 重入时从 `payload_have` 续读（见文件头注释）。
    /// 超过 `out` 容量的帧被整体丢弃并返回 TooLarge（流仍同步，可继续收下一帧）。
    pub async fn next_frame(&mut self, out: &mut [u8]) -> Result<(WsFrameKind, usize), WsError> {
        // 补发上次没写完的出站帧（pong / text / binary 都走 tx_pending）
        self.flush_pending().await?;

        loop {
            match self.stage {
                // ---- 帧头 ----
                Stage::Hdr => {
                    fill(&mut self.transport, &mut self.hdr[..2], &mut self.hdr_have).await?;
                    let len7 = (self.hdr[1] & 0x7F) as usize;
                    let ext = if len7 == 126 {
                        2
                    } else if len7 == 127 {
                        8
                    } else {
                        0
                    };
                    if ext > 0 {
                        // 取消在此处发生也没关系：hdr_have 已记着已收字节数
                        fill(&mut self.transport, &mut self.hdr[..2 + ext], &mut self.hdr_have)
                            .await?;
                    }
                    self.opcode = self.hdr[0] & 0x0F;
                    self.payload_len = if ext == 2 {
                        u16::from_be_bytes([self.hdr[2], self.hdr[3]]) as usize
                    } else if ext == 8 {
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&self.hdr[2..10]);
                        u64::from_be_bytes(b) as usize
                    } else {
                        len7
                    };
                    // 进入下一阶段时才清零该阶段的进度计数（此后中途取消不再清零）
                    self.stage = if self.opcode == 0x9 || self.opcode == 0x8 {
                        self.dbuf_have = 0;
                        Stage::Ctl
                    } else if self.payload_len > out.len() {
                        self.discard_left = 0;
                        self.dbuf_have = 0;
                        Stage::Discard
                    } else {
                        self.payload_have = 0;
                        Stage::Payload
                    };
                }

                // ---- 控制帧载荷（ping：回 pong；close：回执后判断开）----
                Stage::Ctl => {
                    // RFC6455：控制帧载荷 ≤125B，一段收完
                    let k = self.payload_len.min(DBUF);
                    fill(&mut self.transport, &mut self.dbuf[..k], &mut self.dbuf_have).await?;
                    let is_ping = self.opcode == 0x9;
                    let mut pong = [0u8; DBUF];
                    pong[..k].copy_from_slice(&self.dbuf[..k]);
                    // 先归位再发送：pong 写到一半被取消的话，tx_pending 会在
                    // 下次进本函数时补发完，线路不会被半截帧污染
                    self.reset_frame();
                    if is_ping {
                        let _ = self.send_raw(0xA, &pong[..k]).await;
                        return Ok((WsFrameKind::Ping, k));
                    }
                    let _ = self.send_raw(0x8, &[]).await;
                    return Err(WsError::Closed);
                }

                // ---- 过大帧：逐段丢弃，保持流同步 ----
                Stage::Discard => {
                    while self.discard_left < self.payload_len {
                        let k = (self.payload_len - self.discard_left).min(DBUF);
                        fill(&mut self.transport, &mut self.dbuf[..k], &mut self.dbuf_have).await?;
                        self.discard_left += k;
                        self.dbuf_have = 0;
                    }
                    self.reset_frame();
                    return Err(WsError::TooLarge);
                }

                // ---- 数据帧载荷 + 分片归并 ----
                Stage::Payload => {
                    let n = self.payload_len;
                    fill(&mut self.transport, &mut out[..n], &mut self.payload_have).await?;
                    let fin = self.hdr[0] & 0x80 != 0;
                    let op = self.opcode;

                    if op == 0x0 && !self.frag_active {
                        // 没有首帧的续传帧：丢弃
                        self.reset_frame();
                        continue;
                    }
                    if op == 0x1 || op == 0x2 {
                        if fin {
                            let kind = if op == 0x1 { WsFrameKind::Text } else { WsFrameKind::Binary };
                            self.reset_frame();
                            return Ok((kind, n));
                        }
                        // 未结束的首帧：开始累积
                        self.frag_active = true;
                        self.frag.clear();
                    }
                    if self.frag_active {
                        self.frag.extend_from_slice(&out[..n]);
                        let total = self.frag.len();
                        if total > out.len() {
                            self.frag.clear();
                            self.frag_active = false;
                            self.reset_frame();
                            return Err(WsError::TooLarge);
                        }
                        if fin {
                            out[..total].copy_from_slice(&self.frag);
                            self.frag.clear();
                            self.frag_active = false;
                            self.reset_frame();
                            return Ok((WsFrameKind::Text, total));
                        }
                        self.reset_frame();
                        continue;
                    }
                    // 其余 opcode（预留的 0x3~0x7）：整帧丢弃
                    self.reset_frame();
                    continue;
                }
            }
        }
    }

    /// 发送文本帧
    pub async fn send_text(&mut self, text: &str) -> Result<(), WsError> {
        self.send_raw(0x1, text.as_bytes()).await
    }

    /// 发送二进制帧（麦克风 PCM）
    pub async fn send_binary(&mut self, data: &[u8]) -> Result<(), WsError> {
        self.send_raw(0x2, data).await
    }

    /// 构造掩码帧并发送（帧体先进 tx_pending，写完才清空 → 取消安全）
    async fn send_raw(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WsError> {
        // 上一帧还有尾巴没写完（上一次 flush_pending 在 await 点被取消）：
        // 先把旧帧补完，否则下面的覆盖会把旧帧的剩余部分永久丢在线路之外
        // ——半截帧会让服务端帧解析错位（比丢整帧严重得多）。
        if self.tx_sent < self.tx_pending.len() {
            self.flush_pending().await?;
        }
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode); // FIN + opcode
        self.mask_state = self
            .mask_state
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let mask = self.mask_state.to_le_bytes();
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else if payload.len() <= 0xFFFF {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        self.tx_pending = frame;
        self.tx_sent = 0;
        self.flush_pending().await
    }

    /// 把 tx_pending 的剩余部分写完；`tx_sent` 持久，取消后从此处续发。
    /// 帧尾必调 transport.flush()：wss 下把 TLS 记录层的密文推上线
    /// （明文模式 no-op）。若 flush 被取消：tx_sent 已 == len，下次自动重试。
    async fn flush_pending(&mut self) -> Result<(), WsError> {
        while self.tx_sent < self.tx_pending.len() {
            let n = self
                .transport
                .write(&self.tx_pending[self.tx_sent..])
                .await?;
            if n == 0 {
                self.tx_pending.clear();
                self.tx_sent = 0;
                return Err(WsError::Closed);
            }
            self.tx_sent += n;
        }
        self.transport.flush().await?;
        self.tx_pending.clear();
        self.tx_sent = 0;
        Ok(())
    }

    /// 一帧处理完毕：回到帧头阶段（分片累积 frag 由调用方自行维护）
    fn reset_frame(&mut self) {
        self.stage = Stage::Hdr;
        self.hdr_have = 0;
        self.payload_have = 0;
        self.dbuf_have = 0;
        self.discard_left = 0;
        self.payload_len = 0;
        self.opcode = 0;
    }
}

/// 读满 dst；进度写回 `have`（持久）→ 被取消后从中断处续读，不丢字节
async fn fill(
    transport: &mut Transport,
    dst: &mut [u8],
    have: &mut usize,
) -> Result<(), WsError> {
    while *have < dst.len() {
        let n = transport
            .read(&mut dst[*have..])
            .await?;
        if n == 0 {
            return Err(WsError::Closed);
        }
        *have += n;
    }
    Ok(())
}

async fn write_all(transport: &mut Transport, mut data: &[u8]) -> Result<(), WsError> {
    while !data.is_empty() {
        let n = transport.write(data).await?;
        if n == 0 {
            return Err(WsError::Closed);
        }
        data = &data[n..];
    }
    Ok(())
}
