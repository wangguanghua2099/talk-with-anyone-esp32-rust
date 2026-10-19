// ws.rs —— WS 适配层：把 embedded-ws（../embedded-ws，从本工程提炼的独立 crate）
// 包成本工程既有接口（connect / reconnect / next_frame / send_text / send_binary）。
//
// 分工：
// · 帧状态机、升级握手（含 SHA-1 + Sec-WebSocket-Accept 校验）、取消安全契约：
//   全部由 embedded-ws 提供（crate 文档与 src/client.rs 有完整论述）。
//   本文件只保留"工程特供"三件：Transport 枚举（运行时 ws/wss 切换）、
//   TrngRng（esp-hal RNG → rand_core 0.6）、静态缓冲收回（裸指针）。
// · 为什么取消安全是命门（select(next_frame, 50ms) 随时整体丢弃 future、
//   已读字节不退回、进度只能落在 self/同一 out 缓冲）：见 embedded-ws 文档；
//   本工程历史血泪注释已随迁移归档到 docs/ 方案文档（不进 git）。
//
// **TLS 写路径的隐含约束（改动出站帧尺寸/缓冲前先读这段）**：
// 为规避 embedded-io-async `Write` 在"取消丢返回值"上的固有歧义，单次
// send_* 的整帧长度必须远小于 TLS 写缓冲的明文块容量（TLS_WRITE_BUF - 29
// ≈ 4KB），保证每次 tls.write 都能"一次性整体入账"（append 全收 + 返回值
// 不丢），不出现"一部分进了记录缓冲、下次又被重发"的中间态。
// 当前所有出站帧：麦克风 1024B+14、文本帧 <1KB，安全。
//
// wss 的取消语义下沉到 TLS 记录层：本工程继续用打过"取消安全"补丁的
// .deps/embedded-tls（读头/读体断点续读、密文记录断点续发；见其
// PATCH-NOTES.md）。
//
// accept 校验：embedded-ws 默认开启（SHA-1 为内置实现，且与 tungstenite
// 真实实现双向对拍过）；服务端为标准实现，校验通过。自建服务器无 SNI
// 需求，TlsConfig 不设 server_name（共享 IP/CDN 场景才必须设置）。

use alloc::boxed::Box;
use core::net::Ipv4Addr;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Timer};
use embedded_tls::{
    Aes128GcmSha256, FlushPolicy, TlsConfig, TlsConnection, TlsContext, TlsError,
    UnsecureProvider,
};
use embedded_ws::{HandshakeOptions, WsClient as CoreWsClient};
use esp_hal::rng::Rng;
use esp_println::println;

use crate::config;

/// WS 帧类型（转发自 embedded-ws；载荷留在调用方传入的缓冲里，用长度切片）
pub use embedded_ws::FrameKind as WsFrameKind;

/// 本固件的 WS 错误类型：embedded-ws 通用错误，传输层错误具体化为
/// [`TransportError`]（`WsError::TooLarge` 等变体语义与旧实现一致）。
pub type WsError = embedded_ws::WsError<TransportError>;

/// 传输层错误（embedded-ws `Transport` 契约要求实现 `embedded_io::Error`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    /// 对端正常关闭（TLS close_notify / EOF）
    Closed,
    /// socket / TLS 层错误
    Io,
}

impl core::fmt::Display for TransportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            TransportError::Closed => "closed",
            TransportError::Io => "io error",
        })
    }
}

impl core::error::Error for TransportError {}

impl embedded_io_async::Error for TransportError {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        embedded_io_async::ErrorKind::Other
    }
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

/// 传输层：明文 TCP（ws）或 TLS 1.3（wss），实现 embedded-ws 的
/// [`embedded_ws::Transport`] 契约（read/write + "推上线"语义的 flush）。
enum Transport {
    /// 明文 ws（config::USE_WSS = false）
    Plain(TcpSocket<'static>),
    /// wss：TLS 1.3 over TCP（Boxed：TlsConnection 本体放堆上，不占任务栈）
    Tls(Box<TlsConnection<'static, TcpSocket<'static>, Aes128GcmSha256>>),
}

/// TLS 错误 → 传输错误：对端正常关闭（close_notify/EOF）映射 Closed，其余按 Io。
fn tls_err(e: TlsError) -> TransportError {
    match e {
        TlsError::ConnectionClosed => TransportError::Closed,
        _ => TransportError::Io,
    }
}

impl embedded_io_async::ErrorType for Transport {
    type Error = TransportError;
}

impl embedded_io_async::Read for Transport {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self {
            Transport::Plain(socket) => socket.read(buf).await.map_err(|_| TransportError::Io),
            // 取消安全由 .deps/embedded-tls 的补丁保证（见文件头）
            Transport::Tls(tls) => tls.read(buf).await.map_err(tls_err),
        }
    }
}

impl embedded_io_async::Write for Transport {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, TransportError> {
        match self {
            Transport::Plain(socket) => socket.write(buf).await.map_err(|_| TransportError::Io),
            // 注意返回值语义：wss 下是"已收进 TLS 记录缓冲"的字节数（可能少于
            // buf.len()）；记录本体要等 flush 才上线路。取消安全的边界见文件头。
            Transport::Tls(tls) => tls.write(buf).await.map_err(tls_err),
        }
    }

    /// 明文模式 no-op；wss 下推送 TLS 记录层密文（embedded-ws 的
    /// `flush_transport` 委托到这里，语义 = "落线即可，不等对端 ACK"）。
    async fn flush(&mut self) -> Result<(), TransportError> {
        match self {
            Transport::Plain(_) => Ok(()),
            Transport::Tls(tls) => tls.flush().await.map_err(tls_err),
        }
    }
}

impl embedded_ws::Transport for Transport {
    async fn flush_transport(&mut self) -> Result<(), TransportError> {
        embedded_io_async::Write::flush(self).await
    }
}

pub struct WsClient {
    /// 网络栈（重连重建 socket 用；Stack 是 Copy）
    stack: embassy_net::Stack<'static>,
    /// 连接（None 只出现在重连尝试的瞬间，此时不持有任何传输对象）
    client: Option<CoreWsClient<Transport>>,
    // 缓冲裸指针：socket/TLS 对象按生命周期"吞掉"了 &'static mut 借用，
    // 断线整体重建时需要重新取回。SAFETY 约定：单任务顺序执行；
    // 解引用（build_transport 内）前旧持有者必须已 drop（self.client 为 None）。
    ws_rx_ptr: *mut [u8],
    ws_tx_ptr: *mut [u8],
    tls_rx_ptr: *mut [u8],
    tls_tx_ptr: *mut [u8],
    /// 连接种子：握手 key 派生 + 帧掩码序列起点（逐连接演进）
    seed: u32,
}

impl WsClient {
    /// 发起连接（含 wss 时的 TLS 握手）并完成 WS 升级握手。永不失败：
    /// 握手未成功前内部重试（缓冲随对象移交后，由传输对象反复重建；
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
        let mut me = Self {
            stack,
            client: None,
            ws_rx_ptr: ws_rx,
            ws_tx_ptr: ws_tx,
            tls_rx_ptr: tls_rx,
            tls_tx_ptr: tls_tx,
            seed: mask_seed,
        };
        loop {
            match me.try_connect(ip, port, path).await {
                Ok(()) => return me,
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
        self.client = None;
        self.try_connect(ip, port, path).await
    }

    /// 建传输（TCP + 可选 TLS）→ embedded-ws 升级握手。
    /// 不重试：调用方（connect 循环 / main 的重连循环）负责节拍。
    async fn try_connect(&mut self, ip: Ipv4Addr, port: u16, path: &str) -> Result<(), WsError> {
        let transport = self.build_transport(ip, port).await?;
        // 每连接演进一次种子：握手 key 与帧掩码序列逐连接不同
        self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        // Host 头与旧实现保持逐字节一致：只写 IP、不带端口
        let host = alloc::format!("{ip}");
        let opts = HandshakeOptions::new(&host, path, self.seed);
        let client = CoreWsClient::handshake(transport, opts).await?;
        self.client = Some(client);
        Ok(())
    }

    /// TCP 连接 +（USE_WSS 时）TLS 1.3 握手。失败时对象已 drop
    /// （缓冲所有权恢复），调用方可安全重建。
    async fn build_transport(&mut self, ip: Ipv4Addr, port: u16) -> Result<Transport, WsError> {
        // SAFETY：本函数只在 self.client 为 None 时被调用（connect 首轮 /
        // reconnect 先置 None），旧 socket/TLS 对象已 drop，两根 WS 缓冲
        // 独占可用；单任务顺序执行，无并发访问。
        let rx: &'static mut [u8] = unsafe { &mut *self.ws_rx_ptr };
        let tx: &'static mut [u8] = unsafe { &mut *self.ws_tx_ptr };
        let mut socket = TcpSocket::new(self.stack, rx, tx);
        socket.set_timeout(Some(Duration::from_secs(10)));
        socket
            .connect((ip, port))
            .await
            .map_err(|_| WsError::Transport(TransportError::Io))?;

        if config::USE_WSS {
            // SAFETY：同上——TLS 记录缓冲的裸指针此刻独占可用。
            let tls_rx: &'static mut [u8] = unsafe { &mut *self.tls_rx_ptr };
            let tls_tx: &'static mut [u8] = unsafe { &mut *self.tls_tx_ptr };

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
            match tls.open(TlsContext::new(&tls_config, provider)).await {
                Ok(()) => Ok(Transport::Tls(tls)),
                Err(e) => {
                    // 失败：TLS 对象连同内部 TcpSocket 一起 drop（连接即断）
                    println!("[Voice] TLS 握手失败: {e:?}");
                    Err(WsError::Transport(TransportError::Io))
                }
            }
        } else {
            Ok(Transport::Plain(socket))
        }
    }

    /// 读取下一帧，载荷写入 `out`，返回 (帧类型, 载荷长度)。
    /// **`out` 必须是每轮同一块缓冲**（取消安全契约，见 embedded-ws 文档）；
    /// 超过 `out` 容量的帧被整体丢弃并返回 `WsError::TooLarge`（流仍同步）。
    pub async fn next_frame(&mut self, out: &mut [u8]) -> Result<(WsFrameKind, usize), WsError> {
        match self.client.as_mut() {
            Some(c) => c.next_frame(out).await.map(|f| (f.kind, f.len)),
            None => Err(WsError::Closed),
        }
    }

    /// 发送文本帧
    pub async fn send_text(&mut self, text: &str) -> Result<(), WsError> {
        match self.client.as_mut() {
            Some(c) => c.send_text(text).await,
            None => Err(WsError::Closed),
        }
    }

    /// 发送二进制帧（麦克风 PCM）
    pub async fn send_binary(&mut self, data: &[u8]) -> Result<(), WsError> {
        match self.client.as_mut() {
            Some(c) => c.send_binary(data).await,
            None => Err(WsError::Closed),
        }
    }
}
