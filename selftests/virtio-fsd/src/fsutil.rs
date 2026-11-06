// Redox open flags sit in the upper 16 bits; mode is the low 16 when packed for openat.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

use libredox::error::Error as RedoxError;
use libredox::{Fd, flag};
use syscall::{StdFsCallKind, StdFsCallMeta};

pub const O_STAT: i32 = 0x2000_0000u32 as i32;
pub const O_SYMLINK: i32 = 0x4000_0000u32 as i32;
pub const O_NOFOLLOW: i32 = 0x8000_0000u32 as i32;

pub const ENOENT: i32 = 2;
pub const EBADF: i32 = 9;
pub const EEXIST: i32 = 17;
pub const EXDEV: i32 = 18;
pub const ENOTDIR: i32 = 20;
pub const EISDIR: i32 = 21;
pub const EINVAL: i32 = 22;
pub const EPERM: i32 = 1;
pub const ENAMETOOLONG: i32 = 36;
pub const ENOTEMPTY: i32 = 39;
pub const ELOOP: i32 = 40;
pub const EAGAIN: i32 = 11;
// Redox EWOULDBLOCK is 41; Linux aliases it to EAGAIN (11).
pub const EWOULDBLOCK: i32 = 41;
pub const ENODEV: i32 = 19;
pub const ENOSYS: i32 = 38;
pub const EOPNOTSUPP: i32 = 95;

// relibc's libc doesn't export these.
pub const F_RDLCK: i16 = 0;
pub const F_WRLCK: i16 = 1;
pub const F_UNLCK: i16 = 2;
pub const F_GETLK: i32 = 5;
pub const F_SETLK: i32 = 6;
pub const F_SETLKW: i32 = 7;
pub const F_OFD_GETLK: i32 = 36;
pub const F_OFD_SETLK: i32 = 37;
pub const F_OFD_SETLKW: i32 = 38;
pub const SEEK_SET: i16 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Flock {
    pub l_type: i16,
    pub l_whence: i16,
    pub l_start: i64,
    pub l_len: i64,
    pub l_pid: i32,
}

pub const MODE_TYPE: u16 = 0xF000;
pub const MODE_FILE: u16 = 0o100000;
pub const MODE_DIR: u16 = 0o040000;
pub const MODE_SYMLINK: u16 = 0xA000;

// tv_nsec sentinels, not a nanosecond.
pub const UTIME_NOW: i64 = 0x3fffffff;
pub const UTIME_OMIT: i64 = 0x3ffffffe;

pub const AT_REMOVEDIR: i32 = libc::AT_REMOVEDIR;

// relibc `RENAME_NOREPLACE` (0x01). `frenameat` tests this bit.
pub const RENAME_NOREPLACE: u32 = 1;

fn std_fs_at(
    first: &File,
    second: &File,
    mut payload: Vec<u8>,
    kind: StdFsCallKind,
    arg1: u64,
) -> io::Result<()> {
    let fds = [first.as_raw_fd() as usize, second.as_raw_fd() as usize];
    let meta = StdFsCallMeta::new(kind, arg1, 0);
    syscall::std_fs_call(&fds[..], &mut payload, &meta)
        .map(|_| ())
        .map_err(|err| io::Error::from_raw_os_error(err.errno))
}

pub fn open_flags(path: &Path, flags: i32, mode: u16) -> io::Result<File> {
    let s = path_to_str(path)?;
    let fd = Fd::open(s, flags, mode).map_err(redox_to_io)?;
    // SAFETY: freshly opened fd, sole owner; `into_raw` prevents double-close.
    Ok(unsafe { File::from_raw_fd(fd.into_raw() as RawFd) })
}

pub fn open_ro(path: &Path) -> io::Result<File> {
    open_flags(path, flag::O_RDONLY, 0)
}

pub fn open_rw(path: &Path) -> io::Result<File> {
    open_flags(path, flag::O_RDWR, 0)
}

pub fn open_wo(path: &Path) -> io::Result<File> {
    open_flags(path, flag::O_WRONLY, 0)
}

pub fn open_dir(path: &Path) -> io::Result<File> {
    open_flags(path, flag::O_RDONLY | flag::O_DIRECTORY, 0)
}

pub fn create_rw(path: &Path) -> io::Result<File> {
    open_flags(path, flag::O_RDWR | flag::O_CREAT | flag::O_TRUNC, 0o644)
}

pub fn create_excl(path: &Path) -> io::Result<File> {
    open_flags(path, flag::O_RDWR | flag::O_CREAT | flag::O_EXCL, 0o644)
}

pub fn mkdir(path: &Path) -> io::Result<File> {
    open_flags(
        path,
        flag::O_RDONLY | flag::O_CREAT | flag::O_DIRECTORY | flag::O_EXCL,
        0o755,
    )
}

pub fn mkdir_noexcl(path: &Path) -> io::Result<File> {
    open_flags(
        path,
        flag::O_RDONLY | flag::O_CREAT | flag::O_DIRECTORY,
        0o755,
    )
}

pub fn make_dir(path: &Path) -> io::Result<()> {
    mkdir(path).map(drop)
}

pub fn open_stat(path: &Path) -> io::Result<File> {
    open_flags(path, O_STAT, 0)
}

pub fn open_symlink(path: &Path) -> io::Result<File> {
    open_flags(path, flag::O_RDONLY | O_SYMLINK, 0)
}

pub fn open_creat_symlink(path: &Path) -> io::Result<File> {
    open_flags(path, flag::O_WRONLY | flag::O_CREAT | O_SYMLINK, 0o777)
}

pub fn readlink(path: &Path) -> io::Result<String> {
    let c = cstring(path_to_str(path)?)?;
    let mut buf = vec![0u8; 4096];
    let n = unsafe { libc::readlink(c.as_ptr(), buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn symlink(target: &str, path: &Path) -> io::Result<()> {
    let t = cstring(target)?;
    let p = cstring(path_to_str(path)?)?;
    libc_zero(unsafe { libc::symlink(t.as_ptr(), p.as_ptr()) })
}

pub fn is_symlink(path: &Path) -> io::Result<bool> {
    Ok(lstat(path)?.st_mode as u16 & MODE_TYPE == MODE_SYMLINK)
}

pub fn stat(path: &Path) -> io::Result<libc::stat> {
    path_stat(path, true)
}

pub fn lstat(path: &Path) -> io::Result<libc::stat> {
    path_stat(path, false)
}

pub fn fstat(file: &File) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    libc_zero(unsafe { libc::fstat(file.as_raw_fd(), &mut st) })?;
    Ok(st)
}

fn path_stat(path: &Path, follow: bool) -> io::Result<libc::stat> {
    let c = cstring(path_to_str(path)?)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = if follow {
        unsafe { libc::stat(c.as_ptr(), &mut st) }
    } else {
        unsafe { libc::lstat(c.as_ptr(), &mut st) }
    };
    libc_zero(rc)?;
    Ok(st)
}

pub fn perms(st: &libc::stat) -> u16 {
    st.st_mode as u16 & 0o7777
}

pub fn mode_of(path: &Path) -> io::Result<u16> {
    Ok(perms(&stat(path)?))
}

pub fn type_of(path: &Path) -> io::Result<u16> {
    Ok(stat(path)?.st_mode as u16 & MODE_TYPE)
}

pub fn owner(st: &libc::stat) -> (u32, u32) {
    (st.st_uid, st.st_gid)
}

pub fn clear_lru_eviction_log(debug_root: &Path) -> io::Result<()> {
    let f = open_flags(
        &debug_root.join("lru_evicted"),
        flag::O_RDONLY | flag::O_TRUNC,
        0,
    )?;
    drop(f);
    Ok(())
}

pub fn openat(dir: &File, rel: &str, flags: i32) -> io::Result<File> {
    // Don't let Fd drop close the caller's dir.
    let parent = Fd::new(dir.as_raw_fd() as usize);
    let fcntl_flags = (flags as u32) & (flag::O_NONBLOCK | flag::O_APPEND | flag::O_ASYNC) as u32;
    let child = parent.openat(rel, flags, fcntl_flags);
    core::mem::forget(parent);
    let child = child.map_err(redox_to_io)?;
    // SAFETY: freshly opened child fd, sole owner.
    Ok(unsafe { File::from_raw_fd(child.into_raw() as RawFd) })
}

pub fn openat_ro(dir: &File, rel: &str) -> io::Result<File> {
    openat(dir, rel, flag::O_RDONLY)
}

pub fn openat_dir(dir: &File, rel: &str) -> io::Result<File> {
    openat(dir, rel, flag::O_RDONLY | flag::O_DIRECTORY)
}

pub fn openat_create(dir: &File, rel: &str) -> io::Result<File> {
    let flags = (flag::O_RDWR | flag::O_CREAT) | 0o644;
    openat(dir, rel, flags)
}

pub fn openat_mkdir(dir: &File, rel: &str) -> io::Result<File> {
    let flags = (flag::O_RDONLY | flag::O_CREAT | flag::O_DIRECTORY | flag::O_EXCL) | 0o755;
    openat(dir, rel, flags)
}

pub fn unlink(path: &Path) -> io::Result<()> {
    let c = cstring(path_to_str(path)?)?;
    libc_zero(unsafe { libc::unlink(c.as_ptr()) })
}

pub fn rmdir(path: &Path) -> io::Result<()> {
    let c = cstring(path_to_str(path)?)?;
    libc_zero(unsafe { libc::rmdir(c.as_ptr()) })
}

pub fn unlinkat(dir: &File, rel: &str, flags: i32) -> io::Result<()> {
    let c = cstring(rel)?;
    libc_zero(unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), flags) })
}

pub fn unlinkat_file(dir: &File, rel: &str) -> io::Result<()> {
    unlinkat(dir, rel, 0)
}

pub fn unlinkat_dir(dir: &File, rel: &str) -> io::Result<()> {
    unlinkat(dir, rel, AT_REMOVEDIR)
}

pub fn link(old: &Path, new: &Path) -> io::Result<()> {
    let o = cstring(path_to_str(old)?)?;
    let n = cstring(path_to_str(new)?)?;
    libc_zero(unsafe { libc::link(o.as_ptr(), n.as_ptr()) })
}

pub fn linkat_into(old: &Path, new_dir: &File, new_rel: &str) -> io::Result<()> {
    let o = cstring(path_to_str(old)?)?;
    let n = cstring(new_rel)?;
    libc_zero(unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            o.as_ptr(),
            new_dir.as_raw_fd(),
            n.as_ptr(),
            0,
        )
    })
}

pub fn rename(old: &Path, new: &Path) -> io::Result<()> {
    let o = cstring(path_to_str(old)?)?;
    let n = cstring(path_to_str(new)?)?;
    libc_zero(unsafe { libc::rename(o.as_ptr(), n.as_ptr()) })
}

pub fn flinkat(source: &File, dir: &File, path: &str) -> io::Result<()> {
    std_fs_at(
        source,
        dir,
        path.as_bytes().to_vec(),
        StdFsCallKind::Flinkat,
        0,
    )
}

pub fn frenameat(
    old_dir: &File,
    old_path: &str,
    new_dir: &File,
    new_path: &str,
    flags: u32,
) -> io::Result<()> {
    if old_path.contains('\0') || new_path.contains('\0') {
        return Err(io::Error::from_raw_os_error(EINVAL));
    }
    let mut payload = Vec::with_capacity(old_path.len() + 1 + new_path.len());
    payload.extend_from_slice(old_path.as_bytes());
    payload.push(0);
    payload.extend_from_slice(new_path.as_bytes());
    std_fs_at(
        old_dir,
        new_dir,
        payload,
        StdFsCallKind::Frenameat,
        u64::from(flags),
    )
}

pub fn remove_tree(path: &Path) -> io::Result<()> {
    match unlink(path) {
        Ok(()) => return Ok(()),
        Err(e) if is_errno(&e, ENOENT) => return Ok(()),
        Err(e) if is_errno(&e, EISDIR) => {}
        Err(e) => return Err(e),
    }
    for name in read_dir_names(path)? {
        if name == "." || name == ".." {
            continue;
        }
        remove_tree(&path.join(name))?;
    }
    match rmdir(path) {
        Ok(()) => Ok(()),
        Err(e) if is_errno(&e, ENOENT) => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn cleanup_prefixed(
    root: &Path,
    prefix: &str,
) -> (usize, Vec<(std::path::PathBuf, io::Error)>) {
    let names = match read_dir_names(root) {
        Ok(n) => n,
        Err(_) => return (0, Vec::new()),
    };
    let mut ok = 0usize;
    let mut errs = Vec::new();
    for name in names {
        if !name.starts_with(prefix) {
            continue;
        }
        let p = root.join(&name);
        match remove_tree(&p) {
            Ok(()) => ok += 1,
            Err(e) => errs.push((p, e)),
        }
    }
    (ok, errs)
}

pub fn fpath(file: &File) -> io::Result<String> {
    let fd = Fd::new(file.as_raw_fd() as usize);
    let mut buf = [0u8; 1024];
    let n = fd.fpath(&mut buf);
    core::mem::forget(fd);
    let n = n.map_err(redox_to_io)?;
    let s = std::str::from_utf8(&buf[..n])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(s.to_owned())
}

pub fn scheme_dup(file: &File, buf: &[u8]) -> io::Result<usize> {
    let fd = Fd::new(file.as_raw_fd() as usize);
    let res = fd.dup(buf).map_err(redox_to_io);
    core::mem::forget(fd);
    res
}

pub fn fsync(file: &File) -> io::Result<()> {
    file.sync_all()
}

pub fn read_to_vec(path: &Path) -> io::Result<Vec<u8>> {
    let mut f = open_ro(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

pub fn write_all_path(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut f = create_rw(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

pub fn write_at(file: &mut File, offset: u64, data: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)?;
    Ok(())
}

pub fn read_exact_at(file: &mut File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn read_at(file: &mut File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    let mut got = 0;
    while got < len {
        match file.read(&mut buf[got..])? {
            0 => {
                buf.truncate(got);
                break;
            }
            n => got += n,
        }
    }
    Ok(buf)
}

pub fn is_dir(path: &Path) -> bool {
    open_dir(path).is_ok()
}

pub fn seek_end(file: &mut File) -> io::Result<u64> {
    file.seek(SeekFrom::End(0))
}

pub fn mtime(path: &Path) -> io::Result<SystemTime> {
    fs::metadata(path)?.modified()
}

pub fn atime(path: &Path) -> io::Result<SystemTime> {
    fs::metadata(path)?.accessed()
}

pub fn timespec(sec: i64, nsec: i64) -> libc::timespec {
    libc::timespec {
        tv_sec: sec as libc::time_t,
        tv_nsec: nsec as libc::c_long,
    }
}

pub fn futimens(file: &File, atime: libc::timespec, mtime: libc::timespec) -> io::Result<()> {
    let ts = [atime, mtime];
    libc_zero(unsafe { libc::futimens(file.as_raw_fd(), ts.as_ptr()) })
}

pub fn set_times_secs(file: &File, secs: i64) -> io::Result<()> {
    futimens(file, timespec(secs, 0), timespec(secs, 0))
}

pub fn futimens_now(file: &File) -> io::Result<()> {
    futimens(file, timespec(0, UTIME_NOW), timespec(0, UTIME_NOW))
}

pub fn chmod(path: &Path, mode: u32) -> io::Result<()> {
    let c = cstring(path_to_str(path)?)?;
    libc_zero(unsafe { libc::chmod(c.as_ptr(), mode as libc::mode_t) })
}

pub fn fchmod(file: &File, mode: u32) -> io::Result<()> {
    libc_zero(unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) })
}

pub fn fcntl(file: &File, cmd: i32, arg: i32) -> io::Result<i32> {
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), cmd, arg) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

pub fn fcntl_getfl(file: &File) -> io::Result<i32> {
    fcntl(file, libc::F_GETFL, 0)
}

pub fn fcntl_setfl(file: &File, flags: i32) -> io::Result<()> {
    fcntl(file, libc::F_SETFL, flags).map(|_| ())
}

pub fn fcntl_lk(file: &File, cmd: i32, fl: &mut Flock) -> io::Result<i32> {
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), cmd, fl as *mut Flock) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

pub fn fstatvfs(file: &File) -> io::Result<libc::statvfs> {
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    libc_zero(unsafe { libc::fstatvfs(file.as_raw_fd(), &mut st) })?;
    Ok(st)
}

pub fn statvfs(path: &Path) -> io::Result<libc::statvfs> {
    let c = cstring(path_to_str(path)?)?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    libc_zero(unsafe { libc::statvfs(c.as_ptr(), &mut st) })?;
    Ok(st)
}

pub fn fchown(file: &File, uid: u32, gid: u32) -> io::Result<()> {
    libc_zero(unsafe { libc::fchown(file.as_raw_fd(), uid, gid) })
}

pub fn chown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c = cstring(path_to_str(path)?)?;
    libc_zero(unsafe { libc::chown(c.as_ptr(), uid, gid) })
}

pub fn cp(src: &Path, dst: &Path, preserve: bool) -> io::Result<()> {
    let extra: &[&str] = if preserve {
        &["--preserve=mode,ownership,timestamps"]
    } else {
        &[]
    };
    let out = Command::new("cp").args(extra).arg(src).arg(dst).output()?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(io::Error::other(format!(
            "cp {} -> {} failed: {err}",
            src.display(),
            dst.display()
        )))
    }
}

pub fn read_dir_names(path: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for ent in fs::read_dir(path)? {
        let ent = ent?;
        names.push(ent.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

pub fn path_to_str(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path is not valid UTF-8"))
}

fn cstring(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::from_raw_os_error(EINVAL))
}

fn libc_zero(rc: i32) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn redox_to_io(err: RedoxError) -> io::Error {
    io::Error::from_raw_os_error(err.errno())
}

fn errno(err: &io::Error) -> Option<i32> {
    err.raw_os_error()
}

pub fn is_errno(err: &io::Error, want: i32) -> bool {
    errno(err) == Some(want)
}

pub fn gone(path: &Path) -> bool {
    match open_stat(path) {
        Err(e) => is_errno(&e, ENOENT),
        Ok(_) => false,
    }
}

pub fn listed(dir: &Path, name: &str) -> io::Result<bool> {
    Ok(read_dir_names(dir)?.iter().any(|n| n == name))
}

pub fn base_name(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

pub fn skip_if_missing(path: &Path, what: &str) -> Result<(), crate::Outcome> {
    match open_stat(path) {
        Ok(_) => Ok(()),
        Err(e) if is_errno(&e, ENOENT) => Err(crate::Outcome::Skip(format!(
            "{what} missing at {} - re-run scripts/gen-fixtures.sh",
            path.display()
        ))),
        Err(e) => Err(crate::Outcome::Fail(format!(
            "open {}: {e}",
            path.display()
        ))),
    }
}

pub fn close_enough(got: SystemTime, want: SystemTime, secs: u64) -> bool {
    got.duration_since(want)
        .or_else(|_| want.duration_since(got))
        .map(|d| d <= std::time::Duration::from_secs(secs))
        .unwrap_or(false)
}

pub fn pread(file: &File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let n = unsafe {
        libc::pread(
            file.as_raw_fd(),
            buf.as_mut_ptr() as *mut libc::c_void,
            len,
            offset as libc::off_t,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    Ok(buf)
}

pub fn pwrite(file: &File, offset: u64, data: &[u8]) -> io::Result<usize> {
    let n = unsafe {
        libc::pwrite(
            file.as_raw_fd(),
            data.as_ptr() as *const libc::c_void,
            data.len(),
            offset as libc::off_t,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

pub fn is_wouldblock(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock || is_errno(err, EWOULDBLOCK) || is_errno(err, EAGAIN)
}

pub fn epoll_add(file: &File, events: i32) -> io::Result<()> {
    let epfd = unsafe { libc::epoll_create1(0) };
    if epfd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut ev: libc::epoll_event = unsafe { std::mem::zeroed() };
    ev.events = events as u32;
    let rc = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, file.as_raw_fd(), &mut ev) };
    let err = io::Error::last_os_error();
    unsafe { libc::close(epfd) };
    if rc == 0 { Ok(()) } else { Err(err) }
}

pub fn select_readable(file: &File, timeout_ms: i32) -> io::Result<bool> {
    select_ready(file, true, false, timeout_ms)
}

pub fn select_writable(file: &File, timeout_ms: i32) -> io::Result<bool> {
    select_ready(file, false, true, timeout_ms)
}

fn select_ready(file: &File, read: bool, write: bool, timeout_ms: i32) -> io::Result<bool> {
    let fd = file.as_raw_fd();
    let mut rfds = unsafe { std::mem::zeroed::<libc::fd_set>() };
    let mut wfds = unsafe { std::mem::zeroed::<libc::fd_set>() };
    unsafe {
        libc::FD_ZERO(&mut rfds);
        libc::FD_ZERO(&mut wfds);
        if read {
            libc::FD_SET(fd, &mut rfds);
        }
        if write {
            libc::FD_SET(fd, &mut wfds);
        }
    }
    let mut tv = libc::timeval {
        tv_sec: (timeout_ms / 1000) as libc::time_t,
        tv_usec: libc::suseconds_t::from((timeout_ms % 1000) * 1000),
    };
    let n = unsafe {
        libc::select(
            fd + 1,
            if read {
                &mut rfds
            } else {
                std::ptr::null_mut()
            },
            if write {
                &mut wfds
            } else {
                std::ptr::null_mut()
            },
            std::ptr::null_mut(),
            &mut tv,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n > 0)
    }
}

pub fn poll_in(file: &File, timeout_ms: i32) -> io::Result<i16> {
    let mut pfd = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(pfd.revents)
    }
}
