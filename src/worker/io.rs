use std::{
    io,
    pin::Pin,
    sync::{Arc, atomic::{AtomicU64, Ordering}},
    task::{Context, Poll},
};

use wasmer_wasix::virtual_fs::{AsyncRead, AsyncSeek, AsyncWrite, FsError, Result, VirtualFile};

use crate::types::{StdoutMode, WorkerOut};

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ Stdin                                                                    │
// ╰──────────────────────────────────────────────────────────────────────────╯

// Must match TypeScript StdinStream constants
const HEADER_SIZE: u32 = 8;
const READ_IDX: u32 = 0;
const WRITE_IDX: u32 = 1;

#[derive(Debug)]
pub struct Stdin {
    indices: js_sys::Int32Array,
    data: js_sys::Uint8Array,
}

unsafe impl Send for Stdin {}
unsafe impl Sync for Stdin {}

impl Stdin {
    pub fn new(stdin_buffer: &js_sys::SharedArrayBuffer) -> Self {
        // Create typed array views over the SharedArrayBuffer
        let indices = js_sys::Int32Array::new_with_byte_offset_and_length(
            stdin_buffer,
            0,
            2, // 2 i32s: read_index and write_index
        );
        let data = js_sys::Uint8Array::new_with_byte_offset(
            stdin_buffer,
            HEADER_SIZE
        );
        Self { indices, data }
    }
}

impl AsyncWrite for Stdin {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot write to stdin",
        )))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot flush stdin",
        )))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl AsyncRead for Stdin {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut wasmer_wasix::virtual_fs::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Empty reads must complete without waiting for stdin data.
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        let read_idx =
            js_sys::Atomics::load(&self.indices, READ_IDX).expect("Loaded read_idx") as u32;
        let write_idx =
            js_sys::Atomics::load(&self.indices, WRITE_IDX).expect("Loaded write_idx") as u32;

        // Contiguous bytes available before the buffer wraps.
        let available = if read_idx <= write_idx {
            write_idx - read_idx
        } else {
            self.data.length() - read_idx
        };

        if available == 0 {
            // Wait on the write index
            js_sys::Atomics::wait(&self.indices, WRITE_IDX, write_idx as i32)
                .expect("Waited on write_idx");
            return self.poll_read(_cx, buf);
        }

        // Read contiguous chunk only
        let to_read = std::cmp::min(available, buf.remaining() as u32);
        let slice = self.data.slice(read_idx, read_idx + to_read);
        buf.put_slice(slice.to_vec().as_slice());

        // Update read index atomically
        let new_read_idx = (read_idx + to_read) % self.data.length();
        js_sys::Atomics::store(&self.indices, READ_IDX, new_read_idx as i32)
            .expect("Stored new read_idx");

        // Notify TypeScript that we consumed data
        js_sys::Atomics::notify(&self.indices, READ_IDX).expect("Notified main thread");

        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for Stdin {
    fn start_seek(self: Pin<&mut Self>, _position: io::SeekFrom) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot seek stdin",
        ))
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot seek stdin",
        )))
    }
}

impl VirtualFile for Stdin {
    fn last_accessed(&self) -> u64 {
        0
    }
    fn last_modified(&self) -> u64 {
        0
    }
    fn created_time(&self) -> u64 {
        0
    }
    fn size(&self) -> u64 {
        0
    }

    fn set_len(&mut self, _new_size: u64) -> Result<()> {
        Err(FsError::Unsupported)
    }

    fn unlink(&mut self) -> Result<()> {
        Ok(())
    }

    fn get_special_fd(&self) -> Option<u32> {
        Some(0)
    }

    fn poll_read_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(0))
    }

    fn poll_write_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot write to stdin",
        )))
    }
}

// ╭──────────────────────────────────────────────────────────────────────────╮
// │ Stdout                                                                   │
// ╰──────────────────────────────────────────────────────────────────────────╯

#[derive(Debug)]
pub struct Stdout {
    mode: StdoutMode,
    byte_count: Option<Arc<AtomicU64>>,
}

impl Stdout {
    pub fn new_counted(mode: StdoutMode, byte_count: Arc<AtomicU64>) -> Self {
        Self { mode, byte_count: Some(byte_count) }
    }
}

impl AsyncWrite for Stdout {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(byte_count) = &self.byte_count {
            byte_count.fetch_add(buf.len() as u64, Ordering::Relaxed);
        }
        WorkerOut::Stdout {
            data: buf,
            mode: self.mode,
        }
        .send();
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl AsyncRead for Stdout {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut wasmer_wasix::virtual_fs::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot read from stdout",
        )))
    }
}

impl AsyncSeek for Stdout {
    fn start_seek(self: Pin<&mut Self>, _position: io::SeekFrom) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot seek from stdout",
        ))
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot seek from stdout",
        )))
    }
}

impl VirtualFile for Stdout {
    fn last_accessed(&self) -> u64 {
        0
    }
    fn last_modified(&self) -> u64 {
        0
    }
    fn created_time(&self) -> u64 {
        0
    }
    fn size(&self) -> u64 {
        0
    }

    fn set_len(&mut self, _new_size: u64) -> Result<()> {
        Err(FsError::Unsupported)
    }

    fn unlink(&mut self) -> Result<()> {
        Err(FsError::Unsupported)
    }

    fn get_special_fd(&self) -> Option<u32> {
        Some(if let StdoutMode::Err = self.mode {
            2
        } else {
            1
        })
    }

    fn poll_read_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cannot read from stdout",
        )))
    }

    fn poll_write_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(usize::MAX))
    }
}
