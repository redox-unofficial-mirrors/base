//! Disk scheme implementing the NBD (Network Block Device) protocol.
//!
//! <https://github.com/NetworkBlockDevice/nbd/blob/master/doc/proto.md>

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};

use driver_block::{Disk, DiskScheme, ExecutorTrait, TrivialExecutor};
use syscall::error::*;

mod spec;
use spec::*;

const SECTOR_SIZE: u32 = 512;

struct NbdDisk {
    conn: TcpStream,
    size: u64,
}

impl NbdDisk {
    fn new(addr: impl ToSocketAddrs, export_name: &str) -> anyhow::Result<NbdDisk> {
        let mut conn = TcpStream::connect(addr)?;

        // FIXME Workaround for netstack bug when SYN hasn't been ACKed by remote
        std::thread::sleep(std::time::Duration::from_millis(100));

        let mut buf = [0; 8];
        conn.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"NBDMAGIC");

        let mut buf = [0; 8];
        conn.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"IHAVEOPT");

        let mut handshake_flags = [0; 2];
        conn.read_exact(&mut handshake_flags).unwrap();
        let handshake_flags = u16::from_be_bytes(handshake_flags);
        assert!(handshake_flags & NBD_FLAG_FIXED_NEWSTYLE != 0);
        assert!(handshake_flags & NBD_FLAG_NO_ZEROES != 0);

        let client_flags = NBD_FLAG_C_FIXED_NEWSTYLE | NBD_FLAG_C_NO_ZEROES;
        conn.write_all(&client_flags.to_be_bytes()).unwrap();

        conn.write_all(b"IHAVEOPT")?;
        conn.write_all(&NBD_OPT_EXPORT_NAME.to_be_bytes())?;
        conn.write_all(&(export_name.len() as u32).to_be_bytes())?;
        conn.write_all(export_name.as_bytes())?;

        let mut size = [0; 8];
        conn.read_exact(&mut size).unwrap();
        let size = u64::from_be_bytes(size);

        let mut transmission_flags = [0; 2];
        conn.read_exact(&mut transmission_flags).unwrap();
        let transmission_flags = u16::from_be_bytes(transmission_flags);
        assert!(transmission_flags & NBD_FLAG_HAS_FLAGS != 0);

        Ok(NbdDisk { conn, size })
    }

    // FIXME allow multiple in-flight requests at the same time
    fn request(
        &mut self,
        flags: u16,
        type_: u16,
        offset: u64,
        length: u32,
        write_data: &[u8],
        read_data: &mut [u8],
    ) -> anyhow::Result<()> {
        let cookie = 0xfefefefededededeu64;

        self.conn.write_all(&NBD_REQUEST_MAGIC.to_be_bytes())?;
        self.conn.write_all(&flags.to_be_bytes())?;
        self.conn.write_all(&type_.to_be_bytes())?;
        self.conn.write_all(&cookie.to_be_bytes())?; // FIXME cookie
        self.conn.write_all(&offset.to_be_bytes())?;
        self.conn.write_all(&length.to_be_bytes())?;
        self.conn.write_all(write_data)?;

        let mut magic = [0; 4];
        self.conn.read_exact(&mut magic)?;
        let magic = u32::from_be_bytes(magic);
        assert_eq!(magic, NBD_SIMPLE_REPLY_MAGIC);

        let mut error = [0; 4];
        self.conn.read_exact(&mut error)?;
        let error = u32::from_be_bytes(error);
        assert_eq!(error, 0);

        let mut reply_cookie = [0; 8];
        self.conn.read_exact(&mut reply_cookie)?;
        let reply_cookie = u64::from_be_bytes(reply_cookie);
        assert_eq!(reply_cookie, cookie);

        if error == 0 {
            self.conn.read_exact(read_data)?;
        }

        Ok(())
    }
}

impl Disk for NbdDisk {
    fn block_size(&self) -> u32 {
        SECTOR_SIZE as u32
    }

    fn size(&self) -> u64 {
        self.size
    }

    async fn read(&mut self, block: u64, buffer: &mut [u8]) -> syscall::Result<usize> {
        if block as usize * SECTOR_SIZE as usize + buffer.len() > self.size as usize {
            return Err(syscall::Error::new(EINVAL));
        }
        self.request(
            0,
            NBD_CMD_READ,
            block * u64::from(SECTOR_SIZE),
            buffer.len() as u32,
            &[],
            buffer,
        )
        .unwrap();
        Ok(buffer.len())
    }

    async fn write(&mut self, block: u64, buffer: &[u8]) -> syscall::Result<usize> {
        if block as usize * SECTOR_SIZE as usize + buffer.len() > self.size as usize {
            return Err(syscall::Error::new(EINVAL));
        }
        self.request(
            0,
            NBD_CMD_WRITE,
            block * u64::from(SECTOR_SIZE),
            buffer.len() as u32,
            buffer,
            &mut [],
        )
        .unwrap();
        Ok(buffer.len())
    }
}

fn main() {
    daemon::Daemon::new(daemon);
}

fn daemon(daemon: daemon::Daemon) -> ! {
    let network_addr = std::env::args().nth(1).expect("network address argument");

    let event_queue = event::EventQueue::new().unwrap();

    event::user_data! {
        enum Event {
            Scheme,
        }
    };

    let mut scheme = DiskScheme::new(
        Some(daemon),
        format!("disk.nbd-{}", network_addr.replace(':', "-")),
        BTreeMap::from([(
            0,
            NbdDisk::new(network_addr, "").unwrap_or_else(|err| {
                eprintln!("failed to initialize nbd scheme: {}", err);
                std::process::exit(1)
            }),
        )]),
        &TrivialExecutor,
    );

    libredox::call::setrens(0, 0).expect("lived: failed to enter null namespace");

    event_queue
        .subscribe(
            scheme.event_handle().raw(),
            Event::Scheme,
            event::EventFlags::READ,
        )
        .unwrap();

    for event in event_queue {
        match event.unwrap().user_data {
            Event::Scheme => TrivialExecutor.block_on(scheme.tick()).unwrap(),
        }
    }

    std::process::exit(0);
}
