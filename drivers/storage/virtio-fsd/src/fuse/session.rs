use std::ffi::CString;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};

use fuse_abi::linux::{
    FUSE_ATOMIC_O_TRUNC, FUSE_EXPORT_SUPPORT, FUSE_GETATTR_FH, FUSE_KERNEL_MINOR_VERSION,
    FUSE_KERNEL_VERSION, FUSE_MAX_PAGES, FUSE_NO_EXPORT_SUPPORT, fuse_attr_out, fuse_create_in,
    fuse_flush_in, fuse_forget_in, fuse_fsync_in, fuse_getattr_in, fuse_in_header, fuse_init_in,
    fuse_init_out, fuse_link_in, fuse_mkdir_in, fuse_opcode, fuse_open_in, fuse_out_header,
    fuse_read_in, fuse_release_in, fuse_rename_in, fuse_setattr_in, fuse_statfs_out, fuse_write_in,
    fuse_write_out,
};
use log::{info, warn};
use syscall::PAGE_SIZE;
use syscall::error::{EIO, ENOMEM, Error};
use thiserror::Error;
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::fuse::protocol::*;

#[derive(Debug, Error)]
pub enum FuseError {
    #[error("Host rejected request with POSIX code: {0}")]
    Host(i32),
    #[error("Received malformed payload from transport")]
    MalformedPayload,
    #[error("Memory exhausted.")]
    OutOfMemory,
}

impl FuseError {
    pub fn into_syscall(self, op: &str) -> Error {
        if !matches!(self, Self::Host(_)) {
            warn!("virtio-fsd: {op}: {self}");
        }
        match self {
            Self::Host(errno) => Error::new(errno),
            Self::OutOfMemory => Error::new(ENOMEM),
            Self::MalformedPayload => Error::new(EIO),
        }
    }
}

pub struct FuseRequest<I> {
    pub fuse_in_header: fuse_in_header,
    pub fuse_in: I,
}

pub enum FuseReply<O> {
    Struct(O),
    /// Bytes copied into the caller's `Data::In` buffer.
    Size(usize),
}

impl<O> FuseReply<O> {
    pub fn into_struct(self) -> Result<O, FuseError> {
        match self {
            Self::Struct(s) => Ok(s),
            _ => Err(FuseError::MalformedPayload),
        }
    }

    pub fn into_size(self) -> Result<usize, FuseError> {
        match self {
            Self::Size(n) => Ok(n),
            _ => Err(FuseError::MalformedPayload),
        }
    }
}

/// Variable-length bytes for one FUSE call, separate from the typed request and reply.
/// `Out` is sent with the request; `In` receives the bytes the host wrote back.
pub enum Data<'a> {
    None,
    Out(&'a [u8]),
    In(&'a mut [u8]),
}

impl<O: FromBytes + Immutable> FuseReply<O> {
    /// Builds the payload from the first `written` bytes of `rx`: a `fuse_out_header`, then `O`.
    /// `Data::In` is the exception: those bytes are copied from `bounce` into the caller's buffer.
    /// Bytes past `written` stay unread, since `rx` can hold stale data there.
    pub fn from_wire(
        rx: &[u8],
        written: u32,
        unique: u64,
        data: Data<'_>,
        bounce: Option<&[u8]>,
    ) -> Result<Self, FuseError> {
        let header_len = mem::size_of::<fuse_out_header>();
        if (written as usize) < header_len || rx.len() < header_len {
            return Err(FuseError::MalformedPayload);
        }

        let (fuse_out_header, _) = fuse_out_header::read_from_prefix(&rx[..header_len])
            .map_err(|_| FuseError::MalformedPayload)?;

        if fuse_out_header.len != written {
            panic!(
                "virtio-fsd: reply length '{}' does not match bytes written '{}'",
                fuse_out_header.len, written
            );
        }
        if fuse_out_header.unique != unique {
            panic!(
                "virtio-fsd: reply unique value '{}' does not match request '{}'",
                fuse_out_header.unique, unique
            );
        }

        if fuse_out_header.error != 0 {
            return Err(FuseError::Host(fuse_out_header.error.wrapping_neg()));
        }

        match data {
            Data::In(buf) => {
                let n = written as usize - header_len;
                if n > buf.len() {
                    return Err(FuseError::MalformedPayload);
                }
                if n > 0 {
                    let src = bounce.and_then(|b| b.get(..n));
                    buf[..n].copy_from_slice(src.ok_or(FuseError::MalformedPayload)?);
                }
                Ok(Self::Size(n))
            }
            Data::None | Data::Out(_) => {
                let end = header_len + mem::size_of::<O>();
                if written as usize != end || rx.len() < end {
                    return Err(FuseError::MalformedPayload);
                }
                let (payload, _) = O::read_from_prefix(&rx[header_len..end])
                    .map_err(|_| FuseError::MalformedPayload)?;
                Ok(Self::Struct(payload))
            }
        }
    }
}

pub trait FuseTransporter {
    async fn send<I, O>(
        &self,
        request: FuseRequest<I>,
        data: Data<'_>,
    ) -> Result<FuseReply<O>, FuseError>
    where
        I: IntoBytes + Immutable,
        O: FromBytes + Immutable;

    async fn send_hiprio_noreply<I>(&self, request: FuseRequest<I>) -> Result<(), FuseError>
    where
        I: IntoBytes + Immutable;
}

/// Identity for `fuse_in_header`.
/// `NONE` (zeros) when the scheme call has no `CallerCtx` (INIT, OnClose, unmount).
#[derive(Clone, Copy, Debug, Default)]
pub struct Caller {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
}

impl Caller {
    pub const NONE: Self = Self {
        uid: 0,
        gid: 0,
        pid: 0,
    };
}

pub struct FuseSession<T: FuseTransporter> {
    next_unique: AtomicU64,
    init: fuse_init_out,
    transporter: T,
}

impl<T> FuseSession<T>
where
    T: FuseTransporter,
{
    /// Flags we put on FUSE_INIT. The host replies with the intersection.
    const CLIENT_INIT_FLAGS: u64 = FUSE_MAX_PAGES as u64 | FUSE_ATOMIC_O_TRUNC as u64;

    /// Initializes the FUSE session by sending a FUSE_INIT request to the device.
    pub async fn new(transporter: T) -> Result<Self, FuseError> {
        let mut session = Self {
            transporter,
            init: fuse_init_out::default(),
            next_unique: AtomicU64::new(1),
        };

        let flags = Self::CLIENT_INIT_FLAGS;
        let init_in = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 1 << 20, // 1 MiB
            flags: flags as u32,
            flags2: (flags >> 32) as u32,
            unused: [0; 11],
        };

        let request = session.request(
            fuse_opcode::FUSE_INIT.into(),
            NodeId::ROOT,
            init_in,
            0,
            None,
            Caller::NONE,
        );

        let init_out = session
            .transporter
            .send::<_, fuse_init_out>(request, Data::None)
            .await?
            .into_struct()?;

        if init_out.major != FUSE_KERNEL_VERSION {
            warn!(
                "virtio-fsd: FUSE major {} (client offered {})",
                init_out.major, FUSE_KERNEL_VERSION
            );
        }

        session.init = init_out;

        let use_local_parent_resolution =
            session.capable(FUSE_NO_EXPORT_SUPPORT) || !session.capable(FUSE_EXPORT_SUPPORT);

        // `walk_path` always resolves `..` locally via `parent_of`, regardless
        // of export support. The host flag is logged for diagnosis only.
        if use_local_parent_resolution {
            info!(
                "virtio-fsd: Host disabled export support (or is old). lookup('..') is resolved locally."
            );
        } else {
            info!(
                "virtio-fsd: Host enabled full export support; lookup('..') is still resolved locally."
            );
        }

        info!("virtio-fsd: FUSE session is initialized.");
        Ok(session)
    }

    /// Whether this `FUSE_INIT` flag was negotiated (`flags` / `flags2`).
    pub fn capable(&self, cap: impl Into<u64>) -> bool {
        (u64::from(self.init.flags) | u64::from(self.init.flags2) << 32) & cap.into() != 0
    }

    /// Max payload of one `FUSE_WRITE` (`fuse_init_out.max_write`).
    fn max_write_len(&self) -> usize {
        match self.init.max_write as usize {
            0 => PAGE_SIZE,
            n => n,
        }
    }

    /// Max payload of one `FUSE_READ` (`max_pages * PAGE_SIZE`).
    /// Linux uses 32 pages when the host leaves `max_pages` at 0.
    fn max_read_len(&self) -> usize {
        let pages = match self.init.max_pages as usize {
            0 => 32,
            n => n,
        };
        let len = pages.checked_mul(PAGE_SIZE).unwrap_or_else(|| {
            panic!("virtio-fsd: {pages} pages * PAGE_SIZE overflows usize.");
        });
        len.max(PAGE_SIZE)
    }

    /// Builds a request for `fuse_in`. `extra_len` is the number of bytes sent after it as
    /// `Data::Out` (`len` = header + `fuse_in` + extra_len).
    /// `unique` is allocated from `next_unique` when `None`. FUSE_FORGET must send `Some(0)`.
    fn request<I>(
        &self,
        opcode: u32,
        nodeid: NodeId,
        fuse_in: I,
        extra_len: usize,
        unique: Option<u64>,
        caller: Caller,
    ) -> FuseRequest<I> {
        let fuse_in_header = fuse_in_header {
            len: (mem::size_of::<fuse_in_header>() + mem::size_of::<I>() + extra_len) as u32,
            opcode,
            unique: unique.unwrap_or_else(|| self.next_unique.fetch_add(1, Ordering::SeqCst)),
            nodeid: nodeid.into(),
            uid: caller.uid,
            gid: caller.gid,
            pid: caller.pid,
            total_extlen: 0,
            padding: 0,
        };

        FuseRequest {
            fuse_in_header,
            fuse_in,
        }
    }

    /// Handles both `open` and `opendir` opcodes based on `is_dir`.
    pub async fn open(
        &self,
        nodeid: NodeId,
        flags: u32,
        is_dir: bool,
        caller: Caller,
    ) -> Result<FuseOpenOut, FuseError> {
        let opcode = if is_dir {
            fuse_opcode::FUSE_OPENDIR.into()
        } else {
            fuse_opcode::FUSE_OPEN.into()
        };
        let fuse_in = fuse_open_in {
            flags,
            open_flags: 0,
        };

        let request = self.request(opcode, nodeid, fuse_in, 0, None, caller);

        self.transporter
            .send(request, Data::None)
            .await?
            .into_struct()
    }

    pub async fn getattr(
        &self,
        nodeid: NodeId,
        fh: Option<FileHandle>,
        caller: Caller,
    ) -> Result<fuse_attr_out, FuseError> {
        let fuse_in = fuse_getattr_in {
            getattr_flags: if fh.is_some() { FUSE_GETATTR_FH } else { 0 },
            dummy: 0,
            fh: fh.map_or(0, u64::from),
        };

        let request = self.request(
            fuse_opcode::FUSE_GETATTR.into(),
            nodeid,
            fuse_in,
            0,
            None,
            caller,
        );

        self.transporter
            .send(request, Data::None)
            .await?
            .into_struct()
    }

    pub async fn statfs(
        &self,
        nodeid: NodeId,
        caller: Caller,
    ) -> Result<fuse_statfs_out, FuseError> {
        let request = self.request(fuse_opcode::FUSE_STATFS.into(), nodeid, (), 0, None, caller);

        self.transporter
            .send(request, Data::None)
            .await?
            .into_struct()
    }

    pub async fn setattr(
        &self,
        nodeid: NodeId,
        fuse_in: fuse_setattr_in,
        caller: Caller,
    ) -> Result<fuse_attr_out, FuseError> {
        let request = self.request(
            fuse_opcode::FUSE_SETATTR.into(),
            nodeid,
            fuse_in,
            0,
            None,
            caller,
        );

        self.transporter
            .send(request, Data::None)
            .await?
            .into_struct()
    }

    pub async fn readdir(
        &self,
        nodeid: NodeId,
        fh: FileHandle,
        offset: u64,
        caller: Caller,
    ) -> Result<FuseReaddirOut, FuseError> {
        let fuse_in = fuse_read_in {
            fh: fh.into(),
            offset,
            size: FUSE_READDIR_BUFFER_SIZE,
            read_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };

        let request = self.request(
            fuse_opcode::FUSE_READDIR.into(),
            nodeid,
            fuse_in,
            0,
            None,
            caller,
        );

        let mut buf = vec![0u8; FUSE_READDIR_BUFFER_SIZE as usize];

        let n = self
            .transporter
            .send::<_, ()>(request, Data::In(&mut buf))
            .await?
            .into_size()?;

        buf.truncate(n);
        Ok(FuseReaddirOut::new(buf))
    }

    pub async fn lookup(
        &self,
        parent_nodeid: NodeId,
        name: CString,
        caller: Caller,
    ) -> Result<FuseEntryOut, FuseError> {
        let name = name.as_bytes_with_nul();
        let name_len = name.len();

        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let request = self.request(
            fuse_opcode::FUSE_LOOKUP.into(),
            parent_nodeid,
            (),
            name_len,
            None,
            caller,
        );

        self.transporter
            .send(request, Data::Out(name))
            .await?
            .into_struct()
    }

    /// Issues a single `FUSE_READ` for at most `max_read_len()` bytes of `buf`.
    pub async fn read(
        &self,
        nodeid: NodeId,
        fh: FileHandle,
        offset: u64,
        buf: &mut [u8],
        read_flags: u32,
        flags: u32,
        caller: Caller,
    ) -> Result<usize, FuseError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min(self.max_read_len());
        let buf = &mut buf[..n];

        let fuse_in = fuse_read_in {
            fh: fh.into(),
            offset,
            size: n as u32,
            read_flags,
            lock_owner: 0,
            flags,
            padding: 0,
        };

        let request = self.request(
            fuse_opcode::FUSE_READ.into(),
            nodeid,
            fuse_in,
            0,
            None,
            caller,
        );

        self.transporter
            .send::<_, ()>(request, Data::In(buf))
            .await?
            .into_size()
    }

    /// Issues a single `FUSE_WRITE` for at most `max_write_len()` bytes of `data`.
    pub async fn write(
        &self,
        nodeid: NodeId,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: u32,
        flags: u32,
        caller: Caller,
    ) -> Result<fuse_write_out, FuseError> {
        if data.is_empty() {
            return Ok(fuse_write_out {
                size: 0,
                padding: 0,
            });
        }
        let size = data.len().min(self.max_write_len());
        let data = &data[..size];

        let fuse_in = fuse_write_in {
            fh: fh.into(),
            offset,
            size: size as u32,
            write_flags,
            lock_owner: 0,
            flags,
            padding: 0,
        };

        let request = self.request(
            fuse_opcode::FUSE_WRITE.into(),
            nodeid,
            fuse_in,
            size,
            None,
            caller,
        );

        self.transporter
            .send(request, Data::Out(data))
            .await?
            .into_struct()
    }

    pub async fn create(
        &self,
        parent_nodeid: NodeId,
        name: CString,
        flags: u32,
        mode: u32,
        caller: Caller,
    ) -> Result<FuseCreateOut, FuseError> {
        let name = name.as_bytes_with_nul();
        let name_len = name.len();

        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let fuse_in = fuse_create_in {
            flags,
            mode,
            umask: 0,
            open_flags: 0,
        };

        let request = self.request(
            fuse_opcode::FUSE_CREATE.into(),
            parent_nodeid,
            fuse_in,
            name_len,
            None,
            caller,
        );

        self.transporter
            .send(request, Data::Out(name))
            .await?
            .into_struct()
    }

    pub async fn fsync(
        &self,
        nodeid: NodeId,
        fh: FileHandle,
        is_dir: bool,
        caller: Caller,
    ) -> Result<(), FuseError> {
        let opcode = if is_dir {
            fuse_opcode::FUSE_FSYNCDIR.into()
        } else {
            fuse_opcode::FUSE_FSYNC.into()
        };
        let fuse_in = fuse_fsync_in {
            fh: fh.into(),
            fsync_flags: 0,
            padding: 0,
        };

        let request = self.request(opcode, nodeid, fuse_in, 0, None, caller);

        self.transporter
            .send::<_, ()>(request, Data::None)
            .await?
            .into_struct()
    }

    pub async fn flush(
        &self,
        nodeid: NodeId,
        fh: FileHandle,
        caller: Caller,
    ) -> Result<(), FuseError> {
        let fuse_in = fuse_flush_in {
            fh: fh.into(),
            unused: 0,
            padding: 0,
            lock_owner: 0,
        };

        let request = self.request(
            fuse_opcode::FUSE_FLUSH.into(),
            nodeid,
            fuse_in,
            0,
            None,
            caller,
        );

        self.transporter
            .send::<_, ()>(request, Data::None)
            .await?
            .into_struct()
    }

    pub async fn release(
        &self,
        nodeid: NodeId,
        fh: FileHandle,
        is_dir: bool,
        caller: Caller,
    ) -> Result<(), FuseError> {
        let opcode = if is_dir {
            fuse_opcode::FUSE_RELEASEDIR.into()
        } else {
            fuse_opcode::FUSE_RELEASE.into()
        };
        let fuse_in = fuse_release_in {
            fh: fh.into(),
            flags: 0,
            release_flags: 0,
            lock_owner: 0,
        };

        let request = self.request(opcode, nodeid, fuse_in, 0, None, caller);

        self.transporter
            .send::<_, ()>(request, Data::None)
            .await?
            .into_struct()
    }

    pub async fn forget(
        &self,
        nodeid: NodeId,
        nlookup: u64,
        caller: Caller,
    ) -> Result<(), FuseError> {
        let fuse_in = fuse_forget_in { nlookup };

        let request = self.request(
            fuse_opcode::FUSE_FORGET.into(),
            nodeid,
            fuse_in,
            0,
            Some(0),
            caller,
        );

        self.transporter.send_hiprio_noreply(request).await?;

        Ok(())
    }

    pub async fn mkdir(
        &self,
        parent_nodeid: NodeId,
        name: CString,
        mode: u32,
        caller: Caller,
    ) -> Result<FuseEntryOut, FuseError> {
        let name = name.as_bytes_with_nul();
        let name_len = name.len();

        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let fuse_in = fuse_mkdir_in { mode, umask: 0 };

        let request = self.request(
            fuse_opcode::FUSE_MKDIR.into(),
            parent_nodeid,
            fuse_in,
            name_len,
            None,
            caller,
        );

        self.transporter
            .send(request, Data::Out(name))
            .await?
            .into_struct()
    }

    pub async fn rm(
        &self,
        parent_nodeid: NodeId,
        name: CString,
        is_dir: bool,
        caller: Caller,
    ) -> Result<(), FuseError> {
        let name = name.as_bytes_with_nul();
        let name_len = name.len();

        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let opcode = if is_dir {
            fuse_opcode::FUSE_RMDIR.into()
        } else {
            fuse_opcode::FUSE_UNLINK.into()
        };
        let request = self.request(opcode, parent_nodeid, (), name_len, None, caller);

        self.transporter
            .send::<_, ()>(request, Data::Out(name))
            .await?
            .into_struct()
    }

    pub async fn readlink(&self, nodeid: NodeId, caller: Caller) -> Result<String, FuseError> {
        let request = self.request(
            fuse_opcode::FUSE_READLINK.into(),
            nodeid,
            (),
            0,
            None,
            caller,
        );

        let mut buf = vec![0u8; FUSE_READLINK_MAX];

        let n = self
            .transporter
            .send::<_, ()>(request, Data::In(&mut buf))
            .await?
            .into_size()?;

        if n > FUSE_READLINK_MAX {
            warn!("virtio-fsd: readlink: path length is larger than FUSE_READLINK_MAX");
            return Err(FuseError::MalformedPayload);
        }

        buf.truncate(n);
        while buf.last() == Some(&0) {
            buf.pop();
        }

        String::from_utf8(buf).map_err(|e| {
            warn!("virtio-fsd: readlink: path is not a valid utf8: {e:?}");
            FuseError::MalformedPayload
        })
    }

    pub async fn symlink(
        &self,
        parent_nodeid: NodeId,
        name: &CString,
        path: &CString,
        caller: Caller,
    ) -> Result<FuseEntryOut, FuseError> {
        let name = name.as_bytes_with_nul();
        let path = path.as_bytes_with_nul();

        let name_len = name.len();
        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let path_len = path.len();
        if name_len + path_len > FUSE_SYMLINK_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let mut data = [0u8; FUSE_SYMLINK_MAX];

        data[..name_len].copy_from_slice(name);
        data[name_len..name_len + path_len].copy_from_slice(path);

        let request = self.request(
            fuse_opcode::FUSE_SYMLINK.into(),
            parent_nodeid,
            (),
            name_len + path_len,
            None,
            caller,
        );

        self.transporter
            .send(request, Data::Out(&data[..name_len + path_len]))
            .await?
            .into_struct()
    }

    pub async fn rename(
        &self,
        old_parent: NodeId,
        old_name: CString,
        new_parent: NodeId,
        new_name: CString,
        caller: Caller,
    ) -> Result<(), FuseError> {
        let old = old_name.as_bytes_with_nul();
        let new = new_name.as_bytes_with_nul();
        if old.len() > NAME_MAX || new.len() > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let mut names = [0u8; NAME_MAX * 2];
        names[..old.len()].copy_from_slice(old);
        names[old.len()..old.len() + new.len()].copy_from_slice(new);
        let names = &names[..old.len() + new.len()];

        let request = self.request(
            // switch to FUSE_RENAME2 if scheme starts accepting flags
            fuse_opcode::FUSE_RENAME.into(),
            old_parent,
            fuse_rename_in {
                newdir: new_parent.into(),
            },
            names.len(),
            None,
            caller,
        );

        self.transporter
            .send::<_, ()>(request, Data::Out(names))
            .await?
            .into_struct()
    }

    pub async fn link(
        &self,
        new_parent: NodeId,
        old_nodeid: NodeId,
        name: CString,
        caller: Caller,
    ) -> Result<FuseEntryOut, FuseError> {
        let name = name.as_bytes_with_nul();
        let name_len = name.len();
        if name_len > NAME_MAX {
            return Err(FuseError::Host(syscall::ENAMETOOLONG as i32));
        }

        let fuse_in = fuse_link_in {
            oldnodeid: old_nodeid.into(),
        };

        let request = self.request(
            fuse_opcode::FUSE_LINK.into(),
            new_parent,
            fuse_in,
            name_len,
            None,
            caller,
        );

        self.transporter
            .send(request, Data::Out(name))
            .await?
            .into_struct()
    }

    #[cfg(test)]
    pub(crate) fn for_test(transporter: T) -> Self {
        Self {
            transporter,
            init: fuse_init_out {
                major: FUSE_KERNEL_VERSION,
                minor: FUSE_KERNEL_MINOR_VERSION,
                flags: FUSE_ATOMIC_O_TRUNC,
                max_write: PAGE_SIZE as u32,
                max_pages: 1,
                ..fuse_init_out::default()
            },
            next_unique: AtomicU64::new(1),
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;
