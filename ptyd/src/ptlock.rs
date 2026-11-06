//! "ptlock" resource for the `pty` scheme.
//! Lock and unlock the PTY.
//! All the lock does currently is prevent terminals from being opened without first being
//! unlocked.
use std::cell::RefCell;
use std::rc::Weak;

use syscall::error::{Error, Result, EBADF, EINVAL, EPIPE};
use syscall::flag::{EventFlags, F_GETFL, F_SETFL, O_ACCMODE};

use crate::pty::Pty;
use crate::resource::Resource;

/// Read side of a pipe
pub struct PtyLock {
    pty: Weak<RefCell<Pty>>,
    flags: usize,
}

impl PtyLock {
    pub fn new(pty: Weak<RefCell<Pty>>, flags: usize) -> Self {
        PtyLock { pty, flags }
    }
}

impl Resource for PtyLock {
    fn pty(&self) -> Weak<RefCell<Pty>> {
        self.pty.clone()
    }

    fn flags(&self) -> usize {
        self.flags
    }

    fn path(&mut self, buf: &mut [u8]) -> Result<usize> {
        if let Some(pty_lock) = self.pty.upgrade() {
            pty_lock.borrow_mut().path(buf)
        } else {
            Err(Error::new(EPIPE))
        }
    }

    // FIXME: assuming c_int (input) has same size as u32 (output)
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if let Some(pty_lock) = self.pty.upgrade() {
            let pty = pty_lock.borrow();

            let lock_buf = buf
                .get_mut(..4)
                .and_then(|b| <&mut [u8; 4]>::try_from(b).ok())
                .ok_or(Error::new(EBADF))?;
            *lock_buf = u32::from(pty.locked).to_ne_bytes();
            Ok(4)
        } else {
            Ok(0)
        }
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        if let Some(pty_lock) = self.pty.upgrade() {
            let mut pty = pty_lock.borrow_mut();

            let lock_val = u32::from_ne_bytes(
                buf.get(..4)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .ok_or(Error::new(EBADF))?,
            );
            pty.locked = lock_val != 0;
            Ok(4)
        } else {
            Ok(0)
        }
    }

    fn sync(&mut self) -> Result<()> {
        Ok(())
    }

    fn fcntl(&mut self, cmd: usize, arg: usize) -> Result<usize> {
        match cmd {
            F_GETFL => Ok(self.flags),
            F_SETFL => {
                self.flags = (self.flags & O_ACCMODE) | (arg & !O_ACCMODE);
                Ok(0)
            }
            _ => Err(Error::new(EINVAL)),
        }
    }

    fn fevent(&mut self) -> Result<EventFlags> {
        Err(Error::new(EBADF))
    }

    fn events(&mut self) -> EventFlags {
        EventFlags::empty()
    }
}
