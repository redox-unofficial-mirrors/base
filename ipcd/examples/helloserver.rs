use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

fn main() -> io::Result<()> {
    let server = File::create("chan:hello")?;

    loop {
        let stream = libredox::call::dup(server.as_raw_fd() as usize, b"listen")?;
        let mut stream = unsafe { File::from_raw_fd(stream as RawFd) };

        stream.write(b"Hello World!\n")?;
    }
}
