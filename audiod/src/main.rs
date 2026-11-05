//! The audio daemon for RedoxOS.
use std::mem::MaybeUninit;
use std::ptr::addr_of_mut;
use std::sync::{Arc, Mutex};
use std::{process, thread};

use anyhow::Context;
use audiod::AudioChunk;
use daemon::SchemeDaemon;
use ioslice::IoSlice;
use libredox::error::Result;
use libredox::{flag, Fd};
use redox_scheme::Socket;
use scheme_utils::ReadinessBased;

use self::scheme::{AudioScheme, AudioSchemeInner};

mod scheme;

extern "C" fn sigusr_handler(_sig: usize) {}

fn uring_thread(inner_mutex: Arc<Mutex<AudioSchemeInner>>, pid: usize, shm_fd: Fd) -> Result<()> {
    let mut ring = redox_rings::user::Producer::<AudioChunk>::from_fd(shm_fd, false, None)?;
    loop {
        let buffer = {
            let mut inner = inner_mutex.lock().unwrap();
            inner.buffer()
        };
        // Wake up the scheme thread
        libredox::call::kill(pid, libredox::flag::SIGUSR1 as u32)?;

        loop {
            match ring.push(buffer) {
                Ok(_) => {
                    break;
                }
                Err(_) => {
                    thread::sleep(std::time::Duration::from_micros(100));
                }
            }
        }
    }
}

fn scheme_thread(inner_mutex: Arc<Mutex<AudioSchemeInner>>, pid: usize, hw_file: Fd) -> Result<()> {
    loop {
        let buffer = {
            let mut inner = inner_mutex.lock().unwrap();
            inner.buffer()
        };
        // Wake up the scheme thread
        libredox::call::kill(pid, libredox::flag::SIGUSR1 as u32)?;

        let buffer_u8 = unsafe {
            core::slice::from_raw_parts(buffer.as_ptr() as *const u8, size_of_val(&buffer))
        };

        hw_file.write(&buffer_u8)?;
    }
}

fn daemon(daemon: SchemeDaemon) -> anyhow::Result<()> {
    // Handle signals from the hw thread

    let new_sigaction = unsafe {
        let mut sigaction = MaybeUninit::<libc::sigaction>::uninit();
        addr_of_mut!((*sigaction.as_mut_ptr()).sa_flags).write(0);
        libc::sigemptyset(addr_of_mut!((*sigaction.as_mut_ptr()).sa_mask));
        addr_of_mut!((*sigaction.as_mut_ptr()).sa_sigaction)
            .write(sigusr_handler as *const () as usize);
        sigaction.assume_init()
    };
    libredox::call::sigaction(flag::SIGUSR1, Some(&new_sigaction), None)?;

    let pid = libredox::call::getpid()?;

    let hw_file = Fd::open("/scheme/audiohw", flag::O_WRONLY | flag::O_CLOEXEC, 0)?;

    let socket = Socket::create().context("failed to create scheme")?;

    let mut scheme = AudioScheme::new();

    let _ = daemon.ready_sync_scheme(&socket, &mut scheme).unwrap();

    // Enter a constrained namespace
    let ns = libredox::call::mkns(&[
        IoSlice::new(b"memory"),
        IoSlice::new(b"rand"), // for HashMap
    ])
    .context("failed to make namespace")?;
    libredox::call::setns(ns).context("failed to set namespace")?;

    // Spawn a thread to mix and send audio data
    let inner_thread = scheme.inner.clone();

    let _join_handle = thread::spawn(move || scheme_thread(inner_thread, pid, hw_file).unwrap());
    /*TODO: fix crackling when using ring
    let _join_handle = match hw_file.openat("audio_shm", 0, 0) {
        Ok(shm_fd) => thread::spawn(move || uring_thread(inner_thread, pid, shm_fd).unwrap()),
        Err(_) => {
            println!("audiod: uring communication is not supported by the audio driver. falling back to standard IO");
            thread::spawn(move || scheme_thread(inner_thread, pid, hw_file).unwrap())
        }
    };
    */

    let mut readiness = ReadinessBased::new(Box::new(socket), 16);

    loop {
        readiness.read_and_process_requests(&mut scheme)?;
        readiness.poll_all_requests(&mut scheme)?;
        readiness.write_responses()?;
    }
}

fn main() {
    SchemeDaemon::new(inner);
}

fn inner(x: SchemeDaemon) -> ! {
    match daemon(x) {
        Ok(()) => {
            process::exit(0);
        }
        Err(err) => {
            eprintln!("audiod: {}", err);
            process::exit(1);
        }
    }
}
