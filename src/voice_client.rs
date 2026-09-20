// voice_client.rs —— talk-with-anyone 语音协议实现（流式协议版）
// （对应 C++ voice_client.cpp，协议契约见 C++ 仓库 docs/交接文档-C++固件转Rust.md §3）
//
// 通道与报文（勿改）：
//   /ws/voice 主通道：
//     上行  二进制帧 = PCM16@16k 单声道小端（~512 样本/块）
//     上行  {"type":"session.start"|"interrupt"|"session.stop"|"client_stats"}
//     下行  server.ready / session.ready / vad.speaking / asr.result /
//           assistant.delta(文字增量) / audio.start / audio.chunk(b64) /
//           audio.done / assistant.completed / assistant.error /
//           audio.file(忽略) / interrupt.ack / session.closed / error
//   /ws/tts-stream 兜底通道：仅当整轮没收到过任何 audio.chunk 时，
//     发 {"text":全文,"sample_rate":16000}（不带 type 字段），
//     收 audio.start/chunk/done，与主通道同格式。
//
// 关键顺序：正常一轮 = asr.result → (assistant.delta×n 与
// audio.start→chunk×n→done 并行解耦) → assistant.completed。
// 用户再次说话的新 asr.result 隐式作废上一轮；interrupt 由设备按键主动发起。
//
// 解析语义与 C++（ArduinoJson 探测式 doc["type"]）保持一致：
// 未知字段忽略、未知消息类型返回 Unknown 而不是丢弃整个连接语义。
// 因此这里不用 serde 严格 tagged 枚举（收到未建模消息会整条报错丢弃），
// 而是一个只支持平坦 JSON 对象的小型宽容扫描器。
//
// WS 传输层接入时保留的串口观测点（与 C++ 同名，带毫秒时间戳）：
//   [ASR][t=ms] <文本>
//   [Voice][t=ms] LLM 开始流式输出
//   [Voice][t=ms] LLM首token→首音频块: XX ms
//   [Voice][t=ms] 回复音频推送完毕，播完自动继续聆听
//   [AudioOut] 环形缓冲 3072KB / [AudioOut] 播放完成 / [麦克风] 峰值

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

/// 服务端 → 设备的事件
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    /// 连接建立（session_id, asr_engine）。固件未用，仅解析。
    ServerReady,
    /// session.start 的应答
    SessionReady,
    /// VAD 检测：当前是否说话（仅用于"聆听超时"计时刷新）
    VadSpeaking(bool),
    /// 语音识别结果 = 新一轮回复开始（is_final 字段固定 true，固件不关心）
    AsrResult(String),
    /// LLM 回复文字增量（token 级，随生成实时推送，字幕流式打字用）
    AssistantDelta(String),
    /// 本轮音频流开始。sample_rate 引擎采样率（当前 24000），可能省略
    AudioStart(Option<u32>),
    /// TTS 音频块（base64 文本，尚未解码）
    AudioChunk(String),
    /// 本轮音频推送完毕（之后播放器可判定"排空即播完"）
    AudioDone,
    /// 回复生成结束，全文用于字幕对账
    AssistantCompleted(String),
    /// 本轮生成失败
    AssistantError(String),
    /// 服务端确认打断
    InterruptAck,
    /// 服务端结束会话。固件未用，仅解析。
    SessionClosed,
    /// edge 云端引擎逐批发文件 URL；固件忽略（依赖 completed 兜底）
    AudioFile(String),
    /// 通用错误（message 字段缺失时回退读 error 字段）
    Error(String),
    /// 未知消息（与 C++ 行为一致：调用方忽略）
    Unknown,
}

/// 解析一条服务端文本帧（UTF-8 JSON）。坏报文返回 Unknown，不会 panic。
pub fn parse_server_message(payload: &[u8]) -> ServerEvent {
    let ty = match json_string(payload, b"type") {
        Some(t) => t,
        None => return ServerEvent::Unknown,
    };
    match ty.as_str() {
        "server.ready" => ServerEvent::ServerReady,
        "session.ready" => ServerEvent::SessionReady,
        "vad.speaking" => ServerEvent::VadSpeaking(json_bool(payload, b"speaking").unwrap_or(false)),
        "asr.result" => ServerEvent::AsrResult(json_string(payload, b"text").unwrap_or_default()),
        "assistant.delta" => {
            ServerEvent::AssistantDelta(json_string(payload, b"text").unwrap_or_default())
        }
        "audio.start" => ServerEvent::AudioStart(json_u32(payload, b"sample_rate")),
        "audio.chunk" => ServerEvent::AudioChunk(json_string(payload, b"data").unwrap_or_default()),
        "audio.done" => ServerEvent::AudioDone,
        "assistant.completed" => {
            ServerEvent::AssistantCompleted(json_string(payload, b"text").unwrap_or_default())
        }
        "assistant.error" => {
            ServerEvent::AssistantError(json_string(payload, b"message").unwrap_or_default())
        }
        "interrupt.ack" => ServerEvent::InterruptAck,
        "session.closed" => ServerEvent::SessionClosed,
        "audio.file" => ServerEvent::AudioFile(json_string(payload, b"path").unwrap_or_default()),
        // C++ 兼容：message 字段缺失时回退读 error 字段
        "error" | "asr.error" => {
            let msg = json_string(payload, b"message")
                .or_else(|| json_string(payload, b"error"))
                .unwrap_or_default();
            ServerEvent::Error(msg)
        }
        _ => ServerEvent::Unknown,
    }
}

// ---------- 上行报文构造 ----------

/// /ws/voice 连接建立后立刻发送：{"type":"session.start","mode":"chat"}
pub fn build_session_start(out: &mut String) {
    out.clear();
    out.push_str("{\"type\":\"session.start\",\"mode\":\"chat\"}");
}

/// 打断当前 AI 回复（服务端 cancel LLM/TTS）：{"type":"interrupt"}
pub fn build_interrupt(out: &mut String) {
    out.clear();
    out.push_str("{\"type\":\"interrupt\"}");
}

/// 结束会话（挂断；固件挂断时不断开 WS）：{"type":"session.stop"}
pub fn build_session_stop(out: &mut String) {
    out.clear();
    out.push_str("{\"type\":\"session.stop\"}");
}

/// 延迟上报（每轮一次）：收到首个文字增量 → 首个音频块开始播放的毫秒数。
/// 服务端日志展示，对齐 web/手机端。
pub fn build_client_stats(llm_first_token_to_audio_ms: u32, out: &mut String) {
    out.clear();
    let _ = write!(
        out,
        "{{\"type\":\"client_stats\",\"llm_first_token_to_audio_ms\":{}}}",
        llm_first_token_to_audio_ms
    );
}

/// TTS 兜底合成请求（发在 /ws/tts-stream，不带 type 字段，与 C++ 完全一致）。
/// sample_rate 是要求服务端重采样到的目标率。
pub fn build_tts_request(text: &str, sample_rate: u32, out: &mut String) {
    out.clear();
    out.push_str("{\"text\":");
    append_json_string(out, text);
    let _ = write!(out, ",\"sample_rate\":{}}}", sample_rate);
}

/// 路径追加 token 查询参数（服务端启用 access_token 时使用）
pub fn ws_path_with_token(path: &str) -> String {
    if crate::config::ACCESS_TOKEN.is_empty() {
        String::from(path)
    } else {
        let mut s = String::from(path);
        s.push_str("?token=");
        s.push_str(crate::config::ACCESS_TOKEN);
        s
    }
}

// ---------- 轮次状态机（对应 C++ voice_client.cpp 的 6 个 s_round* 静态量） ----------

/// `asr.result` 事件要求调用方执行的动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsrResultAction {
    /// 上一轮音频流未收尾：先 `audio_out.stop_playback()` 清掉残留播放
    pub stop_playback: bool,
    /// 立即暂停麦克风上行：语句已识别完，到出声之间上行只有噪声帧，
    /// 会与下行音频在链路争抢带宽、推高首包出声延迟
    pub pause_mic: bool,
}

/// 一轮流式回复的守卫状态。
///
/// 规则（与 C++ 逐条对应）：
/// · `asr.result`：上一轮音频流未 done 则先停播放，然后整体重置（新一轮开始）；
///   **并立即暂停麦克风上行**——语句已识别完，到出声之间上行只有噪声帧，
///   会与下行音频在链路争抢带宽、推高首包出声延迟；
/// · `assistant.delta` / `audio.start` / `audio.chunk` / `assistant.completed`：
///   `round_finalized`（已收尾）时整条丢弃——打断/出错后迟到的内容不再显示；
/// · `interrupt.ack` / `assistant.error` / 主动打断：立即收尾（字幕定格已收到部分）；
/// · 出错（error/asr.error/assistant.error）时恢复收音，避免停在"聆听中"却没录音；
/// · `assistant.completed` 后若整轮没收到过音频 → 走 /ws/tts-stream 兜底合成；
/// · 首个 delta 记时间戳，首个（delta 之后的）音频块时上报一次 `client_stats` 延迟。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoundTracker {
    /// assistant.delta 流进行中
    pub round_active: bool,
    /// 本轮已收尾（completed/打断/出错），迟到内容丢弃
    pub round_finalized: bool,
    /// 本轮是否收到过 voice WS 推送的音频
    pub received_audio: bool,
    /// voice WS 音频流已 start、尚未 done
    pub voice_audio_on: bool,
    /// 麦克风上行状态（对应 C++ s_recording。严格说是客户端状态而非轮次状态，
    /// 放这里是为了让 asr.result 暂停/出错恢复的守卫逻辑集中一处）
    mic_active: bool,
    llm_first_token_ms: u32,
    stat_sent: bool,
}

impl RoundTracker {
    pub const fn new() -> Self {
        Self {
            round_active: false,
            round_finalized: false,
            received_audio: false,
            voice_audio_on: false,
            mic_active: false,
            llm_first_token_ms: 0,
            stat_sent: false,
        }
    }

    /// 整体重置（新一轮开始）。不改变麦克风上行状态（对应 C++ resetRound）。
    pub fn reset(&mut self) {
        let mic = self.mic_active;
        *self = Self::new();
        self.mic_active = mic;
    }

    // ----- 麦克风上行（对应 C++ startRecording/pauseMic 的状态位） -----

    /// 开始上行成功后调用（实际 I2S 启动由调用方执行）
    pub fn mic_start(&mut self) {
        self.mic_active = true;
    }

    /// 主动暂停上行（播放前防回声等场景）
    pub fn mic_pause(&mut self) {
        self.mic_active = false;
    }

    pub fn mic_is_active(&self) -> bool {
        self.mic_active
    }

    // ----- 轮次事件 -----

    /// `asr.result`：新一轮开始。
    /// `stop_playback` = 上一轮音频流未收尾，调用方需先 `stop_playback()`；
    /// `pause_mic` = 调用方应立即暂停麦克风上行（理由见结构体注释）。
    pub fn on_asr_result(&mut self) -> AsrResultAction {
        let action = AsrResultAction {
            stop_playback: self.voice_audio_on,
            pause_mic: self.mic_active,
        };
        self.mic_active = false;
        self.reset();
        action
    }

    /// `assistant.delta` 守卫。返回 true = 接受该增量（交给字幕流式缓冲），
    /// `first` = 是否本轮首个增量（调用方据此记录打点并打印
    /// "[Voice] LLM 开始流式输出"）。
    pub fn on_assistant_delta(&mut self, now_ms: u32) -> (bool, bool) {
        if self.round_finalized {
            return (false, false); // 已被打断收尾，丢弃迟到增量
        }
        let mut first = false;
        if !self.round_active {
            self.round_active = true;
            self.llm_first_token_ms = now_ms;
            first = true;
        }
        (true, first)
    }

    /// `audio.start`（voice 主通道）守卫。返回 true = 接受（begin_stream）。
    pub fn on_voice_audio_start(&mut self) -> bool {
        if self.round_finalized {
            return false; // 已被打断的旧回复，整条丢弃
        }
        self.received_audio = true;
        self.voice_audio_on = true;
        true
    }

    /// `audio.chunk`（voice 主通道）守卫 + 首块延迟统计。
    /// 返回 (是否接受该块, 需上报的 client_stats 延迟毫秒)。
    pub fn on_audio_chunk(&mut self, now_ms: u32) -> (bool, Option<u32>) {
        if self.round_finalized {
            return (false, None); // 旧回复的迟到音频块
        }
        // C++ 语义：首个"位于首个文字增量之后"的音频块触发一次上报；
        // 音频先于文字到达（并行解耦允许）则不报，等后续块
        let mut report = None;
        if !self.stat_sent && self.llm_first_token_ms != 0 {
            self.stat_sent = true;
            report = Some(now_ms.saturating_sub(self.llm_first_token_ms));
        }
        (true, report)
    }

    /// `audio.done`（voice 主通道）。返回 true = 本轮音频流正常收尾
    /// （调用方 finish_stream，播放器排空后自动判定播完）。
    pub fn on_audio_done(&mut self) -> bool {
        if self.voice_audio_on {
            self.voice_audio_on = false;
            true
        } else {
            false
        }
    }

    /// `assistant.completed` 收尾。返回 true = 整轮没收到过音频，
    /// 调用方需把全文送去 /ws/tts-stream 兜底合成（旧后端/关朗读）。
    pub fn on_completed(&mut self) -> bool {
        if self.round_finalized {
            return false; // 本轮已被打断，丢弃迟到结果
        }
        self.round_finalized = true;
        self.round_active = false;
        !self.received_audio
    }

    /// 本轮被打断/出错收尾（interrupt.ack / assistant.error / 主动打断共用）。
    /// 返回 true = 调用方应立即补完已收到的字幕（flush_typing）。
    pub fn finalize_partial(&mut self) -> bool {
        let flush = self.round_active && !self.round_finalized;
        self.round_active = false;
        self.round_finalized = true;
        flush
    }

    /// 出错恢复收音（对应 C++ 错误分支 error/asr.error/assistant.error 里的
    /// `if (!s_recording && isUp()) startRecording()`——asr.result 时已暂停上行，
    /// 回复出错后若不恢复会停在"聆听中"却没录音）。
    /// 返回 true = 调用方需启动麦克风（服务端连通性由调用方检查）；
    /// 返回 true 时上行状态已置位。
    pub fn resume_mic_on_error(&mut self) -> bool {
        if self.mic_active {
            return false;
        }
        self.mic_active = true;
        true
    }
}

// ---------- JSON 字符串转义/还原 ----------

/// 追加一个 JSON 字符串字面量（含引号）。UTF-8 内容直接透传（合法 JSON），
/// 仅转义引号、反斜杠与控制字符。
pub fn append_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------- base64 解码（audio.chunk 载荷） ----------

/// 解码标准 base64（含 padding 容错）。非法字符按字节序原地跳过；
/// 输出追加到 `out`（调用方复用缓冲避免反复分配）。
pub fn decode_base64(input: &[u8], out: &mut Vec<u8>) {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &c in input {
        if c == b'=' || c == b'\r' || c == b'\n' || c == b' ' {
            continue;
        }
        let Some(v) = val(c) else { continue };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
}

// ---------- 宽容 JSON 扫描器（仅支持平坦对象） ----------

/// 遍历平坦 JSON 对象的键值对，value 为原始字节切片（字符串仍含引号与转义）。
/// 返回 false 可提前终止。
fn for_each_entry<'a, F: FnMut(&'a [u8], &'a [u8]) -> bool>(payload: &'a [u8], mut f: F) {
    let mut i = match skip_ws(payload, 0) {
        Some(i) if payload[i] == b'{' => i + 1,
        _ => return,
    };
    loop {
        let Some(j) = skip_ws(payload, i) else { return };
        if payload[j] == b'}' {
            return;
        }
        // 键
        if payload[j] != b'"' {
            return;
        }
        let Some((key, end)) = scan_string(payload, j) else { return };
        let Some(k) = skip_ws(payload, end) else { return };
        if payload[k] != b':' {
            return;
        }
        let Some(v0) = skip_ws(payload, k + 1) else { return };
        let Some(vend) = scan_value(payload, v0) else { return };
        if !f(key, &payload[v0..vend]) {
            return;
        }
        let Some(k2) = skip_ws(payload, vend) else { return };
        match payload[k2] {
            b',' => i = k2 + 1,
            b'}' => return,
            _ => return,
        }
    }
}

fn skip_ws(payload: &[u8], mut i: usize) -> Option<usize> {
    while i < payload.len() {
        match payload[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            _ => return Some(i),
        }
    }
    None
}

/// 从 payload[i] 处（应为 '"'）扫描一个 JSON 字符串，返回（内部字节含转义、结束位置）。
fn scan_string(payload: &[u8], i: usize) -> Option<(&[u8], usize)> {
    if payload.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    while j < payload.len() {
        match payload[j] {
            b'\\' => j += 2,
            b'"' => return Some((&payload[i + 1..j], j + 1)),
            _ => j += 1,
        }
    }
    None
}

/// 扫描一个值的原始范围（字符串/数字/布尔/null/对象/数组），返回结束位置。
fn scan_value(payload: &[u8], i: usize) -> Option<usize> {
    match *payload.get(i)? {
        b'"' => scan_string(payload, i).map(|(_, e)| e),
        b'{' | b'[' => scan_nested(payload, i),
        b't' => payload[i..].starts_with(b"true").then(|| i + 4),
        b'f' => payload[i..].starts_with(b"false").then(|| i + 5),
        b'n' => payload[i..].starts_with(b"null").then(|| i + 4),
        _ => {
            // 数字：到逗号/右括号/空白为止
            let mut j = i;
            while j < payload.len()
                && matches!(payload[j], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
            {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// 跳过嵌套对象/数组的整体范围（带字符串感知的括号匹配）。
fn scan_nested(payload: &[u8], i: usize) -> Option<usize> {
    let open = payload[i];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth = 0usize;
    let mut j = i;
    while j < payload.len() {
        match payload[j] {
            b'"' => {
                j = scan_string(payload, j)?.1 - 1; // 循环尾 +1
            }
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// 取字符串字段（自动反转义）。
pub fn json_string(payload: &[u8], key: &[u8]) -> Option<String> {
    let raw = json_raw(payload, key)?;
    if raw.first() != Some(&b'"') {
        return None;
    }
    Some(unescape(&raw[1..raw.len() - 1]))
}

/// 取 u32 字段。缺失返回 None（对应 C++ 的 doc["k"] | 默认值 用法）。
pub fn json_u32(payload: &[u8], key: &[u8]) -> Option<u32> {
    let raw = json_raw(payload, key)?;
    let s = core::str::from_utf8(raw).ok()?;
    s.trim().parse().ok()
}

/// 取布尔字段。
pub fn json_bool(payload: &[u8], key: &[u8]) -> Option<bool> {
    match json_raw(payload, key)? {
        b"true" => Some(true),
        b"false" => Some(false),
        _ => None,
    }
}

fn json_raw<'a>(payload: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut found: Option<&'a [u8]> = None;
    for_each_entry(payload, |k, v| {
        if k == key {
            found = Some(v);
            false // 找到即停
        } else {
            true
        }
    });
    found
}

/// 还原 JSON 字符串转义（\" \\ \/ \b \f \n \r \t \uXXXX）。
/// 裸 UTF-8 字节原样保留。损坏的转义序列按字面字节处理，不会 panic。
fn unescape(s: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < s.len() {
        if s[i] != b'\\' || i + 1 >= s.len() {
            // 直接以字节续接：跳过不完整的 UTF-8 尾部由 push_str 处理——
            // 这里逐字节安全切分，保证 char boundary
            let start = i;
            while i < s.len() && s[i] != b'\\' {
                i += 1;
            }
            append_utf8_lossy(&mut out, &s[start..i]);
            continue;
        }
        i += 1;
        let c = s[i];
        i += 1;
        match c {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000C}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                if let Some(cp) = parse_u16_hex(s, &mut i) {
                    // 仅处理 BMP；代理对按替换字符（服务端文本不会拆代理对）
                    if (0xD800..0xE000).contains(&cp) {
                        out.push('\u{FFFD}');
                    } else {
                        if let Some(ch) = char::from_u32(cp as u32) {
                            out.push(ch);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn parse_u16_hex(s: &[u8], i: &mut usize) -> Option<u16> {
    if *i + 4 > s.len() {
        return None;
    }
    let mut v: u16 = 0;
    for n in 0..4 {
        let d = (s[*i + n] as char).to_digit(16)?;
        v = v * 16 + d as u16;
    }
    *i += 4;
    Some(v)
}

/// 按合法 UTF-8 边界追加字节（不完整序列丢弃），保证不 panic。
fn append_utf8_lossy(out: &mut String, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        match core::str::from_utf8(bytes) {
            Ok(s) => {
                out.push_str(s);
                return;
            }
            Err(e) => {
                let good = e.valid_up_to();
                if good > 0 {
                    // valid_up_to 保证前缀合法，安全转写
                    if let Ok(s) = core::str::from_utf8(&bytes[..good]) {
                        out.push_str(s);
                    }
                }
                // 跳过一个坏字节
                let skip = e.error_len().unwrap_or(bytes.len() - good);
                bytes = &bytes[good + skip..];
            }
        }
    }
}

// 说明：本 crate 依赖 esp-hal，只能在 xtensa 目标上编译，无法跑 host 端
// cargo test；解析逻辑的正确性依赖后续联调验证。
