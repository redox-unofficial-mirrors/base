use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

use crate::{Outcome, TestCase, TestCtx, cases, check, check_ok, fsutil};

pub fn cases() -> Vec<TestCase> {
    cases![func io_golden_buffer]
}

const OPS: u32 = 256;
const MAX_LEN: usize = 64 * 1024;

fn io_golden_buffer(ctx: &TestCtx) -> Outcome {
    let path = ctx.work("fsx.bin");
    check_ok!(fsutil::write_all_path(&path, b""));
    let seed = ctx.cfg.run_id.bytes().fold(0x9e37_79b9_u32, |h, b| {
        h.wrapping_mul(16777619) ^ u32::from(b)
    });
    let mut rng = XorShift32::new(seed | 1);
    let mut good = vec![0u8; MAX_LEN];
    let mut file_len: usize = 0;
    let mut file = check_ok!(OpenOptions::new().read(true).write(true).open(&path));

    for step in 0..OPS {
        let op = rng.next() % 5;
        match op {
            0 | 1 => {
                let off = (rng.next() as usize) % MAX_LEN;
                let n = 1 + (rng.next() as usize) % 4096;
                let n = n.min(MAX_LEN - off);
                let buf: Vec<u8> = (0..n).map(|i| ((step + i as u32) % 251) as u8).collect();
                if off > file_len {
                    good[file_len..off].fill(0);
                }
                good[off..off + n].copy_from_slice(&buf);
                file_len = file_len.max(off + n);
                check_ok!(file.seek(SeekFrom::Start(off as u64)), "seek write {step}");
                check_ok!(file.write_all(&buf), "write {step}");
            }
            2 => {
                let new_len = (rng.next() as usize) % (MAX_LEN + 1);
                if new_len > file_len {
                    good[file_len..new_len].fill(0);
                }
                file_len = new_len;
                check_ok!(file.set_len(new_len as u64), "truncate {step}");
            }
            3 => {
                if file_len == 0 {
                    continue;
                }
                let off = (rng.next() as usize) % file_len;
                let n = 1 + (rng.next() as usize) % 4096;
                let n = n.min(file_len - off);
                let got = check_ok!(fsutil::pread(&file, off as u64, n), "pread {step}");
                check!(
                    got == good[off..off + got.len()],
                    "miscompare step={step} seed={seed:#x} off={off:#x} n={}",
                    got.len()
                );
            }
            _ => {
                if rng.next().is_multiple_of(2) {
                    check_ok!(file.sync_all(), "fsync {step}");
                }
                drop(file);
                file = check_ok!(
                    OpenOptions::new().read(true).write(true).open(&path),
                    "reopen {step}"
                );
                let on_disk = check_ok!(fsutil::fstat(&file), "len {step}").st_size as u64;
                check!(
                    on_disk == file_len as u64,
                    "size step={step}: disk={on_disk} golden={file_len}"
                );
            }
        }
    }

    drop(file);
    let got = check_ok!(fsutil::read_to_vec(&path));
    check!(
        got == good[..file_len],
        "final image mismatch seed={seed:#x} len={file_len}"
    );
    Outcome::PassNote(format!("{OPS} ops, seed={seed:#x}, final_len={file_len}"))
}

struct XorShift32(u32);

impl XorShift32 {
    fn new(seed: u32) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
}
