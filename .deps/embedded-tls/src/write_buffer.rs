use crate::{
    TlsError,
    buffer::CryptoBuffer,
    config::{TLS_RECORD_OVERHEAD, TlsCipherSuite},
    connection::encrypt,
    key_schedule::{ReadKeySchedule, WriteKeySchedule},
    record::{ClientRecord, ClientRecordHeader},
};

pub struct WriteBuffer<'a> {
    buffer: &'a mut [u8],
    pos: usize,
    current_header: Option<ClientRecordHeader>,
    /// 取消安全补丁（PATCH-NOTES.md）：已加密、待（续）发送的记录长度。
    /// close_record_pending 置位，flush 全部写出后清零。
    pending_len: usize,
    /// 取消安全补丁：pending 记录已写出的字节数
    pending_sent: usize,
}

pub(crate) struct WriteBufferBorrow<'a> {
    buffer: &'a [u8],
    pos: &'a usize,
    current_header: &'a Option<ClientRecordHeader>,
}

pub(crate) struct WriteBufferBorrowMut<'a> {
    buffer: &'a mut [u8],
    pos: &'a mut usize,
    current_header: &'a mut Option<ClientRecordHeader>,
    /// 取消安全补丁（PATCH-NOTES.md）：见 WriteBuffer 同名字段
    pending_len: &'a mut usize,
    pending_sent: &'a mut usize,
}

impl<'a> WriteBuffer<'a> {
    pub fn new(buffer: &'a mut [u8]) -> Self {
        debug_assert!(
            buffer.len() > TLS_RECORD_OVERHEAD,
            "The write buffer must be sufficiently large to include the tls record overhead"
        );
        Self {
            buffer,
            pos: 0,
            current_header: None,
            pending_len: 0,
            pending_sent: 0,
        }
    }

    pub(crate) fn reborrow_mut(&mut self) -> WriteBufferBorrowMut<'_> {
        WriteBufferBorrowMut {
            buffer: self.buffer,
            pos: &mut self.pos,
            current_header: &mut self.current_header,
            pending_len: &mut self.pending_len,
            pending_sent: &mut self.pending_sent,
        }
    }

    pub(crate) fn reborrow(&self) -> WriteBufferBorrow<'_> {
        WriteBufferBorrow {
            buffer: self.buffer,
            pos: &self.pos,
            current_header: &self.current_header,
        }
    }

    pub fn is_full(&self) -> bool {
        self.reborrow().is_full()
    }

    pub fn append(&mut self, buf: &[u8]) -> usize {
        self.reborrow_mut().append(buf)
    }

    pub fn is_empty(&self) -> bool {
        self.reborrow().is_empty()
    }

    pub fn contains(&self, header: ClientRecordHeader) -> bool {
        self.reborrow().contains(header)
    }

    pub(crate) fn start_record(&mut self, header: ClientRecordHeader) -> Result<(), TlsError> {
        self.reborrow_mut().start_record(header)
    }

    pub(crate) fn close_record<CipherSuite>(
        &mut self,
        write_key_schedule: &mut WriteKeySchedule<CipherSuite>,
    ) -> Result<&[u8], TlsError>
    where
        CipherSuite: TlsCipherSuite,
    {
        close_record(
            self.buffer,
            &mut self.pos,
            &mut self.current_header,
            write_key_schedule,
        )
    }

    /// 取消安全补丁（PATCH-NOTES.md）：加密当前记录并转入"待续发"状态（flush 专用）。
    /// 与 close_record 的唯一区别：把密文长度记到 pending_len/pending_sent，
    /// 使 buffer[0..pending_len] 的密文体可以由 flush 断点续发；握手路径不经过这里。
    pub(crate) fn close_record_pending<CipherSuite>(
        &mut self,
        write_key_schedule: &mut WriteKeySchedule<CipherSuite>,
    ) -> Result<(), TlsError>
    where
        CipherSuite: TlsCipherSuite,
    {
        let len = self.close_record(write_key_schedule)?.len();
        self.pending_len = len;
        self.pending_sent = 0;
        Ok(())
    }

    /// 取消安全补丁：待续发记录的剩余字节数
    pub fn pending_remaining(&self) -> usize {
        self.pending_len - self.pending_sent
    }

    /// 取消安全补丁：待续发记录中尚未写出的切片
    pub fn pending_slice(&self) -> &[u8] {
        &self.buffer[self.pending_sent..self.pending_len]
    }

    /// 取消安全补丁：推进待续发进度
    pub fn advance_pending(&mut self, len: usize) {
        self.pending_sent += len;
    }

    /// 取消安全补丁：待续发记录已全部写出
    pub fn clear_pending(&mut self) {
        self.pending_len = 0;
        self.pending_sent = 0;
    }

    pub fn write_record<CipherSuite>(
        &mut self,
        record: &ClientRecord<CipherSuite>,
        write_key_schedule: &mut WriteKeySchedule<CipherSuite>,
        read_key_schedule: Option<&mut ReadKeySchedule<CipherSuite>>,
    ) -> Result<&[u8], TlsError>
    where
        CipherSuite: TlsCipherSuite,
    {
        write_record(
            self.buffer,
            &mut self.pos,
            &mut self.current_header,
            record,
            write_key_schedule,
            read_key_schedule,
        )
    }
}

impl WriteBufferBorrow<'_> {
    fn max_block_size(&self) -> usize {
        self.buffer.len() - TLS_RECORD_OVERHEAD
    }

    pub fn is_full(&self) -> bool {
        *self.pos == self.max_block_size()
    }

    pub fn len(&self) -> usize {
        *self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn space(&self) -> usize {
        self.max_block_size() - *self.pos
    }

    pub fn contains(&self, header: ClientRecordHeader) -> bool {
        self.current_header.as_ref() == Some(&header)
    }
}

impl WriteBufferBorrowMut<'_> {
    fn reborrow(&self) -> WriteBufferBorrow<'_> {
        WriteBufferBorrow {
            buffer: self.buffer,
            pos: self.pos,
            current_header: self.current_header,
        }
    }

    pub fn is_full(&self) -> bool {
        self.reborrow().is_full()
    }

    pub fn is_empty(&self) -> bool {
        self.reborrow().is_empty()
    }

    pub fn contains(&self, header: ClientRecordHeader) -> bool {
        self.reborrow().contains(header)
    }

    pub fn append(&mut self, buf: &[u8]) -> usize {
        let buffered = usize::min(buf.len(), self.reborrow().space());
        if buffered > 0 {
            self.buffer[*self.pos..*self.pos + buffered].copy_from_slice(&buf[..buffered]);
            *self.pos += buffered;
        }
        buffered
    }

    pub(crate) fn start_record(&mut self, header: ClientRecordHeader) -> Result<(), TlsError> {
        start_record(self.buffer, self.pos, self.current_header, header)
    }

    pub fn close_record<CipherSuite>(
        &mut self,
        write_key_schedule: &mut WriteKeySchedule<CipherSuite>,
    ) -> Result<&[u8], TlsError>
    where
        CipherSuite: TlsCipherSuite,
    {
        close_record(
            self.buffer,
            self.pos,
            self.current_header,
            write_key_schedule,
        )
    }

    /// 取消安全补丁（PATCH-NOTES.md）：见 WriteBuffer::close_record_pending
    pub(crate) fn close_record_pending<CipherSuite>(
        &mut self,
        write_key_schedule: &mut WriteKeySchedule<CipherSuite>,
    ) -> Result<(), TlsError>
    where
        CipherSuite: TlsCipherSuite,
    {
        let len = self.close_record(write_key_schedule)?.len();
        *self.pending_len = len;
        *self.pending_sent = 0;
        Ok(())
    }

    /// 取消安全补丁：待续发记录的剩余字节数
    pub fn pending_remaining(&self) -> usize {
        *self.pending_len - *self.pending_sent
    }

    /// 取消安全补丁：待续发记录中尚未写出的切片
    pub fn pending_slice(&self) -> &[u8] {
        &self.buffer[*self.pending_sent..*self.pending_len]
    }

    /// 取消安全补丁：推进待续发进度
    pub fn advance_pending(&mut self, len: usize) {
        *self.pending_sent += len;
    }

    /// 取消安全补丁：待续发记录已全部写出
    pub fn clear_pending(&mut self) {
        *self.pending_len = 0;
        *self.pending_sent = 0;
    }
}

fn start_record(
    buffer: &mut [u8],
    pos: &mut usize,
    current_header: &mut Option<ClientRecordHeader>,
    header: ClientRecordHeader,
) -> Result<(), TlsError> {
    debug_assert!(current_header.is_none());

    debug!("start_record({:?})", header);
    *current_header = Some(header);

    with_buffer(buffer, pos, |mut buf| {
        header.encode(&mut buf)?;
        buf.push_u16(0)?;
        Ok(buf.rewind())
    })
}

fn with_buffer(
    buffer: &mut [u8],
    pos: &mut usize,
    op: impl FnOnce(CryptoBuffer) -> Result<CryptoBuffer, TlsError>,
) -> Result<(), TlsError> {
    let buf = CryptoBuffer::wrap_with_pos(buffer, *pos);

    match op(buf) {
        Ok(buf) => {
            *pos = buf.len();
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn close_record<'a, CipherSuite>(
    buffer: &'a mut [u8],
    pos: &mut usize,
    current_header: &mut Option<ClientRecordHeader>,
    write_key_schedule: &mut WriteKeySchedule<CipherSuite>,
) -> Result<&'a [u8], TlsError>
where
    CipherSuite: TlsCipherSuite,
{
    const HEADER_SIZE: usize = 5;

    let header = current_header.take().unwrap();
    with_buffer(buffer, pos, |mut buf| {
        if !header.is_encrypted() {
            return Ok(buf);
        }

        buf.push(header.trailer_content_type() as u8)
            .map_err(|_| TlsError::EncodeError)?;

        let mut buf = buf.offset(HEADER_SIZE);
        encrypt(write_key_schedule, &mut buf)?;
        Ok(buf.rewind())
    })?;
    let [upper, lower] = ((*pos - HEADER_SIZE) as u16).to_be_bytes();

    buffer[3] = upper;
    buffer[4] = lower;

    let slice = &buffer[..*pos];

    *pos = 0;
    *current_header = None;

    Ok(slice)
}

fn write_record<'a, CipherSuite>(
    buffer: &'a mut [u8],
    pos: &mut usize,
    current_header: &mut Option<ClientRecordHeader>,
    record: &ClientRecord<CipherSuite>,
    write_key_schedule: &mut WriteKeySchedule<CipherSuite>,
    read_key_schedule: Option<&mut ReadKeySchedule<CipherSuite>>,
) -> Result<&'a [u8], TlsError>
where
    CipherSuite: TlsCipherSuite,
{
    if current_header.is_some() {
        return Err(TlsError::InternalError);
    }

    start_record(buffer, pos, current_header, record.header())?;
    with_buffer(buffer, pos, |buf| {
        let mut buf = buf.forward();
        record.encode_payload(&mut buf)?;

        let transcript = read_key_schedule
            .ok_or(TlsError::InternalError)?
            .transcript_hash();

        record.finish_record(&mut buf, transcript, write_key_schedule)?;
        Ok(buf.rewind())
    })?;
    close_record(buffer, pos, current_header, write_key_schedule)
}
