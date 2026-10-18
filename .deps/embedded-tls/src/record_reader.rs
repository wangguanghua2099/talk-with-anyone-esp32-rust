use crate::key_schedule::ReadKeySchedule;
use embedded_io::{Error, Read as BlockingRead};
use embedded_io_async::Read as AsyncRead;

use crate::{
    TlsError,
    config::TlsCipherSuite,
    record::{RecordHeader, ServerRecord},
};

// ============================================================================
// 取消安全补丁（本仓库对 embedded-tls 0.19.0 的改动；升级指引见 PATCH-NOTES.md）
//
// upstream 的读流程有两个点不适用于"future 可能在任意 await 点被整体丢弃"的
// 调用方式（本仓库 ws.rs 的 next_frame 跑在 select(next_frame, 50ms 超时) 里）：
//   1. 记录头的接收进度（已读字节数）是 next_record_header 的局部变量；
//   2. advance() 重入时按整条记录体重算剩余量。
// 后果分别是：半截记录头字节丢失、半截记录体被越读进下一条记录，TLS 流错位。
//
// 补丁把阶段进度全部落到 RecordReadState（随 RecordReader 长期存活）：
//   * 记录头按 hdr/hdr_have 断点续读，取消后不丢已收字节；
//   * body_active 表示"头已读完、体未读完"，重入时直接重新解码头，不重读；
//   * advance() 剩余量按 amount - pending 重算，只读到本记录体末尾。
// ============================================================================

/// 记录读取阶段进度（跨 future 取消存活；见文件头"取消安全补丁"）
pub struct RecordReadState {
    /// 当前记录头的接收缓冲
    hdr: [u8; RecordHeader::LEN],
    /// 已收到的记录头字节数
    hdr_have: usize,
    /// 是否处于"记录体读取"阶段（记录头已读完）
    body_active: bool,
    /// 当前记录体长度（body_active 时有效）
    body_len: usize,
}

impl RecordReadState {
    pub(crate) const fn new() -> Self {
        Self {
            hdr: [0; RecordHeader::LEN],
            hdr_have: 0,
            body_active: false,
            body_len: 0,
        }
    }
}

pub struct RecordReader<'a> {
    pub(crate) buf: &'a mut [u8],
    /// The number of decoded bytes in the buffer
    decoded: usize,
    /// The number of read but not yet decoded bytes in the buffer
    pending: usize,
    /// 取消安全补丁：记录头/记录体阶段进度
    state: RecordReadState,
}

pub struct RecordReaderBorrowMut<'a> {
    pub(crate) buf: &'a mut [u8],
    /// The number of decoded bytes in the buffer
    decoded: &'a mut usize,
    /// The number of read but not yet decoded bytes in the buffer
    pending: &'a mut usize,
    /// 取消安全补丁：记录头/记录体阶段进度
    state: &'a mut RecordReadState,
}

impl<'a> RecordReader<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        if buf.len() < 16640 {
            warn!("Read buffer is smaller than 16640 bytes, which may cause problems!");
        }
        Self {
            buf,
            decoded: 0,
            pending: 0,
            state: RecordReadState::new(),
        }
    }

    pub fn reborrow_mut(&mut self) -> RecordReaderBorrowMut<'_> {
        RecordReaderBorrowMut {
            buf: self.buf,
            decoded: &mut self.decoded,
            pending: &mut self.pending,
            state: &mut self.state,
        }
    }

    pub async fn read<'m, CipherSuite: TlsCipherSuite>(
        &'m mut self,
        transport: &mut impl AsyncRead,
        key_schedule: &mut ReadKeySchedule<CipherSuite>,
    ) -> Result<ServerRecord<'m, CipherSuite>, TlsError> {
        read(
            self.buf,
            &mut self.decoded,
            &mut self.pending,
            &mut self.state,
            transport,
            key_schedule,
        )
        .await
    }

    pub fn read_blocking<'m, CipherSuite: TlsCipherSuite>(
        &'m mut self,
        transport: &mut impl BlockingRead,
        key_schedule: &mut ReadKeySchedule<CipherSuite>,
    ) -> Result<ServerRecord<'m, CipherSuite>, TlsError> {
        read_blocking(
            self.buf,
            &mut self.decoded,
            &mut self.pending,
            &mut self.state,
            transport,
            key_schedule,
        )
    }
}

impl RecordReaderBorrowMut<'_> {
    pub async fn read<'m, CipherSuite: TlsCipherSuite>(
        &'m mut self,
        transport: &mut impl AsyncRead,
        key_schedule: &mut ReadKeySchedule<CipherSuite>,
    ) -> Result<ServerRecord<'m, CipherSuite>, TlsError> {
        read(
            self.buf,
            self.decoded,
            self.pending,
            self.state,
            transport,
            key_schedule,
        )
        .await
    }

    pub fn read_blocking<'m, CipherSuite: TlsCipherSuite>(
        &'m mut self,
        transport: &mut impl BlockingRead,
        key_schedule: &mut ReadKeySchedule<CipherSuite>,
    ) -> Result<ServerRecord<'m, CipherSuite>, TlsError> {
        read_blocking(
            self.buf,
            self.decoded,
            self.pending,
            self.state,
            transport,
            key_schedule,
        )
    }
}

pub async fn read<'m, CipherSuite: TlsCipherSuite>(
    buf: &'m mut [u8],
    decoded: &mut usize,
    pending: &mut usize,
    state: &mut RecordReadState,
    transport: &mut impl AsyncRead,
    key_schedule: &mut ReadKeySchedule<CipherSuite>,
) -> Result<ServerRecord<'m, CipherSuite>, TlsError> {
    let header: RecordHeader = read_record_header(state, transport).await?;

    advance(
        buf,
        decoded,
        pending,
        transport,
        state.body_len,
    )
    .await?;
    state.body_active = false;
    state.hdr_have = 0;
    consume(
        buf,
        decoded,
        pending,
        header,
        key_schedule.transcript_hash(),
    )
}

pub fn read_blocking<'m, CipherSuite: TlsCipherSuite>(
    buf: &'m mut [u8],
    decoded: &mut usize,
    pending: &mut usize,
    state: &mut RecordReadState,
    transport: &mut impl BlockingRead,
    key_schedule: &mut ReadKeySchedule<CipherSuite>,
) -> Result<ServerRecord<'m, CipherSuite>, TlsError> {
    let header: RecordHeader = read_record_header_blocking(state, transport)?;

    advance_blocking(buf, decoded, pending, transport, state.body_len)?;
    state.body_active = false;
    state.hdr_have = 0;
    consume(
        buf,
        decoded,
        pending,
        header,
        key_schedule.transcript_hash(),
    )
}

/// 读取记录头（取消安全：进度落在 state，future 被丢弃后从断点续读，不丢字节）。
/// 若已进入记录体阶段（body_active），直接从头字节重新解码（解码幂等，不重读传输）。
async fn read_record_header(
    state: &mut RecordReadState,
    transport: &mut impl AsyncRead,
) -> Result<RecordHeader, TlsError> {
    if state.body_active {
        return RecordHeader::decode(state.hdr);
    }

    while state.hdr_have < RecordHeader::LEN {
        let read: usize = transport
            .read(&mut state.hdr[state.hdr_have..])
            .await
            .map_err(|e| TlsError::Io(e.kind()))?;
        if read == 0 {
            return Err(TlsError::IoError);
        }
        state.hdr_have += read;
    }

    let header = RecordHeader::decode(state.hdr)?;
    state.body_len = header.content_length();
    state.body_active = true;
    Ok(header)
}

fn read_record_header_blocking(
    state: &mut RecordReadState,
    transport: &mut impl BlockingRead,
) -> Result<RecordHeader, TlsError> {
    if state.body_active {
        return RecordHeader::decode(state.hdr);
    }

    while state.hdr_have < RecordHeader::LEN {
        let read: usize = transport
            .read(&mut state.hdr[state.hdr_have..])
            .map_err(|e| TlsError::Io(e.kind()))?;
        if read == 0 {
            return Err(TlsError::IoError);
        }
        state.hdr_have += read;
    }

    let header = RecordHeader::decode(state.hdr)?;
    state.body_len = header.content_length();
    state.body_active = true;
    Ok(header)
}

async fn advance(
    buf: &mut [u8],
    decoded: &mut usize,
    pending: &mut usize,
    transport: &mut impl AsyncRead,
    amount: usize,
) -> Result<(), TlsError> {
    ensure_contiguous(buf, decoded, pending, amount)?;

    // 取消安全补丁：重入时 pending 可能已含本记录体的前半截，
    // 剩余量按"记录体全长 - 已收字节"重算（upstream 恒为 amount，重入会越读）。
    let mut remain: usize = amount - *pending;
    while *pending < amount {
        let read = transport
            .read(&mut buf[*decoded + *pending..][..remain])
            .await
            .map_err(|e| TlsError::Io(e.kind()))?;
        if read == 0 {
            return Err(TlsError::IoError);
        }
        remain -= read;
        *pending += read;
    }

    Ok(())
}

fn advance_blocking(
    buf: &mut [u8],
    decoded: &mut usize,
    pending: &mut usize,
    transport: &mut impl BlockingRead,
    amount: usize,
) -> Result<(), TlsError> {
    ensure_contiguous(buf, decoded, pending, amount)?;

    let mut remain: usize = amount - *pending;
    while *pending < amount {
        let read = transport
            .read(&mut buf[*decoded + *pending..][..remain])
            .map_err(|e| TlsError::Io(e.kind()))?;
        if read == 0 {
            return Err(TlsError::IoError);
        }
        remain -= read;
        *pending += read;
    }

    Ok(())
}

fn consume<'m, CipherSuite: TlsCipherSuite>(
    buf: &'m mut [u8],
    decoded: &mut usize,
    pending: &mut usize,
    header: RecordHeader,
    digest: &mut CipherSuite::Hash,
) -> Result<ServerRecord<'m, CipherSuite>, TlsError> {
    let content_len = header.content_length();

    let slice = &mut buf[*decoded..][..content_len];

    *decoded += content_len;
    *pending -= content_len;

    ServerRecord::decode(header, slice, digest)
}

fn ensure_contiguous(
    buf: &mut [u8],
    decoded: &mut usize,
    pending: &mut usize,
    len: usize,
) -> Result<(), TlsError> {
    if *decoded + len > buf.len() {
        if len > buf.len() {
            error!(
                "Record too large for buffer. Size: {} Buffer size: {}",
                len,
                buf.len()
            );
            return Err(TlsError::InsufficientSpace);
        }
        buf.copy_within(*decoded..*decoded + *pending, 0);
        *decoded = 0;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;
    use core::future::Future;

    use super::*;
    use crate::{Aes128GcmSha256, content_types::ContentType, key_schedule::KeySchedule};

    struct ChunkRead<'a>(&'a [u8], usize);

    impl embedded_io::ErrorType for ChunkRead<'_> {
        type Error = Infallible;
    }

    impl BlockingRead for ChunkRead<'_> {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            let len = usize::min(self.1, buf.len());
            let len = usize::min(len, self.0.len());
            buf[..len].copy_from_slice(&self.0[..len]);
            self.0 = &self.0[len..];
            Ok(len)
        }
    }

    #[test]
    fn can_read_blocking() {
        can_read_blocking_case(1);
        can_read_blocking_case(2);
        can_read_blocking_case(3);
        can_read_blocking_case(4);
        can_read_blocking_case(5);
        can_read_blocking_case(6);
        can_read_blocking_case(7);
        can_read_blocking_case(8);
        can_read_blocking_case(9);
        can_read_blocking_case(10);
        can_read_blocking_case(11);
        can_read_blocking_case(12);
        can_read_blocking_case(13);
        can_read_blocking_case(14);
        can_read_blocking_case(15);
        can_read_blocking_case(16);
    }

    fn can_read_blocking_case(chunk_size: usize) {
        let mut transport = ChunkRead(
            &[
                // Header
                ContentType::ApplicationData as u8,
                0x03,
                0x03,
                0x00,
                0x04,
                // Data
                0xde,
                0xad,
                0xbe,
                0xef,
                // Header
                ContentType::ApplicationData as u8,
                0x03,
                0x03,
                0x00,
                0x02,
                // Data
                0xaa,
                0xbb,
            ],
            chunk_size,
        );

        let mut buf = [0; 32];
        let mut reader = RecordReader::new(&mut buf);
        let mut key_schedule = KeySchedule::<Aes128GcmSha256>::new();

        {
            if let ServerRecord::ApplicationData(data) = reader
                .read_blocking(&mut transport, key_schedule.read_state())
                .unwrap()
            {
                assert_eq!([0xde, 0xad, 0xbe, 0xef], data.data.as_slice());
            } else {
                panic!("Wrong server record");
            }

            assert_eq!(4, reader.decoded);
            assert_eq!(0, reader.pending);
        }

        {
            if let ServerRecord::ApplicationData(data) = reader
                .read_blocking(&mut transport, key_schedule.read_state())
                .unwrap()
            {
                assert_eq!([0xaa, 0xbb], data.data.as_slice());
            } else {
                panic!("Wrong server record");
            }

            assert_eq!(6, reader.decoded);
            assert_eq!(0, reader.pending);
        }
    }

    #[test]
    fn can_read_blocking_must_rotate_buffer() {
        let mut transport = [
            // Header
            ContentType::ApplicationData as u8,
            0x03,
            0x03,
            0x00,
            0x04,
            // Data
            0xde,
            0xad,
            0xbe,
            0xef,
            // Header
            ContentType::ApplicationData as u8,
            0x03,
            0x03,
            0x00,
            0x02,
            // Data
            0xaa,
            0xbb,
        ]
        .as_slice();

        let mut buf = [0; 4]; // cannot contain both data portions
        let mut reader = RecordReader::new(&mut buf);
        let mut key_schedule = KeySchedule::<Aes128GcmSha256>::new();

        {
            if let ServerRecord::ApplicationData(data) = reader
                .read_blocking(&mut transport, key_schedule.read_state())
                .unwrap()
            {
                assert_eq!([0xde, 0xad, 0xbe, 0xef], data.data.as_slice());
            } else {
                panic!("Wrong server record");
            }

            assert_eq!(4, reader.decoded);
            assert_eq!(0, reader.pending);
        }

        {
            if let ServerRecord::ApplicationData(data) = reader
                .read_blocking(&mut transport, key_schedule.read_state())
                .unwrap()
            {
                assert_eq!([0xaa, 0xbb], data.data.as_slice());
            } else {
                panic!("Wrong server record");
            }

            assert_eq!(2, reader.decoded);
            assert_eq!(0, reader.pending);
        }
    }

    #[test]
    fn can_read_empty_record() {
        let mut transport = [
            // Header
            ContentType::ApplicationData as u8,
            0x03,
            0x03,
            0x00,
            0x00,
            // Header
            ContentType::ApplicationData as u8,
            0x03,
            0x03,
            0x00,
            0x00,
        ]
        .as_slice();

        let mut buf = [0; 32];
        let mut reader = RecordReader::new(&mut buf);
        let mut key_schedule = KeySchedule::<Aes128GcmSha256>::new();

        {
            if let ServerRecord::ApplicationData(data) = reader
                .read_blocking(&mut transport, key_schedule.read_state())
                .unwrap()
            {
                assert!(data.data.is_empty());
            } else {
                panic!("Wrong server record");
            }

            assert_eq!(0, reader.decoded);
            assert_eq!(0, reader.pending);
        }

        {
            if let ServerRecord::ApplicationData(data) = reader
                .read_blocking(&mut transport, key_schedule.read_state())
                .unwrap()
            {
                assert!(data.data.is_empty());
            } else {
                panic!("Wrong server record");
            }

            assert_eq!(0, reader.decoded);
            assert_eq!(0, reader.pending);
        }
    }

    // ========================================================================
    // 取消安全补丁的回归测试（见文件头注释）：
    // 模拟"future 在任意 await 点被整体丢弃后重入"——mock 传输只按 budget
    // 放行字节，取完即 Pending；在关键断点（记录头半截、记录体半截）poll
    // 一次就把 future 丢掉，再重新调用 read()。
    // ========================================================================

    /// 按预算吐字节的异步传输：budget 用尽后 Pending（等"下一批数据到达"）
    struct ChunkedAsync<'a> {
        data: &'a [u8],
        budget: usize,
    }

    impl embedded_io::ErrorType for ChunkedAsync<'_> {
        type Error = Infallible;
    }

    impl AsyncRead for ChunkedAsync<'_> {
        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            core::future::poll_fn(|_| {
                if self.budget == 0 {
                    core::task::Poll::Pending
                } else {
                    let len = usize::min(self.budget, buf.len());
                    let len = usize::min(len, self.data.len());
                    buf[..len].copy_from_slice(&self.data[..len]);
                    self.data = &self.data[len..];
                    self.budget -= len;
                    core::task::Poll::Ready(Ok(len))
                }
            })
            .await
        }
    }

    fn noop_waker() -> core::task::Waker {
        use core::task::{RawWaker, RawWakerVTable};

        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(core::ptr::null(), &VTABLE)
        }
        fn noop(_: *const ()) {}

        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);

        unsafe { core::task::Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
    }

    /// poll 一次即返回（丢 future = 取消）
    fn poll_once<F: Future>(fut: core::pin::Pin<&mut F>) -> core::task::Poll<F::Output> {
        let waker = noop_waker();
        let mut cx = core::task::Context::from_waker(&waker);
        fut.poll(&mut cx)
    }

    /// 记录头半截、记录体半截被取消后，重入必须从断点续读，字节流不错位。
    #[test]
    fn async_read_survives_cancellation() {
        let data = [
            // 记录 1：头 + 4 字节体
            ContentType::ApplicationData as u8,
            0x03,
            0x03,
            0x00,
            0x04,
            0xde,
            0xad,
            0xbe,
            0xef,
            // 记录 2：头 + 2 字节体
            ContentType::ApplicationData as u8,
            0x03,
            0x03,
            0x00,
            0x02,
            0xaa,
            0xbb,
        ];
        let mut transport = ChunkedAsync {
            data: &data,
            budget: 0,
        };
        let mut buf = [0; 32];
        let mut reader = RecordReader::new(&mut buf);
        let mut key_schedule = KeySchedule::<Aes128GcmSha256>::new();

        // 断点 A：记录 1 的 5 字节头只放行 3 字节 → Pending → 丢 future
        transport.budget = 3;
        {
            let fut = reader.read(&mut transport, key_schedule.read_state());
            let mut fut = core::pin::pin!(fut);
            assert!(poll_once(fut.as_mut()).is_pending());
        }

        // 断点 B：补 2 字节头 + 1 字节体 → Pending → 丢 future（记录体半截）
        transport.budget = 3;
        {
            let fut = reader.read(&mut transport, key_schedule.read_state());
            let mut fut = core::pin::pin!(fut);
            assert!(poll_once(fut.as_mut()).is_pending());
        }

        // 恢复：记录 1 必须完整送达（体剩余量按 4-1 重算，不越读进记录 2）
        transport.budget = 100;
        {
            let fut = reader.read(&mut transport, key_schedule.read_state());
            let mut fut = core::pin::pin!(fut);
            if let core::task::Poll::Ready(Ok(ServerRecord::ApplicationData(d))) =
                poll_once(fut.as_mut())
            {
                assert_eq!([0xde, 0xad, 0xbe, 0xef], d.data.as_slice());
            } else {
                panic!("记录 1 应被完整读出");
            }
        }
        assert_eq!(4, reader.decoded);
        assert_eq!(0, reader.pending);

        // 记录 2：头再半截一次取消，恢复后同样应完整
        transport.budget = 2;
        {
            let fut = reader.read(&mut transport, key_schedule.read_state());
            let mut fut = core::pin::pin!(fut);
            assert!(poll_once(fut.as_mut()).is_pending());
        }
        transport.budget = 100;
        {
            let fut = reader.read(&mut transport, key_schedule.read_state());
            let mut fut = core::pin::pin!(fut);
            if let core::task::Poll::Ready(Ok(ServerRecord::ApplicationData(d))) =
                poll_once(fut.as_mut())
            {
                assert_eq!([0xaa, 0xbb], d.data.as_slice());
            } else {
                panic!("记录 2 应被完整读出");
            }
        }
        assert_eq!(6, reader.decoded);
        assert_eq!(0, reader.pending);
    }
}
