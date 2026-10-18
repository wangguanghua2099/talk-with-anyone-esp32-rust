# PATCH-NOTES：本目录是打过补丁的 embedded-tls 0.19.0

> 来源：crates.io `embedded-tls 0.19.0`（原始拷贝，仅做下述改动）。
> 用途：esp32 小音箱 Rust 固件的 wss 支持；补丁动机见下文。
> **升级 embedded-tls 时：逐条对照本文，重新套用补丁。**

## 为什么需要补丁

本固件把 `ws.next_frame()`（内部会走到 `TlsConnection::read`）放在
`select(next_frame(...), Timer::after_millis(50))` 里跑：50ms 定时先到就整体丢弃
`next_frame` 的 future，下一轮再重新调用。也就是说 **TLS 读写 future 在任意 await 点
都可能被整体取消（drop）**。

upstream 0.19.0 的读写路径都不是取消安全的：

1. **读记录头**：`next_record_header` 把"已收字节数"存在局部变量里。取消后已收到的
   半截记录头字节丢失，下次调用从头再读 → 字节流错位。
2. **读记录体**：`advance()` 每次调用都把"剩余量"重算为整条记录体长度。若上次读到一半
   被取消，重入后会把下一条记录的字节当作本条继续读 → 越读、流错位。
3. **写记录**：`flush()` 先 `close_record()`（加密完成、缓冲游标清零），再用
   `write_all(slice).await` 发送。取消发生在写一半时：下次 `flush()` 已看不到
   "还有一条密文没发完" → 半截记录永久丢失（或重发导致计数器与流量不符）→ 连接必死。

## 改动清单（对照 upstream diff 用）

### 1. `src/record_reader.rs` —— 读路径阶段化（取消安全）

- 新增 `pub struct RecordReadState`（随 `RecordReader` 长期存活）：
  - `hdr: [u8; 5]` / `hdr_have: usize`：记录头接收缓冲与进度；
  - `body_active: bool`：头已读完、体未读完；
  - `body_len: usize`：当前记录体长度。
- `RecordReader` / `RecordReaderBorrowMut` 持有（或按可变引用持有）该 state；
  `read` / `read_blocking` 自由函数与 `RecordReader(BorrowMut)` 方法都随之新增参数。
- `next_record_header` → `read_record_header(state, transport)`：
  - `body_active == true` 时直接 `RecordHeader::decode(state.hdr)`（解码幂等，不重读传输）；
  - 否则按 `hdr_have` 断点续读，进度落盘。
- `advance(_blocking)`：`let mut remain = amount - *pending;`（upstream 恒为 `amount`）。
- `read/read_blocking`：advance 返回后清零 `body_active`/`hdr_have` 再 `consume`。

### 2. `src/write_buffer.rs` —— 写路径 pending 机制

- `WriteBuffer` / `WriteBufferBorrowMut` 新增 `pending_len` / `pending_sent`：
  已加密但未全部写出的记录长度、已写出字节数。
- 新增 `close_record_pending()`（仅 flush 路径使用；**握手路径继续用原
  `close_record()`，语义完全不变**）：加密完成后记录 `pending_len`、`pending_sent = 0`。
- 新增访问器：`pending_remaining()` / `pending_slice()` / `advance_pending(n)` /
  `clear_pending()`。

### 3. `src/asynch.rs` —— flush 断点续发

`TlsConnection::flush` 与 `TlsWriter::flush` 重写为：

1. 缓冲非空 → `close_record_pending()` 加密并转入 pending；
   **`increment_counter()` 立即执行**（加密完成即计数；续发不会二次加密，不复用 nonce）；
2. `while pending_remaining() > 0 { write(pending_slice()) + advance_pending(n) }`
   —— 取消后从断点续写同一段密文；
3. `clear_pending()`；
4. 照旧按 `flush_policy` 冲刷传输层。

### 4. `Cargo.toml` —— dev-dependencies 精简（仅为本地单测）

- 只保留 `[dev-dependencies.env_logger]`（`key_share.rs` 单测用），其余
  （tokio/openssl/rustls/mio/serde/log…）删除。
- 理由：它们只服务于 `tests/` 集成测试；且 **dev-dependencies 不会被固件构建
  解析**，删掉对主工程零影响，换来宿主机离线跑 lib 单测。
- 将来要跑 `tests/` 集成测试时，自行对照 upstream Cargo.toml 补回。

## 未改动的部分（别动）

- 握手路径（`connection.rs` 的 `State::process` / `Handshake`）：握手不在 select 里跑，
  不存在取消；`write_record` / `close_record` / `write_all` 均保持 upstream 原样。
  以上 3 个代码补丁互不牵连，升级 diff 保持最小。
- flush 里把 `write_all` 改为 `write` + 断点推进是刻意的：`write_all` 内部循环的进度
  同样会在取消时丢失。

## 刻意不做的事

- 没有引入 `into_inner()` / 复用底层 socket：本固件 TLS 重建策略是"整体 drop
  （连同 TcpSocket），下次用静态缓冲重建"，不需要取回内部对象。
- 没有把发送挪出 select 上下文：播放中确实会在 select 超时点丢弃发送 future
  （打断场景），写路径的取消安全必须做，而不是绕开。
- 没有启用 `std` / `webpki` / `rsa` 等特性：跳过证书校验（与 C++ 版
  `beginSSL(..., nullptr, nullptr)` 同策略）；RSA 只是在 ClientHello 里
  `enable_rsa_signatures()` 通告能力，不需要本地验签实现。

## 怎么验证（宿主机单测）

在主仓库**之外**的任意目录执行（避免命中主仓库根的 `.cargo/config.toml`——它把
target 指到 xtensa）：

```bash
cargo test --manifest-path <仓库>/.deps/embedded-tls/Cargo.toml --lib --no-default-features
```

- 14 个单测全过即基线正常（buffer / read_buffer / key_share / record_reader）。
- 其中 `record_reader::tests::async_read_survives_cancellation` 是**取消安全补丁的
  回归测试**：模拟 future 在"记录头半截 / 记录体半截"被整体丢弃后重入，断言读出
  的记录内容完整、不越读进下一条记录。
- 依赖只有 `env_logger`（唯一 dev-dep）；离线环境需其已在 cargo 缓存。
