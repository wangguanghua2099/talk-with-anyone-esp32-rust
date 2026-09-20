# Talk With Anyone · ESP32 Speaker Firmware (Rust)

English | [简体中文](README.md)

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A `no_std` Rust port of the Arduino firmware for the
**[talk-with-anyone](https://github.com/wangguanghua2099/talk-with-anyone)**
local-first voice assistant frontend, built on
[esp-hal](https://github.com/esp-rs/esp-hal), [embassy](https://embassy.dev/)
and [esp-rtos](https://docs.espressif.com/projects/rust/esp-rtos/).
It speaks the same streaming protocol as the
[C++ build](https://github.com/wangguanghua2099/talk-with-anyone-esp32) and
has been verified on real hardware:

> You speak → speech recognition → the AI answers out loud → live Chinese
> subtitles on the 1.54" display → back to listening, automatically.

All audio stays between your computer and the speaker — no third-party cloud.

## Status

Everything below the line is on-device verified; the rest is ported and needs a
regression pass against the C++ build.

| Area | State | Notes |
|------|-------|-------|
| Boot self-test (2 tones + panel colour-bar pattern) | ✅ verified | tells "panel timing" apart from "app rendering" |
| Auto-listen / interrupt / cooldown / resume-after-playback | ✅ verified | full three-state machine |
| Mic capture (I2S0 PDM RX, 16 kHz mono, `<<5` gain) | ✅ verified |
| Playback (I2S1 TDM 32-bit, 3 MB PSRAM ring, 24k→16k resampling, volume curve) | ✅ verified | measured ≈61 kB/s sustained, no underrun |
| WebSocket client (`/ws/voice`, RFC6455, auto-reconnect) | ✅ verified | plaintext `ws://` only — see [Limitations](#known-limitations) |
| Streaming subtitles (typewriter, wrap, 80-line scroll) | ✅ verified |
| ST7789 240×240 driver + Chinese bitmap font + partial redraw | ✅ verified |
| SNTP clock / Wi-Fi bars in the status bar | ✅ verified | battery shows `--%` (ADC2 conflicts with Wi-Fi) |
| Volume buttons | ✅ live, ⬜ not persisted | NVS storage still missing |
| `/ws/tts-stream` fallback, voice barge-in, TLS | ⬜ not implemented | |

## Hardware

Developed and verified on the **Luxiaoban 3rd-gen "Xiaozhi" 1.54" Wi-Fi board**
(ESP32-S3, 8 MB flash + 8 MB octal PSRAM):

| Part | Spec | Pins |
|------|------|------|
| Microphone | PDM digital mic | CLK=GPIO2, DATA=GPIO3 |
| Amplifier | NS4168, standard I2S | DOUT=7, BCLK=15, LRCK=16 |
| Display | ST7789 240×240 SPI | MOSI=10, SCLK=9, DC=8, CS=14, RST=18, backlight=13 |
| Buttons | BOOT / Vol+ / Vol− | GPIO0 / GPIO39 / GPIO40 (all active-low) |
| Battery | voltage on ADC2_CH6 (GPIO17), charge detect GPIO38 | |

Pins and behaviour constants live in [`src/config.rs`](src/config.rs); Wi-Fi and
server details in [`src/config_private.rs`](src/config_private_example.rs)
(git-ignored). Porting to another board touches those two files and, if the
panel differs, [`src/screen.rs`](src/screen.rs).

## Getting started

### 1. Toolchain

```bash
cargo install espup --locked
espup install                  # provides the pinned `esp` (Xtensa) toolchain
cargo install espflash --locked   # 4.x
```

`rust-toolchain.toml` pins the `esp` channel, so plain `cargo build` picks it up.

### 2. Fetch the vendored esp-hal

`Cargo.toml` builds against a local clone of esp-hal rather than crates.io,
because no published `esp-radio` release matches esp-hal 1.2 (mixing the two
sources breaks `links` keys such as `esp-rom-sys` / `xtensa-lx-rt`):

```bash
git clone --branch esp-hal-v1.2.0 --depth 1 https://github.com/esp-rs/esp-hal .deps/esp-hal
```

Verified against tag `esp-hal-v1.2.0` (commit `dbd951f`). `.deps/` is git-ignored.

### 3. Private config

```bash
cp src/config_private_example.rs src/config_private.rs
```

Fill in:

- `WIFI_SSID` / `WIFI_PASSWORD` — the network the speaker joins (same LAN as the
  server; a phone hotspot works too)
- `SERVER_HOST` — IPv4 literal of the machine running the server. Hostnames are
  not resolved yet; a static DHCP lease is recommended
- `SERVER_PORT` — `7862` by default
- `ACCESS_TOKEN` — append `?token=` to the WS path, leave empty if unused

`config_private.rs` (and any `*.bak` copy of it) is git-ignored on purpose.

### 4. Build and flash

```bash
cargo run --release           # build + espflash flash --monitor
```

Or in two steps:

```bash
cargo build --release
espflash flash target/xtensa-esp32s3-none-elf/release/talk-with-anyone-esp32
```

With several serial ports attached, pass `-p COMx` to `espflash` explicitly.
The linker's `LOAD segment with RWX permissions` warning is normal for
embedded builds.

### 5. Talk

The firmware connects and starts listening on its own.

- BOOT while the AI is talking — interrupt, flush subtitles, keep listening
- BOOT while listening — leave the conversation (auto-resume honours the cooldown)
- Vol+/Vol− — step 10, with a confirmation tone and a temporary status-bar readout

## Architecture

esp-rtos runs the scheduler; embassy drives all async tasks. **One executor
thread runs every task**, which matters when debugging (see
[Troubleshooting](#troubleshooting)).

```
main task (app_loop) ── three-state machine · audio pump · buttons · renderer
voice_task           ── WS events · mic uplink · interrupt · client_stats
wifi_conn / net_task ── esp-radio connect + embassy-net runner
ntp_task             ── waits for an address, then SNTP

 MIC ─I2S0 PDM─▶ ring(16 kB DMA stream) ─▶ MIC_CH queue ─▶ voice_task ─▶ /ws/voice
 /ws/voice ─▶ base64 decode ─▶ 3 MB PSRAM ring ─▶ resample+volume ─▶ I2S1 TDM ▶ SPEAKER
 Display state ─▶ render.rs diff ─▶ screen.rs ST7789 (SPI2 + DMA, manual CS)
```

| Module | Responsibility |
|--------|----------------|
| [`main.rs`](src/main.rs) | Bring-up, three-state machine, task wiring |
| [`voice_client.rs`](src/voice_client.rs) | Message building, lenient JSON scanning, `RoundTracker` |
| [`ws.rs`](src/ws.rs) | RFC6455 client over embassy-net, cancellation-safe |
| [`audio_input.rs`](src/audio_input.rs) | PDM capture, gain, stream-DMA recovery |
| [`audio_output.rs`](src/audio_output.rs) | PSRAM ring, linear resampling, volume curve, tones |
| [`display.rs`](src/display.rs) | Subtitle model: typewriter, wrapping, history, status |
| [`render.rs`](src/render.rs) | Row-diff partial redraw of subtitles + status bar |
| [`screen.rs`](src/screen.rs) | ST7789 init sequence, pixel streaming, self-test |
| [`font.rs`](src/font.rs) | u8g2-format `efont CN` 14 px glyph decoder |
| [`ntp.rs`](src/ntp.rs) | 48-byte SNTP client, no extra dependencies |

## Design notes

Things that were not obvious and cost real debugging time — keep them in mind
before editing:

- **No framebuffer.** The renderer keeps a single band (`240×22` RGB565 big
  endian, ≈10.5 kB) and pushes one rectangle per changed row. Row snapshots are
  diffed every pass; while typing, the new text is usually a prefix extension of
  the old one, so only the newly typed glyph box touches the bus. No full-screen
  clears, so nothing strobes.
- **`RAMWR` is a command.** It must go out with `DC=0`; only afterwards may the
  pixel stream be sent with `DC=1`. Getting this wrong produces a panel that
  accepts every init command and still shows nothing — and it looks identical for
  CPU writes, DMA writes, 40 MHz and 1 MHz, so it wastes days.
- **CS spans a whole logical transaction** (window + `RAMWR` + pixel stream) via a
  plain GPIO, mirroring LovyanGFX. esp-hal's hardware CS toggles per `write()`.
- **`SpiDma` slice writes need `with_buffers()` registered**, otherwise short or
  unaligned writes fail with `BufferTooSmall` *silently* unless you propagate the
  error. SPI write errors are never swallowed in this codebase.
- **The audio pump probes, it never busy-waits.** esp-hal's blocking `wait()` is
  `while !is_done() {}`, which starves the network work sharing the executor
  thread. Each block is re-armed once and then polled on the main loop's tick.
  Underruns are filled with a whole block of silence — esp-hal has no cyclic
  channel, and this replaces the IDF `tx_desc_auto_clear` behaviour.
- **PDM stream DMA does not recover by itself.** If the main loop is blocked too
  long the channel overflows and `read()` returns 0 forever; consecutive empty
  reads therefore restart the transfer.
- **The WebSocket reader is cancellation-safe.** `next_frame` is raced against a
  50 ms timer every pass so button presses and mic uplink keep their cadence, so
  it can be dropped at any await point. Every piece of cross-await state lives in
  `self` fields (`stage`, `hdr_have`, `payload_have`, `discard_left`,
  `tx_pending`/`tx_sent`, …) — keeping half a frame in a local would desynchronise
  the stream the moment it is cancelled.
- **The protocol parser is deliberately lenient** (a flat-object scanner) instead
  of a strict serde tagged enum, matching the probing style of ArduinoJson:
  unknown fields and unknown message types must not kill the connection.
- **`RoundTracker` mirrors the C++ round guard**: content arriving after an
  interrupt is discarded, a fresh `asr.result` implicitly voids the previous
  round and pauses the uplink immediately (the gap between "recognised" and
  "speaking" is pure noise that fights the downlink for bandwidth), and errors
  restore the microphone so the device never sits in "listening" while deaf.
- **The ring buffer is 3 MB — deliberately not a power of two**, so every pointer
  computation uses `(x + cap - y) % cap`. Small rings overflow when the server
  pushes two batches in flight and drop the tail of long replies.
- **The screen must never depend on the network.** The renderer takes the panel
  over ~0.8 s after boot, before DHCP finishes; if the main task waits for an
  address first, a slow router turns the self-test pattern into what looks like a
  dead device. Likewise a single SPI write error must not latch the panel off
  forever — `Renderer` re-inits it every 2 s and the takeover is logged.
- **Font data** is a 262 kB u8g2-format blob (`src/fonts/efont_cn_14.bin`)
  extracted from LovyanGFX's `lgfx_efont_cn.c`, decoded bit-by-bit with an RLE
  glyph parser. `include_bytes!` keeps it in flash; a scan of 23 952 code points
  is covered by an offline host-side harness (`render.rs`/`display.rs`/`font.rs`
  depend on nothing but the blob, so a stub `Screen` lets them run on a PC).

## Serial log

Every milestone is logged at 115200 baud (expect this shape on a healthy boot):

```
[AudioOut] 环形缓冲 3072KB
[UI] 屏幕就绪 ST7789 240x240 (SPI2+DMA mode3 20MHz · 手工 CS · BGR=1)
[UI] 诊断图形已推送：白/红/绿/蓝、红绿竖条、灰阶、左青右黄
[AudioOut] 自检 1/2 (1200Hz)          ← two short tones you should hear
[UI] 渲染层已接管屏幕（开机色带已清除）  ← the renderer owns the panel
[WiFi] 已连接, IP: 192.168.1.23/24
[NTP] 对时成功 120.25.115.20
[Voice] 已连接 /ws/voice，已发送 session.start
[ASR][t=10036] 你好呀。
[Voice][t=10922] LLM 开始流式输出
[Voice][t=11933] LLM首token→首音频块: 1011 ms
[AudioOut] 已输出 192KB                ← ≈184 kB / 3 s ≈ 61 kB/s
[AudioOut] 播放完成
```

Chinese log tags are shared with the C++ build on purpose, so results from the
two firmwares can be compared line by line.

### Troubleshooting

| Symptom | What to check | Likely cause |
|---------|---------------|--------------|
| Screen stuck on the four colour bars | Is `[UI] 渲染层已接管屏幕` present? | Missing → the main loop never reached the renderer (blocking await before it, or a panic elsewhere on the executor thread). Present but the bars stay → panel writes; look for `[UI] SPI 写失败` / `[UI] 长度不匹配` |
| Screen shows snow/nothing, logs normal | `[UI] 屏幕就绪` printed? | Panel not entering the pixel-stream state — check `DC` handling around `RAMWR`, then CS coverage and MADCTL/`INVON` |
| No response to speech | `[麦克风] 峰值=` while talking | Should exceed 10000. Near 0 → PDM wiring or `MIC_GAIN_SHIFT`; repeated `[AudioIn] 连续无数据` → stream DMA underrun (driver restarts it) |
| Stutter / hissy gaps in replies | Growth of `[AudioOut] 已输出` | Below ≈64 kB/s means the pump is starved; check `buffered()`, ring size, Wi-Fi signal |
| Clock shows `--:--` | `[NTP] 对时成功` | Router blocks outbound UDP 123, or all `NTP_SERVERS` unreachable |
| `[Voice] 连接失败: Io` forever | Firewall, server mode | The client speaks plaintext `ws://`; run the server in HTTP mode, and allow the port |

A panic in **any** task takes the whole UI down: esp-rtos gives all embassy tasks
one executor thread, and esp-backtrace's handler prints and then spins instead of
resetting. The screen freezes on the last drawn frame, so the **last log line is
the crime scene** — read the tail, including any `panicked at ...`.

## Known limitations

- **No TLS.** The client speaks `ws://`; `wss://` would need a TLS backend
  (the C++ build skips certificate validation for the same reason).
- **IPv4 literals only** for `SERVER_HOST`; no DNS client yet.
- **Volume is not persisted** — the C++ build stores it in NVS
  (`Preferences`), the Rust side still needs an NVS binding.
- **Battery level unavailable while Wi-Fi runs** (ADC2/GPIO17 conflicts with the
  radio on this chip, same as in the C++ build) — the status bar shows `--%`.
- **The `/ws/tts-stream` fallback is a stub**: the streaming server normally
  pushes audio on the main channel, so it never triggers in practice.
- **No voice barge-in**; interrupt with the BOOT button.
- Depends on a git clone of esp-hal (see [step 2](#2-fetch-the-vendored-esp-hal))
  until a matching `esp-radio` release lands on crates.io.

## Acknowledgements

- [esp-hal](https://github.com/esp-rs/esp-hal) / [embassy](https://embassy.dev/)
  — the Rust embedded stack this port rides on
- [LovyanGFX](https://github.com/lovyan03/LovyanGFX) — the ST7789 panel setup and
  the `efont CN` glyph arrays under
  [`src/lgfx/Fonts/efont`](https://github.com/lovyan03/LovyanGFX/tree/master/src/lgfx/Fonts/efont),
  which are themselves converted from the `/efont` Electronic Font Open
  Laboratory fonts. The bundled 262 kB blob is derivative data: it carries the
  upstream `/efont` (BSD-3-style) and LovyanGFX (FreeBSD) notices, reproduced
  verbatim in [`licenses/`](licenses) — see [License](#license)
- [xiaozhi-esp32](https://github.com/78/xiaozhi-esp32) — the hardware ecosystem
- [talk-with-anyone](https://github.com/wangguanghua2099/talk-with-anyone) — the
  companion server, and
  [talk-with-anyone-esp32](https://github.com/wangguanghua2099/talk-with-anyone-esp32)
  — the reference C++ firmware this port is validated against

## License

Code: [MIT](LICENSE).

The bitmap font blob [`src/fonts/efont_cn_14.bin`](src/fonts) is derivative data
and keeps its upstream notices; both license texts are shipped verbatim under
[`licenses/`](licenses). Details: [LICENSE-fonts.md](LICENSE-fonts.md).
