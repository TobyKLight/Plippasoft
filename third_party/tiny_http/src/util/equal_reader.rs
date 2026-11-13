use std::io::Read;
use std::io::Result as IoResult;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// PLIPPA PATCH: most bytes of an unread body that are thrown away when the reader is dropped.
/// Upstream allocated a buffer of the whole remaining `Content-Length` (a 64 GiB header aborted
/// the process) and read for as long as the client kept sending.
const MAX_DRAIN_BYTES: usize = 16 * 1024 * 1024;
/// PLIPPA PATCH: longest the cleanup may take. The body is being discarded, so it gets no more
/// patience than that; the connection then ends on the next, malformed, request head.
const MAX_DRAIN_TIME: Duration = Duration::from_secs(10);

/// A `Reader` that reads exactly the number of bytes from a sub-reader.
///
/// If the limit is reached, it returns EOF. If the limit is not reached
/// when the destructor is called, the remaining bytes will be read and
/// thrown away.
pub struct EqualReader<R>
where
    R: Read,
{
    reader: R,
    size: usize,
    last_read_signal: Sender<IoResult<()>>,
    /// PLIPPA PATCH: set when bytes of the body are left unread, so the connection is closed
    /// instead of the leftovers being parsed as the next request head.
    leftover: Option<Arc<AtomicBool>>,
}

impl<R> EqualReader<R>
where
    R: Read,
{
    pub fn new(reader: R, size: usize) -> (EqualReader<R>, Receiver<IoResult<()>>) {
        let (tx, rx) = channel();

        let r = EqualReader {
            reader,
            size,
            last_read_signal: tx,
            leftover: None,
        };

        (r, rx)
    }

    /// PLIPPA PATCH: raise `flag` if the reader is dropped with bytes of the body unread (after
    /// the bounded drain below).
    pub fn closing_connection_on_leftover(mut self, flag: Arc<AtomicBool>) -> Self {
        self.leftover = Some(flag);
        self
    }
}

impl<R> Read for EqualReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if self.size == 0 {
            return Ok(0);
        }

        let buf = if buf.len() < self.size {
            buf
        } else {
            &mut buf[..self.size]
        };

        match self.reader.read(buf) {
            Ok(len) => {
                self.size -= len;
                Ok(len)
            }
            err @ Err(_) => err,
        }
    }
}

impl<R> Drop for EqualReader<R>
where
    R: Read,
{
    fn drop(&mut self) {
        let mut remaining_to_read = self.size;
        // PLIPPA PATCH: a fixed 8 KiB buffer, a byte budget and a time budget (see above).
        let mut buf = vec![0; remaining_to_read.min(8 * 1024)];
        let mut budget = MAX_DRAIN_BYTES;
        let started = Instant::now();

        while remaining_to_read > 0 && budget > 0 && started.elapsed() < MAX_DRAIN_TIME {
            let take = buf.len().min(remaining_to_read).min(budget);
            match self.reader.read(&mut buf[..take]) {
                Err(e) => {
                    self.last_read_signal.send(Err(e)).ok();
                    break;
                }
                Ok(0) => {
                    self.last_read_signal.send(Ok(())).ok();
                    break;
                }
                Ok(other) => {
                    remaining_to_read -= other;
                    budget -= other;
                }
            }
        }
        // Whatever is still unread (budget or time spent, error, timeout, early EOF) must never
        // be parsed as a request: it came from the client's body, and a request smuggled in it
        // would skip every check made on the real head (Origin, X-Plippa).
        if remaining_to_read > 0 {
            if let Some(flag) = &self.leftover {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EqualReader;
    use std::io::Read;

    #[test]
    fn test_limit() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut string = String::new();
            equal_reader.read_to_string(&mut string).unwrap();
            assert_eq!(string, "hello");
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }

    #[test]
    fn test_not_enough() {
        use std::io::Cursor;

        let mut org_reader = Cursor::new("hello world".to_string().into_bytes());

        {
            let (mut equal_reader, _) = EqualReader::new(org_reader.by_ref(), 5);

            let mut vec = [0];
            equal_reader.read_exact(&mut vec).unwrap();
            assert_eq!(vec[0], b'h');
        }

        let mut string = String::new();
        org_reader.read_to_string(&mut string).unwrap();
        assert_eq!(string, " world");
    }
}
