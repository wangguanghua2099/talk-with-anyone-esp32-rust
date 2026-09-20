// display.rs —— 字幕数据模型：流式打字机 + 前缀对账（对应 C++ display_ui.cpp 的字幕部分）
//
// 本模块先落定与 C++ 一致的字幕状态机（WS 层直接调用），硬件绘制层
// （ST7789 驱动 + efontCN 中文点阵 + 行级 diff 局部重绘）待实现：
// 接入时只需把本模块的状态画出来，策略见交接文档 §4.5。
//
// 流式协议下的字幕行为（对应 C++ appendReplyDelta/setReplyFullText）：
// · assistant.delta 增量追加进打字缓冲，由 tick 按固定节奏揭示
//   （LLM 快于揭示速度时缓冲排队，慢于时即时显示）；
// · assistant.completed 的全文与"已入队增量"做前缀对账，只补齐差额；
//   没收到过增量（旧后端）或内容对不上时，整条按打字机重新显示；
// · flush（打断/出错/新一轮）立即定格已收到的部分。

use alloc::string::String;
use alloc::vec::Vec;

use crate::{config, font};

/// 历史行上限（C++ MAX_LINES）
pub const MAX_HISTORY_LINES: usize = 80;
/// 行内文本可用像素宽（C++ SUB_W；顶栏与折行共用）
pub const SUB_W: i32 = 232;
/// 用户行绿色（C++ addUserLine）
pub const USER_COLOR: u16 = 0x07E0;
/// AI 行青色（C++ s_curColor）
pub const AI_COLOR: u16 = 0x07FF;

/// 显示状态（顶栏用）
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum DisplayState {
    #[default]
    Idle,
    Listening,
    Playing,
    Offline,
}

impl DisplayState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DisplayState::Idle => "IDLE",
            DisplayState::Listening => "LISTEN",
            DisplayState::Playing => "PLAY",
            DisplayState::Offline => "OFFLINE",
        }
    }

    /// 顶栏状态字颜色（C++ drawTopBar 的合法 RGB565 调色板）
    pub fn color(&self) -> u16 {
        match self {
            DisplayState::Idle => 0xFFFF,     // 白
            DisplayState::Listening => 0x67F4, // 春绿
            DisplayState::Playing => 0xFF0C,  // 琥珀黄
            DisplayState::Offline => 0xF98C,  // 红
        }
    }
}

/// 已完成的一行字幕（文本 + 颜色）
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleLine {
    pub text: String,
    pub color: u16,
}

/// 字幕数据模型（纯逻辑，不依赖硬件）
#[derive(Debug, Default)]
pub struct Display {
    state: DisplayState,
    volume: i32,

    /// 已完成的字幕行（尾部 MAX_HISTORY_LINES 条）
    history: Vec<SubtitleLine>,
    /// 打字机剩余文本（未揭示）
    typing_rest: String,
    /// 当前正在打的一行
    cur_line: String,
    cur_color: u16,
    /// 流式回复进行中（assistant.delta 追加模式）
    stream_mode: bool,
    /// 已进入打字缓冲的累计文本（与全文对账用）
    stream_queued: String,
    /// 上次揭示时刻（ms）
    last_type_ms: u32,
    /// 顶栏音量临时条的显示截止时刻（ms，0=不显示；对应 C++ s_volShowUntil）
    volume_until_ms: u32,
}

impl Display {
    pub fn new() -> Self {
        Self {
            state: DisplayState::Idle,
            volume: 80,
            history: Vec::new(),
            typing_rest: String::new(),
            cur_line: String::new(),
            cur_color: AI_COLOR,
            stream_mode: false,
            stream_queued: String::new(),
            last_type_ms: 0,
            volume_until_ms: 0,
        }
    }

    // ---------- 顶栏 ----------

    pub fn set_state(&mut self, state: DisplayState) {
        self.state = state;
    }

    pub fn state(&self) -> DisplayState {
        self.state
    }

    /// 设置音量并开启顶栏临时音量条（1.5s 后自动收起）。同值不重复触发，
    /// 与 C++ setVolume 一致。
    pub fn set_volume(&mut self, volume: i32, now_ms: u32) {
        let v = volume.clamp(0, 100);
        if v == self.volume {
            return;
        }
        self.volume = v;
        self.volume_until_ms = now_ms.wrapping_add(1500);
    }

    /// 音量临时条显示截止时刻（0 = 不显示）
    pub fn volume_until_ms(&self) -> u32 {
        self.volume_until_ms
    }

    pub fn volume(&self) -> i32 {
        self.volume
    }

    // ---------- 字幕 API（与 C++ display_ui.cpp 一一对应） ----------

    /// 用户说的话（绿色，前缀"我:"），立即整行显示。上一条 AI 若没打完先补完。
    pub fn add_user_line(&mut self, text: &str) {
        self.flush_typing();
        let mut line = String::from("我:");
        line.push_str(text);
        self.push_line(line, USER_COLOR);
    }

    /// 整段打字机显示（旧后端兜底路径：completed 全文一次到达）
    pub fn begin_reply_typewriter(&mut self, text: &str) {
        self.flush_typing();
        self.typing_rest = String::from(text);
        self.cur_line.clear();
        self.cur_color = AI_COLOR;
        self.stream_mode = false;
        self.stream_queued.clear();
    }

    /// 进入流式模式（首个 assistant.delta 到达时由 append_reply_delta 自动触发）
    pub fn begin_reply_stream(&mut self) {
        self.flush_typing();
        self.typing_rest.clear();
        self.cur_line.clear();
        self.cur_color = AI_COLOR;
        self.stream_queued.clear();
        self.stream_mode = true;
    }

    /// LLM 文字增量追加进打字缓冲，由 tick 按固定节奏揭示
    pub fn append_reply_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if !self.stream_mode {
            self.begin_reply_stream();
        }
        self.typing_rest.push_str(delta);
        self.stream_queued.push_str(delta);
    }

    /// 回复完成：以后端全文为准。与已排队的增量做前缀对账，只补齐差额；
    /// 没收到过增量（旧后端）或内容对不上时，整条按打字机重新显示。
    pub fn set_reply_full_text(&mut self, full: &str) {
        if self.stream_mode && full.starts_with(self.stream_queued.as_str()) {
            if full.len() > self.stream_queued.len() {
                let rest = &full[self.stream_queued.len()..];
                self.typing_rest.push_str(rest);
            }
            self.stream_mode = false;
            self.stream_queued.clear();
            return;
        }
        self.begin_reply_typewriter(full);
    }

    /// 立即定格：把剩余未打的字入历史（打断/出错/新一轮时调用）。
    /// 剩余文本可能含换行（LLM 增量原文），折行统一交给 push_line（对应 C++ pushLines）。
    pub fn flush_typing(&mut self) {
        self.stream_mode = false;
        self.stream_queued.clear();
        if self.typing_rest.is_empty() && self.cur_line.is_empty() {
            return;
        }
        let mut rest = core::mem::take(&mut self.cur_line);
        rest.push_str(&self.typing_rest);
        self.typing_rest.clear();
        let color = self.cur_color;
        self.push_line(rest, color);
    }

    pub fn clear_chat(&mut self) {
        self.history.clear();
        self.typing_rest.clear();
        self.cur_line.clear();
        self.stream_mode = false;
        self.stream_queued.clear();
    }

    /// 打字机推进（主循环调用，now_ms 为毫秒时钟）。
    /// 143ms/单位 ≈ 7 单位/秒，与 TTS 朗读速度大致同步（可读、不晃眼）：
    /// 中文按"字"、英文按"词"（单词+紧连标点+词后空格）各算一个揭示单位，
    /// 中英混排自动适配（如 "这是AI时代，Great!" 按 这/是/AI/时/代/，/Great!
    /// 逐单位揭示，与朗读节奏对上）。
    pub fn tick(&mut self, now_ms: u32) {
        if self.typing_rest.is_empty() {
            return;
        }
        if now_ms.wrapping_sub(self.last_type_ms) < config::TYPE_INTERVAL_MS {
            return;
        }
        self.last_type_ms = now_ms;

        // 本拍揭示单位（对应 C++ tickTypewriter 的单位计算）
        let bytes = self.typing_rest.as_bytes();
        let n = bytes.len();
        let mut i = 0;
        while i < n && bytes[i] == b' ' {
            i += 1; // 前导空格并入本单位
        }
        if i < n && bytes[i].is_ascii_graphic() {
            // 英文：连续 ASCII 可见字符（单词/数字+紧连标点）整体一次揭示，
            // 词后空格一并带出
            while i < n && bytes[i].is_ascii_graphic() {
                i += 1;
            }
            while i < n && bytes[i] == b' ' {
                i += 1;
            }
        } else if i < n {
            // 中文等非 ASCII：按 1 个 UTF-8 字符揭示
            // （typing_rest 恒为合法 UTF-8，len_utf8 即安全步进）
            let ch = self.typing_rest[i..].chars().next().unwrap();
            i += ch.len_utf8();
        }
        // n>0 时上述分支至少推进 1；min(n) 对应 C++ 的 i>n 收敛兜底
        let unit: String = self.typing_rest.drain(..i.min(n)).collect();

        // '\n' 在打字阶段就真正断行。C++ 是靠 LovyanGFX 的 print 顺带处理换行，
        // 我们的渲染层按"一行一矩形"绘制，若把 \n 当普通字符流过，它会占掉一个
        // 字宽（字形表里没有 U+000A 的字形）→ 行中出现空隙。
        if unit.trim() == "\n" {
            let color = self.cur_color;
            // 空行也要占一行（与 C++ pushLines 对空串的处理一致）
            let line = core::mem::take(&mut self.cur_line);
            self.push_line_raw(line, color);
            return;
        }

        // 换行判断：按整单位用真实字库像素宽度量（对应 C++ textW > SUB_W），
        // 放不下则整行入历史、新行以该单位开头 —— 英文以词为单位，
        // 单词不会再从中间折断。
        if font::text_width(&self.cur_line) + font::text_width(&unit) > SUB_W
            && !self.cur_line.is_empty()
        {
            let color = self.cur_color;
            let line = core::mem::take(&mut self.cur_line);
            self.push_line_raw(line, color);
        }
        self.cur_line.push_str(&unit);
    }

    // ---------- 供未来渲染层读取 ----------

    pub fn history(&self) -> &[SubtitleLine] {
        &self.history
    }

    pub fn cur_line(&self) -> &str {
        &self.cur_line
    }

    pub fn cur_color(&self) -> u16 {
        self.cur_color
    }

    pub fn is_typing(&self) -> bool {
        !self.typing_rest.is_empty() || !self.cur_line.is_empty()
    }

    // ---------- 内部 ----------

    /// 入历史：先按像素宽度折行（对应 C++ pushLines → wrapToLines）
    fn push_line(&mut self, text: String, color: u16) {
        for seg in wrap_to_lines(&text) {
            // 空行显示一个空格，与 C++ wrapToLines 的空行处理一致
            let seg = if seg.is_empty() { String::from(" ") } else { seg };
            self.history.push(SubtitleLine { text: seg, color });
        }
        while self.history.len() > MAX_HISTORY_LINES {
            self.history.remove(0);
        }
    }

    fn push_line_raw(&mut self, text: String, color: u16) {
        self.push_line(text, color);
    }
}

/// 按像素宽度把文本折成若干行（对应 C++ wrapToLines）：逐字符量宽，放不下就
/// 整字换行；'\n' 强制断行；末尾的空串也保留为一行，与 C++ 行为一致。
fn wrap_to_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch == '\n' {
            out.push(core::mem::take(&mut cur));
            continue;
        }
        let mut one = String::new();
        one.push(ch);
        if font::text_width(&cur) + font::text_width(&one) > SUB_W && !cur.is_empty() {
            out.push(core::mem::take(&mut cur));
        }
        cur.push(ch);
    }
    out.push(cur);
    out
}
