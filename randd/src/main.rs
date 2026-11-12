use std::arch::asm;

use rand_chacha::ChaCha20Rng;
use rand_core::RngCore;
#[cfg(target_arch = "x86_64")]
use raw_cpuid::CpuId;
use redox_scheme::scheme::SchemeSync;
use redox_scheme::{CallerCtx, OpenResult, Socket};
use scheme_utils::{Blocking, FpathWriter, HandleMap};
use syscall::data::Stat;
use syscall::flag::{EventFlags, O_CREAT, O_EXCL, O_RDONLY, O_RDWR, O_WRONLY};
use syscall::schemev2::NewFdFlags;
use syscall::{Error, Result, EACCES, EBADF, EEXIST, ENOENT, EPERM, MODE_CHR};

// Create an RNG Seed to create initial seed from the rdrand intel instruction
#[rustfmt::skip]
use rand_core::SeedableRng;
use sha2::{Digest, Sha256};

// This Daemon implements a Cryptographically Secure Random Number Generator
// that does not block on read - i.e. it is equivalent to linux /dev/urandom
// We do not implement blocking reads as per linux /dev/random for the reasons outlined
// here: https://www.2uo.de/myths-about-urandom/

// Rand crate recommends at least 256 bits of entropy to seed the RNG
const SEED_BYTES: usize = 32;

/// Create a true random seed for the RNG if hardware support is present.
/// On Intel x64 from rdrand instruction.
/// On AArch64 from RNDRRS system register.
/// Will seed with a zero (insecure) if getting support is not present.
fn create_rdrand_seed() -> [u8; SEED_BYTES] {
    let mut rng = [0; SEED_BYTES];
    let mut have_seeded = false;
    #[cfg(target_arch = "x86_64")]
    {
        if CpuId::new().get_feature_info().unwrap().has_rdrand() {
            for i in 0..SEED_BYTES / 8 {
                // We get 8 bytes at a time from rdrand instruction
                let rand: u64;
                unsafe {
                    asm!("rdrand rax", out("rax") rand);
                }

                rng[i * 8..(i * 8 + 8)].copy_from_slice(&rand.to_le_bytes());
            }
            have_seeded = true;
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        fn is_cpu_feature_detected(feature: &str) -> bool {
            if let Ok(cpu) = std::fs::read_to_string("/scheme/sys/cpu") {
                let found = cpu
                    .lines()
                    .find_map(|s| s.strip_prefix("Features:"))
                    .and_then(|s| s.split(" ").find(|s| s == &feature))
                    .is_some();
                return found;
            }
            false
        }
        if is_cpu_feature_detected("rand") {
            let mut failure = false;
            for i in 0..SEED_BYTES / 8 {
                // We get 8 bytes at a time from RNDRRS register
                let rand: u64;
                unsafe {
                    asm!("mrs {}, s3_3_c2_c4_1", out(reg) rand); // rndrrs
                }
                failure |= rand == 0;
                rng[i * 8..(i * 8 + 8)].copy_from_slice(&rand.to_le_bytes());
            }
            have_seeded = !failure;
        }
    } // TODO integrate alternative entropy sources
    if !have_seeded {
        println!("randd: Seeding failed, no entropy source.  Random numbers on this platform are NOT SECURE");
    }
    rng
}

/// Contains information about an open file
struct OpenFileInfo {
    o_flags: usize,
}

impl OpenFileInfo {
    fn o_flag_set(&self, f: usize) -> bool {
        (f & self.o_flags) == f
    }
}

enum Handle {
    File(OpenFileInfo),
    SchemeRoot,
}

impl Handle {
    fn as_file(&self) -> Option<&OpenFileInfo> {
        match self {
            Self::File(info) => Some(info),
            _ => None,
        }
    }
}

/// Struct to represent the rand scheme.
struct RandScheme {
    prng: ChaCha20Rng,
    // ChaCha20 is a Cryptographically Secure PRNG
    // https://docs.rs/rand/0.5.0/rand/prng/chacha/struct.ChaChaRng.html
    // Allows 2^64 streams of random numbers, which we will equate with file numbers
    handles: HandleMap<Handle>,
}

impl RandScheme {
    /// Create new rand scheme from a message socket
    fn new() -> RandScheme {
        RandScheme {
            prng: ChaCha20Rng::from_seed(create_rdrand_seed()),
            handles: HandleMap::new(),
        }
    }

    /// Gets the open file info for a file descriptor if it is open - error otherwise.
    fn get_fd(&self, fd: usize) -> Result<&OpenFileInfo> {
        // Check we've got a valid file descriptor
        let handle = self.handles.get(fd)?;
        handle.as_file().ok_or(Error::new(EBADF))
    }
    /// Checks to see if the op (MODE_READ, MODE_WRITE) can be performed on the open file
    /// descriptor - Will return the open file info if successful, and error if the file
    /// descriptor is invalid, or the permission is denied.
    fn can_perform_op_on_fd(&self, fd: usize, f: usize) -> Result<()> {
        if !self.get_fd(fd)?.o_flag_set(f) {
            return Err(Error::new(EPERM));
        }
        Ok(())
    }
    /// Reseed the CSPRNG with the supplied entropy.
    /// TODO add this to an entropy pool and give a limited estimate to the amount of entropy
    /// TODO consider having trusted and untrusted entropy URIs, with different permissions.
    fn reseed_prng(&mut self, entropy: &[u8]) {
        // Need to fill a fixed size array for the from_seed, so we'll do 256 bit
        // array and has the entropy into it.
        let mut digest = Sha256::new();
        digest.input(entropy);
        let hash = digest.result();
        let mut entropy_array: [u8; SEED_BYTES] = [0; SEED_BYTES];
        entropy_array.copy_from_slice(hash.as_slice());
        self.prng = ChaCha20Rng::from_seed(entropy_array);
    }

    fn open_inner(&mut self, path: &str, flags: usize, ctx: &CallerCtx) -> Result<OpenResult> {
        // We are only allowing
        // reads/writes from /scheme/rand/ and /scheme/rand/urandom - the root directory on its own is passed as an empty slice
        if !path.is_empty() && path != "/urandom" {
            return Err(Error::new(ENOENT));
        }

        if flags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
            return Err(Error::new(EEXIST));
        }

        let open_file_info = OpenFileInfo { o_flags: flags };

        if (open_file_info.o_flag_set(O_WRONLY) || open_file_info.o_flag_set(O_RDWR))
            && ctx.uid != 0
        {
            return Err(Error::new(EPERM));
        }

        let id = self.handles.insert(Handle::File(open_file_info));

        Ok(OpenResult::ThisScheme {
            number: id,
            flags: NewFdFlags::empty(),
        })
    }
}

impl SchemeSync for RandScheme {
    fn scheme_root(&mut self) -> Result<usize> {
        Ok(self.handles.insert(Handle::SchemeRoot))
    }

    fn openat(
        &mut self,
        dirfd: usize,
        path: &str,
        flags: usize,
        _fcntl_flags: u32,
        ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        if !matches!(self.handles.get(dirfd)?, Handle::SchemeRoot) {
            return Err(Error::new(EACCES));
        }

        self.open_inner(path, flags, ctx)
    }

    /* Resource operations */
    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        _offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        // Check fd and permissions
        self.can_perform_op_on_fd(id, O_RDONLY)?;

        // Setting the stream will ensure that if two clients are reading concurrently, they won't get the same numbers
        self.prng.set_stream(id as u64); // Should probably find a way to re-instate the counter for this stream, but
                                         // not doing so won't make the output any less 'random'
        self.prng.fill_bytes(buf);

        Ok(buf.len())
    }

    fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        _offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        // Check fd and permissions
        self.can_perform_op_on_fd(id, O_WRONLY)?;

        // TODO - when we support other entropy sources, just add this to an entropy pool
        // TODO - consider having trusted and untrusted entropy writing paths
        // We have a healthy mistrust of the entropy we're being given, so we won't seed just with
        // that as the resulting numbers would be predictable based on this input
        // we'll take 512 bits (arbitrary) from the current PRNG, and seed with that
        // and the supplied data.

        let mut rng_buf: [u8; SEED_BYTES] = [0; SEED_BYTES];
        self.prng.fill_bytes(&mut rng_buf);
        let mut rng_vec = Vec::new();
        rng_vec.extend(&rng_buf);
        rng_vec.extend(buf);
        self.reseed_prng(&rng_vec);
        Ok(buf.len())
    }

    fn fcntl(&mut self, _id: usize, _cmd: usize, _arg: usize, _ctx: &CallerCtx) -> Result<usize> {
        // Just ignore this.
        Ok(0)
    }

    fn fevent(&mut self, _id: usize, _flags: EventFlags, _ctx: &CallerCtx) -> Result<EventFlags> {
        Ok(EventFlags::EVENT_READ)
    }
    fn fpath(&mut self, _file: usize, buf: &mut [u8], _ctx: &CallerCtx) -> Result<usize> {
        FpathWriter::with(buf, "rand", |_| Ok(()))
    }

    fn fstat(&mut self, _file: usize, stat: &mut Stat, _ctx: &CallerCtx) -> Result<()> {
        stat.st_mode = 0o644 | MODE_CHR;
        stat.st_uid = 0;
        stat.st_gid = 0;

        Ok(())
    }

    fn on_close(&mut self, file: usize) {
        // just remove the file descriptor from the open descriptors
        self.handles.remove(file);
    }
}

fn daemon(daemon: daemon::SchemeDaemon) -> ! {
    let socket = Socket::create().expect("randd: failed to create rand scheme");

    let mut scheme = RandScheme::new();
    let handler = Blocking::new(Box::new(socket), 16);

    let _ = daemon.ready_sync_scheme(handler.socket(), &mut scheme);

    libredox::call::setrens(0, 0).expect("randd: failed to enter null namespace");

    handler
        .process_requests_blocking(scheme)
        .expect("randd: failed to process events from zero scheme");
}

fn main() {
    daemon::SchemeDaemon::new(daemon);
}
