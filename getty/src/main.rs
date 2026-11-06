use std::error::Error;
use std::ffi::CStr;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Stderr, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::str;
use std::time::{Duration, Instant};

use libc::{EAGAIN, O_NOCTTY, O_NONBLOCK, grantpt, ptsname, unlockpt};

const _MAN_PAGE: &str = /* @MANSTART{getty} */
    r#"
NAME
    getty - set terminal mode

SYNOPSIS
    getty [-J | --noclear | -C | --contain ] tty
    getty [ -h | --help ]

DESCRIPTION
    The getty utility is called by init(8) to open and initialize the tty line,
    read a login name, and invoke login(1).

OPTIONS

    -h, --help
        Display this help and exit.

    -J, --noclear
        Do not clear the screen before forking login(1).

    --no-contain
        Run login instead of contain_login even when /etc/contain.toml exists

AUTHOR
    Written by Jeremy Soller.
"#; /* @MANEND */

const DEFAULT_COLS: u16 = 80;
const DEFAULT_LINES: u16 = 30;

/// Print error message to standard error, and exit with code, _1_.
fn fail(s: &str, stderr: &mut io::Stderr) -> ! {
    let mut stderr = stderr.lock();

    let _ = stderr.write(b"error: ");
    let _ = stderr.write(s.as_bytes());
    let _ = stderr.write(b"\n");
    let _ = stderr.flush();
    std::process::exit(1);
}

const TTY: mio::Token = mio::Token(0);
const MASTER: mio::Token = mio::Token(1);

pub fn handle(event_queue: &mut mio::Poll, tty: &mut File, master: &mut File, process: &mut Child) {
    // tty_fd => Display
    // master_fd => PTY

    let mut handle_event = |event_id: mio::Token| {
        if event_id == TTY {
            let mut packet = [0; 4096];
            loop {
                let count = match tty.read(&mut packet) {
                    Ok(0) => return,
                    Ok(count) => count,
                    Err(ref err) if err.raw_os_error() == Some(EAGAIN) => break,
                    Err(_) => panic!("getty: failed to read from TTY"),
                };

                write_nonblocking(master, &packet[..count], "master PTY");
            }
        } else if event_id == MASTER {
            let mut packet = [0; 4096];
            loop {
                let count = match master.read(&mut packet) {
                    Ok(0) => return,
                    Ok(count) => count,
                    Err(ref err) if err.raw_os_error() == Some(EAGAIN) => break,
                    Err(_) => panic!("getty: failed to read from master TTY"),
                };

                write_nonblocking(tty, &packet[1..count], "TTY");
                if packet[0] & 1 == 1 {
                    let _ = tty.sync_all();
                }
            }
        }
    };

    handle_event(TTY);
    handle_event(MASTER);

    'events: loop {
        let mut events = mio::Events::with_capacity(2);
        event_queue
            .poll(&mut events, None)
            .expect("getty: failed to read event file");
        for event in events.iter() {
            handle_event(event.token());
        }

        match process.try_wait() {
            Ok(status) => match status {
                Some(_code) => break 'events,
                None => (),
            },
            Err(err) => match err.kind() {
                ErrorKind::WouldBlock => (),
                _ => panic!("getty: failed to wait on child: {:?}", err),
            },
        }
    }

    let _ = process.kill();
    process.wait().expect("getty: failed to wait on login");
}

fn write_nonblocking(file: &mut File, mut data: &[u8], name: &str) {
    while !data.is_empty() {
        match file.write(data) {
            Ok(0) => panic!("getty: failed to write to {}: zero bytes written", name),
            Ok(n) => data = &data[n..],
            Err(ref err) if err.kind() == ErrorKind::Interrupted => {
                // signal interrupt can be retried in instant
            }
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
                std::thread::yield_now();
            }
            Err(err) => panic!("getty: failed to write to {}: {:?}", name, err),
        }
    }
}

pub fn getpty(columns: u16, lines: u16) -> (File, String) {
    let master = File::options()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(O_NONBLOCK | O_NOCTTY)
        .open("/dev/ptmx")
        .expect("getty: failed to create PTY");

    if unsafe {
        libc::ioctl(
            master.as_raw_fd(),
            libc::TIOCSWINSZ,
            &libc::winsize {
                ws_row: lines,
                ws_col: columns,
                ws_xpixel: columns * 8,
                ws_ypixel: lines * 16,
            },
        ) != 0
    } {
        eprintln!("failed to set pty size: {}", io::Error::last_os_error());
    }

    let _ = unsafe { grantpt(master.as_raw_fd()) };
    let _ = unsafe { unlockpt(master.as_raw_fd()) };

    let name = unsafe { CStr::from_ptr(ptsname(master.as_raw_fd())) };
    (
        master,
        name.to_str()
            .expect("ptsname returned non-UTF-8")
            .to_owned(),
    )
}

// termion cursor_pos prone to error and does not work on nonblocking files
fn tty_cursor_pos(tty: &mut File) -> Result<(u16, u16), Box<dyn Error>> {
    write!(tty, "\x1B[6n")?;
    tty.flush()?;

    let timeout = Duration::from_millis(500);
    let instant = Instant::now();
    let mut data = String::new();
    while instant.elapsed() < timeout {
        let mut bytes = [0];
        match tty.read(&mut bytes) {
            Ok(count) => {
                if count == 1 {
                    let c = bytes[0] as char;
                    if c == 'R' {
                        break;
                    }
                    data.push(c);
                }
            }
            Err(err) => {
                if err.kind() != ErrorKind::WouldBlock {
                    return Err(err.into());
                }
            }
        }
    }

    if data.is_empty() {
        return Err("cursor position timed out".into());
    }

    let beg = data.rfind('[').ok_or("failed to find [")?;
    let coords: String = data.chars().skip(beg + 1).collect();
    let mut nums = coords.split(';');

    let row = nums.next().ok_or("failed to find row")?.parse::<u16>()?;
    let col = nums.next().ok_or("failed to find col")?.parse::<u16>()?;

    Ok((col, row))
}

fn tty_columns_lines(tty: &mut File) -> Result<(u16, u16), Box<dyn Error>> {
    write!(tty, "{}", termion::cursor::Save)?;
    tty.flush()?;

    write!(tty, "{}", termion::cursor::Goto(999, 999))?;
    tty.flush()?;

    let res = tty_cursor_pos(tty);

    write!(tty, "{}", termion::cursor::Restore)?;
    tty.flush()?;

    res
}

fn daemon(tty: &mut File, clear: bool, contain: bool, stderr: &mut Stderr) {
    let (columns, lines) = tty_columns_lines(tty).unwrap_or((DEFAULT_COLS, DEFAULT_LINES));

    let (mut master, pty) = getpty(columns, lines);

    // FIXME maybe switch to mio?
    let mut event_queue = mio::Poll::new().expect("getty: failed to open event queue");

    event_queue
        .registry()
        .register(
            &mut mio::unix::SourceFd(&tty.as_raw_fd()),
            mio::Token(0),
            mio::Interest::READABLE,
        )
        .expect("getty: failed to fevent TTY");

    event_queue
        .registry()
        .register(
            &mut mio::unix::SourceFd(&master.as_raw_fd()),
            mio::Token(1),
            mio::Interest::READABLE,
        )
        .expect("getty: failed to fevent master PTY");

    loop {
        if clear {
            let _ = tty.write_all(b"\x1Bc");
        }
        let _ = tty.sync_all();

        let slave_stdin = File::options()
            .read(true)
            .open(&pty)
            .expect("getty: failed to open slave stdin");
        let slave_stdout = File::options()
            .write(true)
            .open(&pty)
            .expect("getty: failed to open slave stdout");
        let slave_stderr = File::options()
            .write(true)
            .open(&pty)
            .expect("getty: failed to open slave stderr");

        let mut command = if contain {
            Command::new("contain_login")
        } else {
            Command::new("login")
        };
        command
            .stdin(slave_stdin.try_clone().unwrap())
            .stdout(slave_stdout)
            .stderr(slave_stderr)
            .env("TERM", "xterm-256color")
            .env("TTY", &pty);
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }

                // FIXME enable this once redox supports controlling ttys
                #[cfg(not(target_os = "redox"))]
                if libc::ioctl(slave_stdin.as_raw_fd(), libc::TIOCSCTTY, 1) != 0 {
                    return Err(io::Error::last_os_error());
                }

                Ok(())
            });
        }

        match command.spawn() {
            Ok(mut process) => {
                handle(&mut event_queue, tty, &mut master, &mut process);
            }
            Err(err) => fail(&format!("getty: failed to execute login: {}", err), stderr),
        }
    }
}

pub fn main() {
    let mut stderr = io::stderr();

    let args = clap::Command::new("getty")
        .author("Jeremy Soller")
        .about("Set terminal mode")
        .arg(clap::arg!(<TTY> ""))
        .arg(clap::arg!(NO_CLEAR: -J --"no-clear" "Do not clear the screen before forking"))
        .arg(clap::arg!(NO_CONTAIN: --"no-contain" "Run login instead of contain_login even when /etc/contain.toml exists"))
        .get_matches();

    let clear = !args.get_flag("NO_CLEAR");
    let contain =
        std::fs::exists("/etc/contain.toml").unwrap_or(false) && !args.get_flag("NO_CONTAIN");
    let vt = args.get_one::<String>("TTY").unwrap();

    let buf: String;
    let vt_path = if vt.parse::<usize>().is_ok() {
        buf = format!("/scheme/fbcon.{vt}");
        &*buf
    } else {
        vt
    };

    let mut tty = match File::options()
        .read(true)
        .write(true)
        .custom_flags(O_NONBLOCK)
        .open(vt_path)
    {
        Ok(tty) => tty,
        Err(err) => fail(
            &format!("getty: failed to open TTY {}: {}", vt_path, err),
            &mut stderr,
        ),
    };

    daemon(&mut tty, clear, contain, &mut stderr);
}
