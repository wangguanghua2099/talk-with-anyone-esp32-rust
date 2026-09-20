// ws.rs —— 极简 RFC6455 WebSocket 客户端（embassy-net TcpSocket 之上）
//
// 面向 talk-with-anyone 的需要裁剪：
// · 客户端→服务端帧必须掩码（RFC6455 要求），服务端→客户端帧不掩码
// · 收方向：文本（JSON 控制流）/二进制/ping→自动回 pong/close/分片累积
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
// 握手校验说明：Sec-WebSocket-Accept 需要 SHA-1，局域网自用场景跳过校验
// （与 C++ beginSSL 跳过证书校验同一取舍），只检查 101 状态码。

use alloc::string::String;
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Timer};
use esp_println::println;

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
    socket: TcpSocket<'static>,
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
    /// 发起 TCP 连接并完成 WS 升级握手。永不失败：握手未成功前内部重试
    /// （缓冲随本对象移交后，由 socket 反复重建；每次重建前上一个 socket
    /// 已完全 drop，缓冲独占性恢复）。
    pub async fn connect(
        stack: embassy_net::Stack<'static>,
        ip: Ipv4Addr,
        port: u16,
        path: &str,
        rx_buf: &'static mut [u8],
        tx_buf: &'static mut [u8],
        mask_seed: u32,
    ) -> Self {
        // SAFETY：两根裸指针指向 mk_static 出来的 'static 缓冲；每次循环末尾
        // 整个 client（含 socket）先 drop，再从指针重建 &'static mut，不存在
        // 存活的重叠引用。单任务顺序执行，无并发访问。
        let rx_ptr: *mut [u8] = rx_buf;
        let tx_ptr: *mut [u8] = tx_buf;
        loop {
            // SAFETY: see above
            let rx: &'static mut [u8] = unsafe { &mut *rx_ptr };
            let tx: &'static mut [u8] = unsafe { &mut *tx_ptr };
            let mut client = Self {
                socket: TcpSocket::new(stack, rx, tx),
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

    /// 断线后重连：abort 旧连接（回到 Closed 态）后重新握手。
    /// embassy-net 的 TcpSocket 在 Closed 态可再次 connect，缓冲得以复用。
    pub async fn reconnect(&mut self, ip: Ipv4Addr, port: u16, path: &str) -> Result<(), WsError> {
        self.socket.abort();
        self.tx_pending.clear();
        self.tx_sent = 0;
        self.frag.clear();
        self.frag_active = false;
        self.reset_frame();
        self.handshake(ip, port, path).await
    }

    /// TCP 连接 + WS 升级握手。握手不在 select 里跑（不会被取消），
    /// 所以这里的中间状态允许用局部变量。
    async fn handshake(&mut self, ip: Ipv4Addr, port: u16, path: &str) -> Result<(), WsError> {
        self.socket.set_timeout(Some(Duration::from_secs(10)));
        self.socket
            .connect((ip, port))
            .await
            .map_err(|_| WsError::Io)?;

        // 升级请求
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
        write_all(&mut self.socket, req.as_bytes()).await?;

        // 读响应头到 \r\n\r\n
        let mut resp = Vec::with_capacity(512);
        let mut byte = [0u8; 1];
        loop {
            let n = self.socket.read(&mut byte).await.map_err(|_| WsError::Io)?;
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
                    fill(&mut self.socket, &mut self.hdr[..2], &mut self.hdr_have).await?;
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
                        fill(&mut self.socket, &mut self.hdr[..2 + ext], &mut self.hdr_have)
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
                    fill(&mut self.socket, &mut self.dbuf[..k], &mut self.dbuf_have).await?;
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
                        fill(&mut self.socket, &mut self.dbuf[..k], &mut self.dbuf_have).await?;
                        self.discard_left += k;
                        self.dbuf_have = 0;
                    }
                    self.reset_frame();
                    return Err(WsError::TooLarge);
                }

                // ---- 数据帧载荷 + 分片归并 ----
                Stage::Payload => {
                    let n = self.payload_len;
                    fill(&mut self.socket, &mut out[..n], &mut self.payload_have).await?;
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

    /// 把 tx_pending 的剩余部分写完；`tx_sent` 持久，取消后从此处续发
    async fn flush_pending(&mut self) -> Result<(), WsError> {
        while self.tx_sent < self.tx_pending.len() {
            let n = self
                .socket
                .write(&self.tx_pending[self.tx_sent..])
                .await
                .map_err(|_| WsError::Io)?;
            if n == 0 {
                self.tx_pending.clear();
                self.tx_sent = 0;
                return Err(WsError::Closed);
            }
            self.tx_sent += n;
        }
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
    socket: &mut TcpSocket<'static>,
    dst: &mut [u8],
    have: &mut usize,
) -> Result<(), WsError> {
    while *have < dst.len() {
        let n = socket.read(&mut dst[*have..]).await.map_err(|_| WsError::Io)?;
        if n == 0 {
            return Err(WsError::Closed);
        }
        *have += n;
    }
    Ok(())
}

async fn write_all(socket: &mut TcpSocket<'static>, mut data: &[u8]) -> Result<(), WsError> {
    while !data.is_empty() {
        let n = socket.write(data).await.map_err(|_| WsError::Io)?;
        if n == 0 {
            return Err(WsError::Closed);
        }
        data = &data[n..];
    }
    Ok(())
}
