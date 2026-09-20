# Talk With Anyone · ESP32 小音箱前端 (Rust版)

[English](README.en.md) | 简体中文

本项目是 [talk-with-anyone](https://github.com/wangguanghua2099/talk-with-anyone) 的 ESP32-S3 语音聊天音箱固件的 Rust 重写版本（对应 C++ 版 [talk-with-anyone-esp32](https://github.com/wangguanghua2099/talk-with-anyone-esp32)）。

## 当前进度

协议契约以 C++ 仓库 `docs/交接文档-C++固件转Rust.md` 为准（流式协议版）。

| 模块 | 状态 | 说明 |
|------|------|------|
| 音频输出（I2S1 TDM 32bit + PSRAM 3MB 环形缓冲 + 重采样 + 音量曲线） | ✅ 已移植 | 与 C++ 流式版对齐（3MB/1.5MB/512KB 三级候选） |
| 音频输入（I2S0 PDM RX 16k + 软件增益） | ✅ 已移植 | 对应 `audio_in.cpp` |
| 语音协议（流式：delta/audio 并行、轮次守卫、client_stats 延迟上报、tts 兜底） | ✅ 已移植 | `RoundTracker` 与 C++ 逐条对应 |
| 字幕数据模型（流式打字机 143ms/单位·中英自适应 + 前缀对账 + 整词换行 + flush 定格） | ✅ 已移植 | 硬件绘制层待实现 |
| 硬件 bring-up（开机自检音、麦克风峰值监视、音量键） | ✅ **已实测通过** | 2026-09 实机验证：双短音、音量键反馈音、说话峰值上万 |
| WiFi + WebSocket 传输层 | ⬜ 待开发 | 协议层/轮次状态机已就绪，缺 esp-wifi/WS 客户端 |
| ST7789 驱动 + 中文字库 + 行级 diff 局部重绘 | ⬜ 待开发 | 字幕数据模型已就绪 |
| 三态状态机（聆听/播放/待机、打断、冷却） | ⬜ 待开发 | |
| 音量持久化（NVS）、电池电量、NTP 对时 | ⬜ 待开发 | |

## 硬件要求

- **开发板**：鹿小班第三代 · 小智 1.54 寸屏 WiFi 版（ESP32-S3，8MB Flash + 8MB PSRAM）
- **麦克风**：PDM 数字麦 (CLK=GPIO2, DATA=GPIO3)
- **功放**：NS4168 I2S (DOUT=7, BCLK=15, LRCK=16)
- **屏幕**：ST7789 240×240 SPI（暂未点亮）

## 烧录后如何验证（当前版本）

1. **开机两短音**：扬声器应播放 1200Hz + 1600Hz 自检音（验证 I2S 输出通路）
2. **麦克风峰值**：对麦克风说话，串口每 2 秒打印 `[麦克风] 峰值=xxx`，
   说话时应到 10000+（与 C++ 版排障口径一致）；按 BOOT 键可开/关收音监视
3. **音量键**：音量 +/- 实时生效并带提示音（串口打印 `[音量] xx%`）

## 开发环境

### 1. 安装 Rust 工具链

Xtensa 编译需要 esp-rs 定制工具链（项目已通过 `rust-toolchain.toml` 固定使用 `esp` 工具链）：

```bash
# 安装 espup 并生成 esp 工具链（含 xtensa-esp32s3 目标）
cargo install espup --locked
espup install

# 安装 espflash（烧录工具）
cargo install espflash --locked
```

### 2. 配置私密信息

```bash
cd src
cp config_private_example.rs config_private.rs
```

编辑 `config_private.rs`，填入：
- `WIFI_SSID` / `WIFI_PASSWORD`：音箱要连的 WiFi
- `SERVER_HOST`：运行服务端的电脑局域网 IPv4

（`config_private.rs` 已被 `.gitignore` 排除，真实密码不会入库）

### 3. 编译与烧录

**在项目根目录执行**（`D:\talk-with-anyone-esp32-rust`，不要进 target 子目录）：

```bash
# 编译（rust-toolchain.toml 会自动使用 esp 工具链）
cargo build --release

# 编译 + 烧录 + 直接打开串口日志（一条命令，Ctrl+] 退出监视）
cargo run --release

# 或者分开操作
cargo build --release
espflash flash target/xtensa-esp32s3-none-elf/release/talk-with-anyone-esp32
```

- 串口自动识别：只接了一个设备时无需指定端口；接了多个设备时用
  `espflash flash -p COM3 target/...`（`cargo run` 不方便传参，此时用分开操作）
- 链接器的 `LOAD segment with RWX permissions` 警告是嵌入式构建常态，可忽略

### 4. 查看串口日志

```bash
cargo run --release        # 烧录后自动进入监视（推荐）
espmonitor                 # 或单独看日志（115200）
```

## 项目结构

```
talk-with-anyone-esp32-rust/
├── Cargo.toml              # 项目配置和依赖（esp-hal / esp-alloc / esp-println）
├── rust-toolchain.toml     # 固定 esp 工具链（xtensa 编译必需）
├── .cargo/config.toml      # 目标与链接脚本（-Tlinkall.x 必需，勿删）
├── src/
│   ├── main.rs             # 硬件初始化 + bring-up 主循环（状态机待接入）
│   ├── config.rs           # 引脚定义、音频参数（对应 config.h）
│   ├── config_private.rs   # 私密配置（不入库，从 example 复制）
│   ├── voice_client.rs     # 语音协议：报文构造 + 宽容 JSON 解析（WS 客户端待接）
│   ├── audio_input.rs      # PDM 麦克风采集（I2S0 + DMA 流式缓冲）
│   ├── audio_output.rs     # I2S 播放（PSRAM 环形缓冲 + 重采样 + 音量曲线）
│   └── display.rs          # 屏幕字幕（占位，待实现）
└── README.md
```

## 与C++版本的对应关系

| C++文件 | Rust模块 | 移植状态 |
|---------|----------|----------|
| `config.h` | `config.rs` | ✅ 常量已对齐 |
| `voice_client.h/cpp` | `voice_client.rs` | ✅ 协议层 / ⬜ WS 传输层 |
| `audio_in.h/cpp` | `audio_input.rs` | ✅ 已移植 |
| `audio_out.h/cpp` | `audio_output.rs` | ✅ 已移植 |
| `display_ui.h/cpp` | `display.rs` | ⬜ 待开发 |
| `main.cpp` | `main.rs` | ⬜ 状态机待接入（bring-up 已通） |

## 实现要点（与 C++ 版的差异）

- **流式协议**：`/ws/voice` 主通道收 `assistant.delta`（字幕流式打字）与
  `audio.start/chunk/done`（边生成边播），文字与音频并行解耦；整轮没收到过音频时
  才走 `/ws/tts-stream` 兜底（发 `{"text":全文}`，不带 type 字段）
- **轮次守卫**：`RoundTracker` 复刻 C++ 的 round_active/finalized/received_audio/
  voice_audio_on 六态——打断后迟到的增量与音频块整条丢弃；新 `asr.result` 隐式作废
  上一轮，**并立即暂停麦克风上行**（识别完→出声之间只上行噪声帧，与下行音频争抢
  带宽会推高首包延迟），出错时自动恢复收音；`client_stats` 延迟上报每轮一次
- **环形缓冲 3MB**：服务端按批推音频（一批 48 字 ≈ 340KB@24k），512KB 小环在两批
  在途时溢出丢样；容量非 2 的幂，指针运算用 `(x + cap - y) % cap` 形式
- **I2S 常开**：DMA 队列空闲时推静音避免下溢爆音（C++ 靠 `tx_desc_auto_clear`）；
  起播 = 缓冲到 32KB 水位线后把真实音频顶到 DMA 队首（等价于 C++ 的起播语义）
- **协议解析**：不用 serde 严格枚举（未知消息会整条丢弃），改用宽容扫描器，
  与 C++ ArduinoJson 的探测式解析语义一致
- **麦克风驱动常驻**：仅启停 DMA 传输（C++ 是安装/卸载驱动），行为等价、开销更低

## 待完成事项

- [ ] WiFi（esp-wifi + embassy-net）与 wss/TLS 连接（或服务端加明文 ws 端口）
- [ ] WebSocket 客户端（RFC6455 帧收发）接入 voice_client 协议层
- [ ] base64 解码 + 主通道音频接线（audio.start/chunk/done → audio.feed，RoundTracker 守卫已就绪）
- [ ] ST7789 驱动 + 中文字库 + 行级 diff 局部重绘字幕
- [ ] 三态状态机（自动聆听/打断/冷却/超时）+ 音量 NVS 持久化
- [ ] 电池电量（ADC2）+ NTP 对时顶栏

## 参考资源

- [esp-hal 文档](https://docs.espressif.com/projects/rust/esp-hal/latest/)
- [Rust on ESP Book](https://docs.espressif.com/projects/rust/)
- [原C++项目](https://github.com/wangguanghua2099/talk-with-anyone-esp32)

## 许可证

MIT License
