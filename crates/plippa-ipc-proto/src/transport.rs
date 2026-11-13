//! Unix domain socket client / server helpers for [`IpcMessage`] frames.

use crate::frame::{encode_frame, try_pop_frame, FrameError};
use crate::paths::{ensure_ipc_dir, remove_stale_sock, IpcPaths};
use crate::IpcMessage;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Transport-level errors.
#[derive(Debug, Error)]
pub enum IpcTransportError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("peer closed the connection")]
    Disconnected,
    #[error("timed out waiting for a frame")]
    Timeout,
}

/// Connected peer with a read buffer for partial frames.
#[derive(Debug)]
pub struct IpcConnection {
    stream: UnixStream,
    read_buf: Vec<u8>,
}

impl IpcConnection {
    pub fn from_stream(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            read_buf: Vec::with_capacity(256),
        })
    }

    /// Connect to an existing engine (or peer) socket.
    pub fn connect(path: &Path) -> Result<Self, IpcTransportError> {
        let stream = UnixStream::connect(path)?;
        Ok(Self::from_stream(stream)?)
    }

    /// Retry connect until `timeout` elapses (engine may start after clients).
    pub fn connect_timeout(path: &Path, timeout: Duration) -> Result<Self, IpcTransportError> {
        let deadline = Instant::now() + timeout;
        loop {
            match Self::connect(path) {
                Ok(c) => return Ok(c),
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// Write one full frame without wedging the caller forever.
    ///
    /// Uses a short write timeout so a peer that stops reading (full socket
    /// buffer) cannot block the engine hub — which would starve TapNdef / Play.
    pub fn send(&mut self, msg: &IpcMessage) -> Result<(), IpcTransportError> {
        let frame = encode_frame(msg)?;
        const WRITE_TIMEOUT: Duration = Duration::from_millis(100);
        let _ = self.stream.set_write_timeout(Some(WRITE_TIMEOUT));
        self.stream.set_nonblocking(false)?;
        let result = self.stream.write_all(&frame);
        let _ = self.stream.set_nonblocking(true);
        let _ = self.stream.set_write_timeout(None);
        match result {
            Ok(()) => Ok(()),
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut
                    || e.kind() == io::ErrorKind::WouldBlock
                    || e.raw_os_error() == Some(11) /* EAGAIN */ =>
            {
                Err(IpcTransportError::Timeout)
            }
            Err(e) => Err(IpcTransportError::from(e)),
        }
    }

    /// Read available bytes and return at most one complete frame.
    ///
    /// `Ok(None)` means no complete frame yet (or WouldBlock with empty buffer).
    pub fn try_recv(&mut self) -> Result<Option<IpcMessage>, IpcTransportError> {
        self.fill_buf()?;
        match try_pop_frame(&mut self.read_buf) {
            Ok(v) => Ok(v),
            Err(e @ FrameError::TooLarge(_)) => {
                self.read_buf.clear();
                Err(IpcTransportError::Frame(e))
            }
            Err(e) => Err(IpcTransportError::Frame(e)),
        }
    }

    /// Wait up to `timeout` for one complete frame.
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<IpcMessage>, IpcTransportError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(msg) = self.try_recv()? {
                return Ok(Some(msg));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Write raw bytes (tests / diagnostics). Temporarily uses blocking mode.
    pub fn write_raw(&mut self, bytes: &[u8]) -> Result<(), IpcTransportError> {
        self.stream.set_nonblocking(false)?;
        let result = self.stream.write_all(bytes);
        let _ = self.stream.set_nonblocking(true);
        result?;
        Ok(())
    }

    fn fill_buf(&mut self) -> Result<(), IpcTransportError> {
        let mut tmp = [0u8; 4096];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => {
                    if self.read_buf.is_empty() {
                        return Err(IpcTransportError::Disconnected);
                    }
                    // Peer closed after sending data still in buffer.
                    return Ok(());
                }
                Ok(n) => self.read_buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Engine-side listener for client connections.
#[derive(Debug)]
pub struct IpcListener {
    listener: UnixListener,
    pub paths: IpcPaths,
}

impl IpcListener {
    /// Bind `paths.engine_sock`, removing any stale socket file first.
    pub fn bind(paths: &IpcPaths) -> Result<Self, IpcTransportError> {
        ensure_ipc_dir(paths)?;
        remove_stale_sock(&paths.engine_sock)?;
        let listener = UnixListener::bind(&paths.engine_sock)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            paths: paths.clone(),
        })
    }

    pub fn try_accept(&self) -> Result<Option<IpcConnection>, IpcTransportError> {
        match self.listener.accept() {
            Ok((stream, _)) => Ok(Some(IpcConnection::from_stream(stream)?)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Blocking accept (clears non-blocking briefly).
    pub fn accept(&self) -> Result<IpcConnection, IpcTransportError> {
        self.listener.set_nonblocking(false)?;
        let result = self.listener.accept();
        let _ = self.listener.set_nonblocking(true);
        let (stream, _) = result?;
        Ok(IpcConnection::from_stream(stream)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::IpcPaths;
    use crate::{PlaybackCommand, TapKeyPayload};
    use std::fs;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn temp_paths() -> (tempfile::TempDir, IpcPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = IpcPaths::from_dir(tmp.path());
        (tmp, paths)
    }

    #[test]
    fn uds_encode_decode_round_trip() {
        let (_tmp, paths) = temp_paths();
        let listener = IpcListener::bind(&paths).unwrap();
        let path = paths.engine_sock.clone();
        let barrier = Arc::new(Barrier::new(2));
        let b2 = Arc::clone(&barrier);
        let handle = thread::spawn(move || {
            b2.wait();
            let mut client = IpcConnection::connect(&path).unwrap();
            client
                .send(&IpcMessage::PlaybackCommand(PlaybackCommand::Play))
                .unwrap();
            client
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .expect("status")
        });
        barrier.wait();
        let mut server = loop {
            if let Some(c) = listener.try_accept().unwrap() {
                break c;
            }
            thread::sleep(Duration::from_millis(5));
        };
        let msg = server
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect("cmd");
        assert!(matches!(
            msg,
            IpcMessage::PlaybackCommand(PlaybackCommand::Play)
        ));
        server
            .send(&IpcMessage::TapKey(TapKeyPayload {
                noubin_key: "ok".into(),
                tap_id: 1,
                already_normalised: true,
            }))
            .unwrap();
        let reply = handle.join().unwrap();
        assert!(matches!(reply, IpcMessage::TapKey(_)));
    }

    #[test]
    fn truncated_garbage_does_not_panic() {
        let (_tmp, paths) = temp_paths();
        let listener = IpcListener::bind(&paths).unwrap();
        let path = paths.engine_sock.clone();
        let client = thread::spawn(move || {
            let mut c = IpcConnection::connect_timeout(&path, Duration::from_secs(2)).unwrap();
            let frame = encode_frame(&IpcMessage::PlaybackCommand(PlaybackCommand::Stop)).unwrap();
            // Two bytes of the length prefix only — peer must not panic.
            c.write_raw(&frame[..2]).unwrap();
            thread::sleep(Duration::from_millis(80));
            // Remainder of the same frame.
            c.write_raw(&frame[2..]).unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        let mut server = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(40));
        assert!(
            server.try_recv().unwrap().is_none(),
            "incomplete frame must wait"
        );
        let msg = server
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect("complete frame after partial");
        assert!(matches!(
            msg,
            IpcMessage::PlaybackCommand(PlaybackCommand::Stop)
        ));
        client.join().unwrap();
        let _ = fs::remove_file(&paths.engine_sock);
    }

    #[test]
    fn two_clients_frames_not_corrupted() {
        let (_tmp, paths) = temp_paths();
        let listener = IpcListener::bind(&paths).unwrap();
        let path = paths.engine_sock.clone();
        let path2 = path.clone();
        let hold = Arc::new(Barrier::new(3));
        let h1 = Arc::clone(&hold);
        let h2 = Arc::clone(&hold);
        let t1 = thread::spawn(move || {
            let mut c = IpcConnection::connect_timeout(&path, Duration::from_secs(2)).unwrap();
            for i in 0..20u64 {
                c.send(&IpcMessage::TapKey(TapKeyPayload {
                    noubin_key: format!("a{i}"),
                    tap_id: i,
                    already_normalised: true,
                }))
                .unwrap();
            }
            h1.wait();
        });
        let t2 = thread::spawn(move || {
            let mut c = IpcConnection::connect_timeout(&path2, Duration::from_secs(2)).unwrap();
            for i in 100..120u64 {
                c.send(&IpcMessage::TapKey(TapKeyPayload {
                    noubin_key: format!("b{i}"),
                    tap_id: i,
                    already_normalised: true,
                }))
                .unwrap();
            }
            h2.wait();
        });
        let mut c1 = None;
        let mut c2 = None;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && (c1.is_none() || c2.is_none()) {
            if let Some(c) = listener.try_accept().unwrap() {
                if c1.is_none() {
                    c1 = Some(c);
                } else {
                    c2 = Some(c);
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        let mut c1 = c1.expect("client1");
        let mut c2 = c2.expect("client2");

        let mut got = Vec::new();
        let drain_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < drain_deadline && got.len() < 40 {
            match c1.try_recv() {
                Ok(Some(m)) => got.push(m),
                Ok(None) | Err(IpcTransportError::Disconnected) => {}
                Err(e) => panic!("c1: {e}"),
            }
            match c2.try_recv() {
                Ok(Some(m)) => got.push(m),
                Ok(None) | Err(IpcTransportError::Disconnected) => {}
                Err(e) => panic!("c2: {e}"),
            }
            thread::sleep(Duration::from_millis(2));
        }
        hold.wait();
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(got.len(), 40);
        for m in &got {
            match m {
                IpcMessage::TapKey(p) => {
                    assert!(p.noubin_key.starts_with('a') || p.noubin_key.starts_with('b'));
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }
}
