use std::collections::VecDeque;
use std::str;
use std::sync::{Arc, Mutex};

use audiod::{AudioChunk, HANDLE_BUFFER_SIZE, HW_BUFFER_SIZE};
use redox_scheme::scheme::SchemeSync;
use redox_scheme::{CallerCtx, OpenResult};
use scheme_utils::HandleMap;
use syscall::error::{Error, Result, EACCES, EBADF, EINVAL, ENOENT, EWOULDBLOCK};
use syscall::schemev2::NewFdFlags;

enum Handle {
    Audio { buffer: VecDeque<(i16, i16)> },
    // TODO: move volume to audiohw:?
    // TODO: Use SYS_CALL to handle this better?
    Volume,
    SchemeRoot,
}

pub struct AudioSchemeInner {
    handles: HandleMap<Handle>,
    volume: i32,
}

impl AudioSchemeInner {
    pub fn new() -> Self {
        Self {
            handles: HandleMap::new(),
            volume: 50,
        }
    }

    pub fn buffer(&mut self) -> AudioChunk {
        let mut mix_buffer = [(0i16, 0i16); HW_BUFFER_SIZE];

        // Multiply each sample by the cube of volume divided by 100
        // This mimics natural perception of loudness
        let volume_factor = ((self.volume as f32) / 100.0).powi(3);
        for (_id, handle) in self.handles.iter_mut() {
            match handle {
                Handle::Audio { ref mut buffer } => {
                    let mut i = 0;
                    while i < mix_buffer.len() {
                        if let Some(sample) = buffer.pop_front() {
                            let left = (f32::from(sample.0) * volume_factor) as i16;
                            let right = (f32::from(sample.1) * volume_factor) as i16;
                            mix_buffer[i].0 = mix_buffer[i].0.saturating_add(left);
                            mix_buffer[i].1 = mix_buffer[i].1.saturating_add(right);
                        } else {
                            break;
                        }
                        i += 1;
                    }
                }
                _ => (),
            }
        }

        mix_buffer
    }
}

pub struct AudioScheme {
    pub inner: Arc<Mutex<AudioSchemeInner>>,
}

impl AudioScheme {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(AudioSchemeInner::new())),
        }
    }
}

impl SchemeSync for AudioScheme {
    fn scheme_root(&mut self) -> Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        Ok(inner.handles.insert(Handle::SchemeRoot))
    }
    fn openat(
        &mut self,
        dirfd: usize,
        path: &str,
        _flags: usize,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        let mut inner = self.inner.lock().unwrap();

        if !matches!(inner.handles.get(dirfd)?, Handle::SchemeRoot) {
            return Err(Error::new(EACCES));
        }

        let (handle, flags) = match path.trim_matches('/') {
            "" => (
                Handle::Audio {
                    buffer: VecDeque::new(),
                },
                NewFdFlags::empty(),
            ),
            "volume" => (Handle::Volume, NewFdFlags::POSITIONED),
            _ => return Err(Error::new(ENOENT)),
        };

        let id = inner.handles.insert(handle);

        Ok(OpenResult::ThisScheme { number: id, flags })
    }

    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        off: u64,
        _flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        let mut inner = self.inner.lock().unwrap();

        //TODO: check flags for readable
        match inner.handles.get_mut(id)? {
            Handle::Audio { buffer: _ } => {
                //TODO: audio input?
                Err(Error::new(EBADF))
            }
            Handle::Volume => {
                let Ok(off) = usize::try_from(off) else {
                    return Ok(0);
                };
                //TODO: should we allocate every time?
                let bytes = format!("{}", inner.volume).into_bytes();
                let src = bytes.get(off..).unwrap_or(&[]);
                let len = src.len().min(buf.len());
                buf[..len].copy_from_slice(&src[..len]);

                Ok(len)
            }
            Handle::SchemeRoot => Err(Error::new(EBADF)),
        }
    }

    fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        offset: u64,
        _flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        let mut inner = self.inner.lock().unwrap();

        //TODO: check flags for writable
        match inner.handles.get_mut(id)? {
            Handle::Audio { ref mut buffer } => {
                if buffer.len() >= HANDLE_BUFFER_SIZE {
                    Err(Error::new(EWOULDBLOCK))
                } else {
                    let mut i = 0;
                    while i + 4 <= buf.len() {
                        buffer.push_back((
                            i16::from(buf[i]) | (i16::from(buf[i + 1]) << 8),
                            i16::from(buf[i + 2]) | (i16::from(buf[i + 3]) << 8),
                        ));

                        i += 4;
                    }

                    Ok(i)
                }
            }
            Handle::Volume => {
                //TODO: support other offsets?
                if offset == 0 {
                    let value = str::from_utf8(buf)
                        .map_err(|_| Error::new(EINVAL))?
                        .trim()
                        .parse::<i32>()
                        .map_err(|_| Error::new(EINVAL))?;
                    if value >= 0 && value <= 100 {
                        inner.volume = value;
                        Ok(buf.len())
                    } else {
                        Err(Error::new(EINVAL))
                    }
                } else {
                    // EOF
                    Ok(0)
                }
            }
            Handle::SchemeRoot => Err(Error::new(EBADF)),
        }
    }
}
