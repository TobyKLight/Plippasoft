//! Raspberry Pi–tolerant I2C interface for the PN532.
//!
//! Adapted from `eval/poc2/nfc_test_PN532/src/rpi_i2c.rs`.

use core::task::Poll;
use embedded_hal::i2c::{Error, ErrorKind, I2c, Operation};
use i2cdev::core::{I2CMessage, I2CTransfer};
use i2cdev::linux::{LinuxI2CBus, LinuxI2CError, LinuxI2CMessage};
use linux_embedded_hal::I2CError;
use pn532::i2c::{I2C_ADDRESS, PN532_I2C_READY};
use pn532::Interface;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::thread;
use std::time::Duration;

const POST_WRITE_GAP: Duration = Duration::from_micros(2000);
const RETRY_PAUSE: Duration = Duration::from_millis(2);
const MAX_IO_RETRIES: usize = 8;

const I2C_TIMEOUT_IOCTL: libc::c_ulong = 0x0702;
const I2C_RETRIES_IOCTL: libc::c_ulong = 0x0701;

pub struct Pn532I2c {
    inner: LinuxI2CBus,
}

impl Pn532I2c {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, LinuxI2CError> {
        let inner = LinuxI2CBus::new(path)?;
        let fd = inner.as_raw_fd();
        unsafe {
            libc::ioctl(fd, I2C_TIMEOUT_IOCTL, 100);
            libc::ioctl(fd, I2C_RETRIES_IOCTL, 10);
        }
        std::hint::black_box(fd);
        Ok(Self { inner })
    }

    fn transfer(&mut self, operations: &mut [Operation]) -> Result<(), I2CError> {
        let addr = u16::from(I2C_ADDRESS);
        let mut messages: Vec<LinuxI2CMessage> = operations
            .iter_mut()
            .map(|op| match op {
                Operation::Write(w) => LinuxI2CMessage::write(w).with_address(addr),
                Operation::Read(r) => LinuxI2CMessage::read(r).with_address(addr),
            })
            .collect();

        self.inner
            .transfer(&mut messages)
            .map(drop)
            .map_err(I2CError::from)
    }
}

fn transient(err: &I2CError) -> bool {
    if matches!(
        err.kind(),
        ErrorKind::NoAcknowledge(_) | ErrorKind::Bus | ErrorKind::ArbitrationLoss
    ) {
        return true;
    }
    match err.inner() {
        LinuxI2CError::Errno(5) | LinuxI2CError::Errno(110) | LinuxI2CError::Errno(121) => true,
        LinuxI2CError::Io(io) => matches!(io.raw_os_error(), Some(5 | 110 | 121)),
        _ => false,
    }
}

fn with_retry<T, F>(mut op: F) -> Result<T, I2CError>
where
    F: FnMut() -> Result<T, I2CError>,
{
    let mut last = None;
    for attempt in 0..MAX_IO_RETRIES {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if transient(&e) && attempt + 1 < MAX_IO_RETRIES => {
                last = Some(e);
                thread::sleep(RETRY_PAUSE);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.expect("retry loop records last transient error"))
}

impl embedded_hal::i2c::ErrorType for Pn532I2c {
    type Error = I2CError;
}

impl I2c for Pn532I2c {
    fn transaction(
        &mut self,
        address: u8,
        operations: &mut [Operation],
    ) -> Result<(), Self::Error> {
        debug_assert_eq!(address, I2C_ADDRESS);
        self.transfer(operations)
    }
}

pub struct RpiI2cInterface {
    pub i2c: Pn532I2c,
    /// Ready-byte reads that failed with a transient I2C error (NACK/EIO/timeout) and were
    /// reported to the protocol layer as "not ready yet". Monotonic; the driver compares it
    /// before/after a command to tell a bus failure from an ordinary response timeout.
    pub wait_errors: u32,
}

impl RpiI2cInterface {
    pub fn new(i2c: Pn532I2c) -> Self {
        Self { i2c, wait_errors: 0 }
    }
}

impl Interface for RpiI2cInterface {
    type Error = I2CError;

    fn write(&mut self, frame: &mut [u8]) -> Result<(), Self::Error> {
        with_retry(|| I2c::write(&mut self.i2c, I2C_ADDRESS, frame))?;
        thread::sleep(POST_WRITE_GAP);
        Ok(())
    }

    fn wait_ready(&mut self) -> Poll<Result<(), Self::Error>> {
        let mut buf = [0];
        match with_retry(|| I2c::read(&mut self.i2c, I2C_ADDRESS, &mut buf)) {
            Ok(()) => {
                if buf[0] == PN532_I2C_READY {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
            Err(e) if transient(&e) => {
                self.wait_errors = self.wait_errors.wrapping_add(1);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<(), Self::Error> {
        if buf.is_empty() {
            return Ok(());
        }
        let mut raw = vec![0u8; buf.len() + 1];
        with_retry(|| self.i2c.transfer(&mut [Operation::Read(&mut raw)]))?;
        buf.copy_from_slice(&raw[1..]);
        Ok(())
    }
}
