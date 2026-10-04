use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

fn from_libredox_error(error: libredox::error::Error) -> io::Error {
    io::Error::from_raw_os_error(error.errno())
}
fn dup(file: &File, buf: &str) -> io::Result<File> {
    let stream = libredox::call::dup(file.as_raw_fd() as usize, buf.as_bytes())
        .map_err(from_libredox_error)?;
    Ok(unsafe { File::from_raw_fd(stream as RawFd) })
}

fn main() -> io::Result<()> {
    let mut buf = [0; 5];
    let server = File::create("chan:")?;

    let mut client = dup(&server, "connect")?;
    let mut stream = dup(&server, "listen")?;

    println!("Testing basic I/O...");

    stream.write(b"abc")?;
    stream.flush()?;
    println!("-> Wrote message");

    assert_eq!(client.read(&mut buf)?, 3);
    assert_eq!(&buf[..3], b"abc");
    println!("-> Read message");

    println!("Testing connecting to unnamed socket by name (makes no sense)...");
    assert_eq!(
        File::open("chan:").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );

    println!("Everything tested!");
    Ok(())
}
