# Talk With Anyone · ESP32 小音箱固件（Rust 版）

[English](README.en.md) | 简体中文

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

本地优先语音助手 **[talk-with-anyone](https://github.com/wangguanghua2099/talk-with-anyone)**
前端的 `no_std` Rust 实现，跑在
[esp-hal](https://github.com/esp-rs/esp-hal) + [embassy](https://embassy.dev/) +
[esp-rtos](https://docs.espressif.com/projects/rust/esp-rtos/) 之上。协议与
[C++ 版固件](https://github.com/wangguanghua2099/talk-with-anyone-esp32) 完全一致，并已实机验证：

> 你说话 → 语音识别 → AI 出声回答 → 1.54 寸屏上中文实时字幕 → 自动回到聆听。

音频只在你自己的电脑和音箱之间走，不经过任何第三方云。

## 状态

下表的 ✅ 是**上机实测通过**；其余为已移植、还需对着 C++ 版做回归。

| 模块 | 状态 | 说明 |
|------|------|------|
| 开机自检（两短音 + 面板色带图） | ✅ 实测 | 用来把"面板时序"和"业务渲染"两类故障分开 |
| 自动聆听 / 打断 / 冷却 / 播完续听 | ✅ 实测 | 三态状态机全环 |
| 麦克风采集（I2S0 PDM RX，16 kHz 单声道，`<<5` 增益） | ✅ 实测 |
| 播放（I2S1 TDM 32bit、3 MB PSRAM 环形缓冲、24k→16k 重采样、音量曲线） | ✅ 实测 | 实测稳定 ≈61 kB/s，无欠载 |
| WebSocket 客户端（`/ws/voice`，RFC6455，自动重连） | ✅ 实测 | 仅明文 `ws://`，见[已知限制](#已知限制) |
| 流式字幕（打字机、折行、80 行滚动） | ✅ 实测 |
| ST7789 240×240 驱动 + 中文点阵字库 + 局部重绘 | ✅ 实测 |
| 顶栏 SNTP 时钟 / WiFi 信号柱 | ✅ 实测 |
| 顶栏电量（ADC2_CH6/GPIO17，30s 一采）+ 充电标志 | ✅ 实测 | S3 上 ADC2 与 WiFi 可共存；百分比量程待校准 |
| 音量键 | ✅ 实时生效 / ⬜ 不持久化 | 缺 NVS 存储 |
| `/ws/tts-stream` 兜底、语音打断（barge-in）、TLS | ⬜ 未实现 | |

## 硬件

开发与验证机型：**鹿小班第三代「小智」1.54 寸 WiFi 版**（ESP32-S3，8MB Flash + 8MB 八线 PSRAM）：

| 部件 | 规格 | 引脚 |
|------|------|------|
| 麦克风 | PDM 数字麦 | CLK=GPIO2, DATA=GPIO3 |
| 功放 | NS4168，标准 I2S | DOUT=7, BCLK=15, LRCK=16 |
| 屏幕 | ST7789 240×240 SPI | MOSI=10, SCLK=9, DC=8, CS=14, RST=18, 背光=13 |
| 按键 | BOOT / 音量+ / 音量− | GPIO0 / GPIO39 / GPIO40（均低有效） |
| 电源 | 电池电压 ADC2_CH6（GPIO17），充电检测 GPIO38 | |

引脚与行为常量在 [`src/config.rs`](src/config.rs)，Wi-Fi 与服务器信息在
`src/config_private.rs`（从 [`config_private_example.rs`](src/config_private_example.rs)
复制，**不入库**）。换板子通常只动这两个文件；面板不同才需要动 [`src/screen.rs`](src/screen.rs)。

## 快速开始

### 1. 工具链

```bash
cargo install espup --locked
espup install                      # 提供 rust-toolchain.toml 里固定的 `esp`（Xtensa）工具链
cargo install espflash --locked    # 4.x
```

### 2. 拉取随附的 esp-hal

`Cargo.toml` 依赖指向本地克隆而不是 crates.io：目前没有与 esp-hal 1.2 配套的
`esp-radio` 正式版，混用两个来源会在 `esp-rom-sys` / `xtensa-lx-rt` 这类带 `links`
键的 crate 上冲突。新克隆请先执行：

```bash
git clone --branch esp-hal-v1.2.0 --depth 1 https://github.com/esp-rs/esp-hal .deps/esp-hal
```

验证基线：tag `esp-hal-v1.2.0`（commit `dbd951f`）。`.deps/` 已被 gitignore。

### 3. 私密配置

```bash
cp src/config_private_example.rs src/config_private.rs
```

填写：

- `WIFI_SSID` / `WIFI_PASSWORD` —— 音箱要连的网络（与服务端同网段；手机热点也可以）
- `SERVER_HOST` —— 运行服务端机器的 **IPv4 字面量**。暂不做域名解析，建议做静态 DHCP 绑定
- `SERVER_PORT` —— 默认 `7862`
- `ACCESS_TOKEN` —— 服务端开启访问口令时必须一致，不使用则留空

`config_private.rs`（以及它的 `*.bak` 副本）刻意被 gitignore 挡住。

### 4. 编译与烧录

```bash
cargo run --release           # 编译 + espflash flash --monitor
```

分两步：

```bash
cargo build --release
espflash flash target/xtensa-esp32s3-none-elf/release/talk-with-anyone-esp32
```

接了多个串口设备时给 `espflash` 显式加 `-p COMx`。链接器的
`LOAD segment with RWX permissions` 警告是嵌入式构建常态，可忽略。

### 5. 开聊

**服务端要以 HTTP 模式启动，别用 HTTPS/WSS。** 后端 `main.py` 在自己的目录根下同时
看到 `cert.pem` 和 `key.pem` 就会带 `ssl_certfile`/`ssl_keyfile` 起 uvicorn，于是
浏览器能开、固件连不上 —— 本固件只说明文 `ws://`，TLS 握手第一步就对不上，串口表现为
`[Voice] 连接失败: Io` 反复重连。要跑 HTTP，把这两个证书文件临时移出后端目录（放到
任何子目录或其他位置都行，只要 `<后端目录>/cert.pem` 不再存在），聊完再放回去。
服务端启动日志会说明它走了哪条路：出现 `[MAIN] 检测到 HTTPS 证书，以 https://<本机IP>:7862 启动`
就是 HTTPS，没出现即 HTTP。这一条只约束 Rust 版；C++ 版走 `wss://`（不校验自签证书），
连 HTTPS 起的后端是正常的。

固件连上后会自动进入聆听。

- AI 说话时短按 BOOT —— 打断播放、字幕定格、继续聆听
- 聆听时短按 BOOT —— 退出对话（自动重听会遵守冷却）
- 音量 +/- —— 实时生效并带提示音，顶栏短暂显示读数

## 架构

esp-rtos 跑调度器，embassy 驱动全部异步任务。**所有任务共用一个执行器线程**，
这点对排障很关键（见[排障](#排障)）。

```
main 任务 (app_loop) ── 三态状态机 · 音频泵 · 按键 · 渲染层
voice_task           ── WS 事件分发 · 麦克风上行 · interrupt · client_stats
wifi_conn / net_task ── esp-radio 连接 + embassy-net runner
ntp_task             ── 先等 IP，再做 SNTP

 MIC ─I2S0 PDM─▶ 流式 DMA 环(16 kB) ─▶ MIC_CH 队列 ─▶ voice_task ─▶ /ws/voice
 /ws/voice ─▶ base64 解码 ─▶ 3 MB PSRAM 环形缓冲 ─▶ 重采样+音量 ─▶ I2S1 TDM ▶ 喇叭
 字幕状态 ─▶ render.rs 行级 diff ─▶ screen.rs ST7789（SPI2 + DMA，手工 CS）
```

| 模块 | 职责 |
|------|------|
| [`main.rs`](src/main.rs) | 硬件 bring-up、三态状态机、任务装配 |
| [`voice_client.rs`](src/voice_client.rs) | 报文构造、宽容 JSON 扫描、`RoundTracker` 轮次守卫 |
| [`ws.rs`](src/ws.rs) | embassy-net 之上的 RFC6455 客户端，取消安全 |
| [`audio_input.rs`](src/audio_input.rs) | PDM 采集、增益、流式 DMA 恢复 |
| [`audio_output.rs`](src/audio_output.rs) | PSRAM 环形缓冲、线性插值重采样、音量曲线、提示音 |
| [`display.rs`](src/display.rs) | 字幕数据模型：打字机、折行、历史、状态 |
| [`render.rs`](src/render.rs) | 字幕 + 顶栏的行级 diff 局部重绘 |
| [`screen.rs`](src/screen.rs) | ST7789 初始化序列、像素流推送、开机自检 |
| [`font.rs`](src/font.rs) | u8g2 格式 `efont CN` 14px 点阵解码 |
| [`ntp.rs`](src/ntp.rs) | 48 字节 SNTP 客户端，不引额外依赖 |

## 设计要点

这些地方不是一眼能看出来的，也都真实花掉了调试时间 —— 改之前请先读：

- **没有 framebuffer。** 渲染层只留一条 band（`240×22` RGB565 大端，约 10.5 kB），
  每次把发生变化的行作为矩形推给面板。每圈拿 12 个可见行的期望内容与上次绘制快照比对；
  打字时新文本通常是旧文本的前缀延长，于是只有新增字符那一段矩形会碰总线。
  不做整屏清屏，所以不闪。
- **`RAMWR` 是命令。** 它必须以 `DC=0` 发出，之后才能抬 `DC=1` 送像素流。写错的表现是
  面板收下全部初始化命令却仍然什么都不显示 —— 而且 CPU 写/DMA 写、40MHz/1MHz、
  硬件 CS/手工 CS 现象完全一样，能把人带走好几天。
- **CS 覆盖整个逻辑事务**（窗口 + `RAMWR` + 像素流），用普通 GPIO 手工拉，与 LovyanGFX
  一致；esp-hal 的硬件 CS 会按每次 `write()` 独立拉放。
- **`SpiDma` 的切片写接口必须先 `with_buffers()` 注册**，否则短包/非对齐数据会以
  `BufferTooSmall` 失败 —— 前提是你别把错误吞掉。本仓库里 SPI 写错误一律不 `let _ =`。
- **音频泵只探测、不忙等。** esp-hal 阻塞模式的 `wait()` 就是 `while !is_done() {}`，
  在同一个执行器线程上会把网络任务活活饿死（实测：音频喂不进来、环形缓冲抽干、
  播放停在开头）。每块武装一次后回到主循环节拍轮询完成位。欠载时填**整块静音** ——
  esp-hal 没有 cyclic 通道，这就是 IDF `tx_desc_auto_clear` 的等价物。
- **PDM 流式 DMA 不会自愈。** 主循环被长任务占住一段时间后通道溢出，`read()` 会恒返回 0；
  所以连续空读到阈值就重启通道。
- **WebSocket 读取是取消安全的。** `next_frame` 每圈和 50ms 定时器 select 竞争，以保证
  按键与麦克风上行的节拍，因此它可能在任意 await 点被整体丢弃。所有跨 await 的中间状态
  都落在 `self` 字段里（`stage`、`hdr_have`、`payload_have`、`discard_left`、
  `tx_pending`/`tx_sent`…）—— 半截帧放在局部变量里，一被取消就丢字节、流就失步。
- **协议解析刻意做得宽容**（平坦对象扫描器）而不是 serde 严格 tagged 枚举，与 ArduinoJson
  的探测式语义一致：未知字段、未知消息类型都不能让连接语义崩掉。
- **`RoundTracker` 对齐 C++ 的轮次守卫**：打断后迟到的内容整条丢弃；新的 `asr.result`
  隐式作废上一轮并**立即暂停上行**（"识别完→出声"那段只有噪声帧，会和下行音频抢带宽、
  推高首包延迟）；出错时恢复收音，避免停在"聆听中"却是聋的。
- **环形缓冲 3 MB，故意不是 2 的幂**，所以指针运算一律 `(x + cap - y) % cap`。
  小环在服务端两批在途时就会溢出丢样，长回复后半段跳字卡顿。
- **屏幕绝不能依赖网络。** 渲染层在开机约 0.8s 后接管面板，**早于 DHCP 完成**；
  一旦主任务先等地址，路由器慢一点，自检色带就变成"看起来像死机"。同理，一次 SPI 写失败
  不能把面板永久判死 —— `Renderer` 每 2 秒重发一次初始化，接管动作本身也有日志。
- **字库**是 262 kB 的 u8g2 格式 blob（`src/fonts/efont_cn_14.bin`），从 LovyanGFX 的
  `lgfx_efont_cn.c` 提取，逐位 RLE 解码。`include_bytes!` 让它留在 Flash。
  23 952 个码位的解码由 host 端离线回归覆盖：`render.rs`/`display.rs`/`font.rs` 除 blob 外
  不依赖任何硬件，用一个替身 `Screen` 就能在 PC 上跑完整渲染流程。

## 串口日志

115200 波特率，健康启动应当长这样：

```
[AudioOut] 环形缓冲 3072KB
[UI] 屏幕就绪 ST7789 240x240 (SPI2+DMA mode3 20MHz · 手工 CS · BGR=1)
[UI] 诊断图形已推送：白/红/绿/蓝、红绿竖条、灰阶、左青右黄
[AudioOut] 自检 1/2 (1200Hz)          ← 应当听到两短音
[UI] 渲染层已接管屏幕（开机色带已清除）  ← 渲染层拿到面板所有权
[WiFi] 已连接, IP: 192.168.1.23/24
[NTP] 对时成功 120.25.115.20
[电量] adc=3341 level=100 charging=0   ← 30s 一条；adc 是 12bit 原始值
[Voice] 已连接 /ws/voice，已发送 session.start
[ASR][t=10036] 你好呀。
[Voice][t=10922] LLM 开始流式输出
[Voice][t=11933] LLM首token→首音频块: 1011 ms
[AudioOut] 已输出 192KB                ← 约 184 kB / 3 s ≈ 61 kB/s
[AudioOut] 播放完成
```

中文日志标签与 C++ 版保持一致，方便两版固件逐行对账。

### 排障

| 现象 | 先看什么 | 大概率原因 |
|------|----------|------------|
| 屏幕停在四条色带 | 有没有 `[UI] 渲染层已接管屏幕` | 没有 → 主循环没走到渲染层（前面有阻塞 await，或别处的 panic 带走了执行器线程）。有但色带还在 → 面板写入问题，找 `[UI] SPI 写失败` / `[UI] 长度不匹配` |
| 屏幕雪花/全黑，日志正常 | 有没有 `[UI] 屏幕就绪` | 面板没进入"接收像素流"状态 —— 先查 `RAMWR` 的 `DC` 语义，再查 CS 覆盖范围与 MADCTL/`INVON` |
| 说话没反应 | 说话时的 `[麦克风] 峰值=` | 应当超过 10000。接近 0 → PDM 接线或 `MIC_GAIN_SHIFT`；反复出现 `[AudioIn] 连续无数据` → 流式 DMA 溢出（驱动会自己重启通道） |
| 回复卡顿/毛刺 | `[AudioOut] 已输出` 的增长速度 | 低于 ≈64 kB/s 说明泵被抽干了；查 `buffered()`、环容量、WiFi 信号 |
| 时钟显示 `--:--` | `[NTP] 对时成功` | 路由器拦了出站 UDP 123，或 `NTP_SERVERS` 全不可达 |
| `[Voice] 连接失败: Io` 一直刷 | 服务端是不是 HTTPS 起的，其次防火墙 | 客户端只说明文 `ws://`：后端目录根下还留着 `cert.pem`+`key.pem` 时，服务端会以 WSS 启动，握手第一步就连不上（处理办法见[开聊](#5-开聊)） |

**任何一个任务 panic 都会带走整个界面**：esp-rtos 下所有 embassy 任务共用一个执行器线程，
esp-backtrace 的 panic handler 打完就 `loop {}` 而不复位。于是屏幕冻在最后一帧上 ——
**日志的最后一行就是现场**，务必看尾部有没有 `panicked at ...`。

## 已知限制

- **没有 TLS。** 客户端只说 `ws://`，所以服务端必须以 HTTP 模式启动，见[开聊](#5-开聊)。
  C++ 版相反，它走的是 `wss://`（`beginSSL(host, port, path, nullptr, nullptr)`：CA 与指纹
  都不传 = 不校验自签证书），因此能直连 HTTPS 起的后端。Rust 侧缺的是 TLS 客户端本身。
- **`SERVER_HOST` 只接受 IPv4 字面量**，还没有 DNS 客户端。
- **音量不持久化** —— C++ 版用 NVS（`Preferences`）存，Rust 侧还缺 NVS 绑定。
- **电量百分比的量程还没校准。** S3 上 ADC2 与 WiFi **可以共存**（"WiFi 开启后 ADC2
  恒失败"是经典 ESP32 的限制，两版固件都实测能读），射频抢占的那一拍是无效值，用 5 次
  采样取中位数兜住。但实测满电时 `adc=3341`，已远超沿用 C++ 卖家标定表的 100% 点
  （2430），也就是现在这个 100% 是**表格上限饱和**的结果而非插值结果。放电一段时间后
  对照串口 `[电量] adc=…`：若原始值长期停在 2430 以上，百分比就没有区分度了，需要按
  多用电表实测重标 `battery.rs` 里的 `CAL` 表。
- **`/ws/tts-stream` 兜底是空实现**：流式服务端一般都在主通道推音频，实际不会触发。
- **没有语音打断**，打断请用 BOOT 键。
- 依赖 git 克隆的 esp-hal（见[第 2 步](#2-拉取随附的-esp-hal)），等 `esp-radio`
  有配套正式版后再切回 crates.io。

## 致谢

- [esp-hal](https://github.com/esp-rs/esp-hal) / [embassy](https://embassy.dev/)
  —— 本移植所依赖的 Rust 嵌入式栈
- [LovyanGFX](https://github.com/lovyan03/LovyanGFX) —— ST7789 面板配置，以及
  [`src/lgfx/Fonts/efont`](https://github.com/lovyan03/LovyanGFX/tree/master/src/lgfx/Fonts/efont)
  下的 `efont CN` 点阵数组；这些数组本身由 `/efont`（Electronic Font Open Laboratory）
  字体转换而来。随附的 262 kB blob 属于衍生数据，带有 `/efont`（BSD-3 风格）与
  LovyanGFX（FreeBSD）两份上游声明，原文见 [`licenses/`](licenses)
- [xiaozhi-esp32](https://github.com/78/xiaozhi-esp32) —— 硬件生态来源
- [talk-with-anyone](https://github.com/wangguanghua2099/talk-with-anyone) —— 配套服务端；
  [talk-with-anyone-esp32](https://github.com/wangguanghua2099/talk-with-anyone-esp32)
  —— 用于逐条对账的 C++ 参照固件

## 许可

代码：[MIT](LICENSE)。

中文点阵 blob [`src/fonts/efont_cn_14.bin`](src/fonts) 属衍生数据，保留上游声明，
两份许可原文随包放在 [`licenses/`](licenses)，分层说明见
[LICENSE-fonts.md](LICENSE-fonts.md)。
