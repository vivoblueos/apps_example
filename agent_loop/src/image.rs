// Copyright (c) 2026 vivo Mobile Communication Co., Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Flash-operation command for the esp32-flash0 device. The ioctl shim issues
// an ecall directly (RISC-V only), so this file compiles only on riscv32.
// Standalone flash-op branch, not merged to mainline.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::io::AsRawFd,
};

use core::arch::asm;
use core::ptr;
use libc::{c_int, c_ulong};

use crate::elf_stream::{self, ElfHeader, IO_CHUNK};

include!("image_stream_impl.rs");

const DEV: &str = "/dev/esp32-flash0";
const ESP32_FLASH_ERASE_RANGE: u32 = 0x40;
const ESP32_FLASH_MAP_EXEC: u32 = 0x44;
const FLASH_IOCTL_ABI_VERSION: u32 = 1;
const ESP32_FLASH_UNMAP: u32 = 0x45;
const ESP32_FLASH_QUERY_DRAM_SAFE: u32 = 0x46;
const CHUNK: usize = 4096;
// ESP32-C3 DRAM upper bound (SRAM ends at 0x3FCE0000). Bounds the load-time copy
// of RW segments so it never scribbles past physical SRAM. This file already
// targets esp32-flash0 (riscv32-only), so a board constant here adds no new
// architecture coupling.
const DRAM_TOP: u32 = 0x3FCE_0000;

// ELF embedded at build time. Lives at the repo root as `loader_app`; the path
// is relative to this source file (src/commands/), four levels up to the root.
// rustc records it in the dep file, so editing loader_app rebuilds shell.
// loader_app is a non-self-contained ELF: its .rodata/.data segment links to
// 0x3fcd6000 (SRAM, outside the XIP window). `image load` copies that segment
// from the in-hand ELF buf to its SRAM vaddr at load time (see copy_rw_segments)
// so .text (flash-XIP) finds its referenced data in place.
const PAYLOAD_ELF: &[u8] = include_bytes!("../../../../loader_app");
const PAYLOAD_DST: &str = "/data/payload.elf";

// ELF parsing constants.
const EI_MAG: [u8; 4] = [0x7f, 0x45, 0x4c, 0x46];
const ELFCLASS32: u8 = 1;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const EHDR_SIZE: usize = 52;
const PHDR_SIZE: usize = 32;

// Loadable Region (internal main flash) maps into the IROM window via the Flash
// MMU: physical [LOADABLE_REGION_BASE, END) -> virtual [XIP_VADDR_BASE, END).
// An ELF whose PT_LOAD segments are linked into this virtual range can be
// XIP-executed: lay each segment at (p_vaddr - XIP_VADDR_BASE) in the installed
// image, map, then call e_entry directly (its vaddr is then backed by flash).
const IROM_VADDR_BASE: u32 = 0x4200_0000;
const LOADABLE_REGION_BASE: u32 = 0x0020_0000;
const LOADABLE_REGION_END: u32 = 0x0030_0000;
const LOADABLE_REGION_SIZE: u32 = LOADABLE_REGION_END - LOADABLE_REGION_BASE;
const XIP_VADDR_BASE: u32 = IROM_VADDR_BASE + LOADABLE_REGION_BASE;
const XIP_VADDR_END: u32 = IROM_VADDR_BASE + LOADABLE_REGION_END;
const FLASH_MMU_PAGE_SIZE: u32 = 0x0001_0000;
const FLASH_SECTOR_SIZE: u32 = 4096;
const XIP_LOAD_TMP: &str = "/data/.xip_load.img";

// Persisted loader metadata so `image run` can recover after a board reset
// without re-installing. Direct-write + CRC commit (no rename: librs blueos
// Sys::rename is a no-op stub and fatfs has no InodeOps::rename override), so
// metadata_crc32 is the real integrity guarantee -- a torn write surfaces as a
// bad CRC on next recover and is refused, never executed.
const META_PATH: &str = "/data/image.meta";

// blueos_header::syscalls::NR::Ioctl discriminant (Nop=0, counted to 61).
const NR_IOCTL: usize = 61;

// Provides the bare C symbol `ioctl` that libc::ioctl references. librs does
// not export ioctl, and shell cannot link blueos_scal/bk_syscall!, so we issue
// the ecall directly. Fixed-3-arg signature: on riscv LP32 the first integer
// args go in a0/a1/a2 identically for variadic and non-variadic ABIs (no
// floating-point arg, so no register-class difference), matching how librs
// read/write/close are fixed-arg yet called from libc. Raw return: negative is
// errno, non-negative is success.
#[no_mangle]
pub unsafe extern "C" fn ioctl(fd: c_int, request: c_ulong, arg: usize) -> c_int {
    let a0 = fd as usize;
    let a1 = request as usize;
    let a2 = arg;
    let mut ret: usize;
    unsafe {
        asm!(
            "ecall",
            in("a7") NR_IOCTL,
            inlateout("a0") a0 => ret,
            in("a1") a1,
            in("a2") a2,
            options(nostack, preserves_flags),
        );
    }
    ret as c_int
}

// Wraps libc::ioctl (resolved by the `ioctl` symbol above). Raw return:
// negative is errno, non-negative is success.
fn dev_ioctl(dev: &File, req: u32, arg: usize) -> Result<(), String> {
    let fd = dev.as_raw_fd();
    let ret = unsafe { libc::ioctl(fd, req as libc::c_ulong, arg as *mut libc::c_void) };
    if ret < 0 {
        Err(format!("ioctl({:#x}, {}) failed: {}", req, arg, ret))
    } else {
        Ok(())
    }
}

// Little-endian readers for ELF fields (ELF32 LSB header is always LE).
fn u32_from_le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn u16_from_le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

// Reserved: direct pointer read from the mapped IROM window (no read ioctl).
// load uses build_xip_image's buffer-backed e_entry instead, but this dynamic
// read path is kept for future use. Caller ensures the image is mapped.
#[allow(dead_code)]
unsafe fn read_mapped(base: u32, off: usize, out: &mut [u8]) {
    let src = (base as usize + off) as *const u8;
    for (i, b) in out.iter_mut().enumerate() {
        *b = unsafe { src.add(i).read_volatile() };
    }
}

// IEEE 802.3 CRC32 (poly 0xEDB88320, init 0xFFFFFFFF, final XOR).
// Pure byte fold with no init/final XOR: caller seeds with 0xFFFFFFFF and
// applies ! at the end. Shared by crc32_of_file and CrcWriter so a streamed
// image can accumulate the same CRC without re-reading the file.
fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut crc = crc;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

fn crc32_of_file_at(file: &mut File, offset: u64, size: usize) -> Result<u32, String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek: {}", e))?;
    let mut crc = 0xFFFF_FFFF;
    let mut buf = [0u8; CHUNK];
    let mut done = 0usize;
    while done < size {
        let want = core::cmp::min(CHUNK, size - done);
        let n = file
            .read(&mut buf[..want])
            .map_err(|e| format!("read: {}", e))?;
        if n == 0 {
            return Err(format!("unexpected EOF at {}/{}", done, size));
        }
        crc = crc32_update(crc, &buf[..n]);
        done += n;
    }
    Ok(!crc)
}

fn crc32_of_file(file: &mut File, size: usize) -> Result<u32, String> {
    crc32_of_file_at(file, 0, size)
}

// Write wrapper that folds every written byte into a running CRC32, so
// build_xip_image can produce the expected image CRC while streaming to the
// temp file with zero extra reads.
struct CrcWriter<W: Write> {
    inner: W,
    crc: u32,
}

impl<W: Write> CrcWriter<W> {
    fn new(inner: W) -> Self {
        CrcWriter {
            inner,
            crc: 0xFFFF_FFFF,
        }
    }
    fn finalize(self) -> (W, u32) {
        (self.inner, !self.crc)
    }
}

impl<W: Write> Write for CrcWriter<W> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(b)?;
        self.crc = crc32_update(self.crc, &b[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn align_up_sector(value: u32) -> Result<u32, String> {
    value
        .checked_add(FLASH_SECTOR_SIZE - 1)
        .map(|v| v / FLASH_SECTOR_SIZE * FLASH_SECTOR_SIZE)
        .ok_or_else(|| "erase length overflow".to_string())
}

fn erase_range(dev: &File, region_offset: u32, length: u32) -> Result<(), String> {
    let mut req = EraseRangeRequest {
        version: FLASH_IOCTL_ABI_VERSION,
        size: core::mem::size_of::<EraseRangeRequest>() as u32,
        flags: 0,
        region_offset,
        length,
    };
    dev_ioctl(
        dev,
        ESP32_FLASH_ERASE_RANGE,
        &mut req as *mut EraseRangeRequest as usize,
    )
}

// Install a file at a region-relative offset. Erase is explicit and write does
// not auto-erase, matching esp_flash_erase_region + esp_flash_write semantics.
fn install_inner(
    path: &str,
    region_offset: u32,
    erase_size: u32,
    expected_crc: Option<u32>,
) -> Result<u32, String> {
    let mut src = File::open(path).map_err(|e| format!("open source '{}': {}", path, e))?;
    let size = usize::try_from(
        src.metadata()
            .map_err(|e| format!("metadata: {}", e))?
            .len(),
    )
    .map_err(|_| "source size does not fit usize".to_string())?;
    if size == 0 {
        return Err("source is empty".to_string());
    }
    let size_u32 = u32::try_from(size).map_err(|_| "source is too large".to_string())?;
    let end = region_offset
        .checked_add(size_u32)
        .ok_or_else(|| "install range overflow".to_string())?;
    if end > LOADABLE_REGION_SIZE || erase_size < size_u32 {
        return Err("install range exceeds loadable region".to_string());
    }

    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEV)
        .map_err(|e| format!("open {}: {}", DEV, e))?;

    erase_range(&dev, region_offset, erase_size)?;
    dev.seek(SeekFrom::Start(region_offset as u64))
        .map_err(|e| format!("seek device: {}", e))?;

    let mut buf = [0u8; CHUNK];
    let mut src_crc = 0xFFFF_FFFF;
    loop {
        let n = src
            .read(&mut buf)
            .map_err(|e| format!("read source: {}", e))?;
        if n == 0 {
            break;
        }
        dev.write_all(&buf[..n])
            .map_err(|e| format!("write device: {}", e))?;
        src_crc = crc32_update(src_crc, &buf[..n]);
    }
    let want = expected_crc.unwrap_or(!src_crc);
    let actual = crc32_of_file_at(&mut dev, region_offset as u64, size)?;
    if actual != want {
        return Err(format!(
            "verify failed: dev crc={:#010x} expected={:#010x}",
            actual, want
        ));
    }
    println!(
        "installed {} bytes at region offset {:#x}, crc={:#010x}",
        size, region_offset, actual
    );
    Ok(actual)
}

fn install(path: &str) -> Result<(), String> {
    let size = File::open(path)
        .and_then(|file| file.metadata())
        .map_err(|e| format!("metadata '{}': {}", path, e))?
        .len();
    let size = u32::try_from(size).map_err(|_| "source is too large".to_string())?;
    let erase_size = align_up_sector(size)?;
    install_inner(path, 0, erase_size, None).map(|_| ())
}

// Verify a manually installed image at region offset zero.
fn verify(path: &str) -> Result<(), String> {
    let mut src = File::open(path).map_err(|e| format!("open source '{}': {}", path, e))?;
    let size = usize::try_from(
        src.metadata()
            .map_err(|e| format!("metadata: {}", e))?
            .len(),
    )
    .map_err(|_| "source size does not fit usize".to_string())?;
    if size == 0 {
        return Err("source is empty".to_string());
    }
    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEV)
        .map_err(|e| format!("open {}: {}", DEV, e))?;
    let src_crc = crc32_of_file(&mut src, size)?;
    let dev_crc = crc32_of_file_at(&mut dev, 0, size)?;
    if src_crc != dev_crc {
        return Err(format!(
            "verify failed: src crc={:#010x} dev crc={:#010x}",
            src_crc, dev_crc
        ));
    }
    println!("verified {} bytes, crc={:#010x}", size, src_crc);
    Ok(())
}

// Erase only the persisted image range, then remove its metadata.
fn clear() -> Result<(), String> {
    let (meta, _) = read_meta()?;
    let erase_size = align_up_sector(meta.image_size)?;
    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEV)
        .map_err(|e| format!("open {}: {}", DEV, e))?;
    erase_range(&dev, meta.region_offset, erase_size)?;
    let _ = std::fs::remove_file(META_PATH);
    unsafe { CURRENT = None };
    println!(
        "cleared region offset {:#x}, {} bytes",
        meta.region_offset, erase_size
    );
    Ok(())
}

// Map a region-relative range and return its IROM virtual base.
fn map_exec(region_offset: u32, image_size: u32) -> Result<u32, String> {
    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEV)
        .map_err(|e| format!("open {}: {}", DEV, e))?;
    let mut req = MapExecRequest {
        version: FLASH_IOCTL_ABI_VERSION,
        size: core::mem::size_of::<MapExecRequest>() as u32,
        flags: 0,
        region_offset,
        image_size,
        mapped_address: 0,
    };
    dev_ioctl(
        &dev,
        ESP32_FLASH_MAP_EXEC,
        &mut req as *mut MapExecRequest as usize,
    )?;
    println!("mapped: segment_address={:#010x}", req.mapped_address);
    Ok(req.mapped_address)
}

// Manual `image map` debug entry. map_exec needs (region_offset, image_size),
// and after Step B the kernel no longer remembers the install size, so pull it
// from the persisted metadata. Same offset/size run() would use; no execute.
fn map_from_meta() -> Result<u32, String> {
    let (meta, _) = read_meta()?;
    map_exec(meta.region_offset, meta.image_size)
}

// Unmap: release the executable mapping, back to Idle. Caller must not be
// executing inside the mapped region.
fn unmap() -> Result<(), String> {
    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEV)
        .map_err(|e| format!("open {}: {}", DEV, e))?;
    dev_ioctl(&dev, ESP32_FLASH_UNMAP, 0)?;
    println!("unmapped");
    Ok(())
}

// Query the kernel DRAM safe base (__sys_stack_end): the top of kernel heap/stack
// usage, below which free SRAM for RW-segment copies begins. The kernel writes it
// back through the arg out-pointer. Device close() is a no-op, so opening a fresh
// fd here does not disturb the live Mapped state set by map_exec().
fn query_dram_safe() -> Result<u32, String> {
    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEV)
        .map_err(|e| format!("open {}: {}", DEV, e))?;
    let mut addr: u32 = 0;
    dev_ioctl(
        &dev,
        ESP32_FLASH_QUERY_DRAM_SAFE,
        &mut addr as *mut u32 as usize,
    )?;
    Ok(addr)
}

// Local copy of the kernel's versioned ioctl ABI.
#[repr(C)]
struct MapExecRequest {
    version: u32,
    size: u32,
    flags: u32,
    region_offset: u32,
    image_size: u32,
    mapped_address: u32,
}

#[repr(C)]
struct EraseRangeRequest {
    version: u32,
    size: u32,
    flags: u32,
    region_offset: u32,
    length: u32,
}

// Persisted loader metadata, written to META_PATH by load() and read back by
// recover_loaded_image(). On-disk layout = the fixed header (9 u32 = 36 bytes,
// little-endian) followed by elf_path_len raw bytes. metadata_crc32 covers the
// 32 bytes preceding it (magic..elf_path_len) and is the torn-write sentinel.
const META_MAGIC: u32 = 0x4D45_5441; // "META"
const META_VERSION: u32 = 2;
const META_HEADER_SIZE: usize = 36;
const MAX_META_PATH_LEN: usize = 512;

#[repr(C)]
#[derive(Clone, Copy)]
struct ImageMetadata {
    magic: u32,
    version: u32,
    region_offset: u32,
    image_size: u32,
    image_crc32: u32,
    elf_crc32: u32,
    entry: u32,
    elf_path_len: u32,
    metadata_crc32: u32,
}

// CRC covers the fixed header excluding its CRC field plus the trailing path.
fn metadata_crc(header: &[u8; 32], elf_path: &[u8]) -> u32 {
    let crc = crc32_update(0xFFFF_FFFF, header);
    !crc32_update(crc, elf_path)
}

// Serialize header (36B LE) + elf_path bytes. metadata_crc32 is computed over
// the first 32 bytes plus elf_path and is stored as the last header u32.
fn validate_meta_path(elf_path: &[u8]) -> Result<u32, String> {
    if elf_path.is_empty() || elf_path.len() > MAX_META_PATH_LEN {
        return Err(format!(
            "metadata path length {} exceeds limit {}",
            elf_path.len(), MAX_META_PATH_LEN
        ));
    }
    u32::try_from(elf_path.len()).map_err(|_| "metadata path length overflow".to_string())
}

fn serialize_meta(meta: &ImageMetadata, elf_path: &[u8]) -> Result<Vec<u8>, String> {
    let path_len = validate_meta_path(elf_path)?;
    if meta.elf_path_len != path_len {
        return Err("metadata path length does not match header".to_string());
    }
    let mut buf = Vec::with_capacity(META_HEADER_SIZE + elf_path.len());
    let mut hdr = [0u8; 32];
    hdr[0..4].copy_from_slice(&meta.magic.to_le_bytes());
    hdr[4..8].copy_from_slice(&meta.version.to_le_bytes());
    hdr[8..12].copy_from_slice(&meta.region_offset.to_le_bytes());
    hdr[12..16].copy_from_slice(&meta.image_size.to_le_bytes());
    hdr[16..20].copy_from_slice(&meta.image_crc32.to_le_bytes());
    hdr[20..24].copy_from_slice(&meta.elf_crc32.to_le_bytes());
    hdr[24..28].copy_from_slice(&meta.entry.to_le_bytes());
    hdr[28..32].copy_from_slice(&meta.elf_path_len.to_le_bytes());
    let crc = metadata_crc(&hdr, elf_path);
    buf.extend_from_slice(&hdr);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(elf_path);
    Ok(buf)
}

// Read and fully validate META_PATH: magic, version, elf_path_len consistency,
// and metadata_crc32. Returns the metadata and its trailing elf_path bytes.
fn read_meta() -> Result<(ImageMetadata, Vec<u8>), String> {
    let mut file = File::open(META_PATH).map_err(|e| format!("open {}: {}", META_PATH, e))?;
    let mut fixed = [0u8; META_HEADER_SIZE];
    file.read_exact(&mut fixed)
        .map_err(|error| format!("metadata CRC mismatch: truncated: {}", error))?;

    let mut header = [0u8; 32];
    header.copy_from_slice(&fixed[..32]);
    let stored_crc = u32_from_le(&fixed[32..36]);
    let meta = ImageMetadata {
        magic: u32_from_le(&header[0..4]),
        version: u32_from_le(&header[4..8]),
        region_offset: u32_from_le(&header[8..12]),
        image_size: u32_from_le(&header[12..16]),
        image_crc32: u32_from_le(&header[16..20]),
        elf_crc32: u32_from_le(&header[20..24]),
        entry: u32_from_le(&header[24..28]),
        elf_path_len: u32_from_le(&header[28..32]),
        metadata_crc32: stored_crc,
    };
    if meta.magic != META_MAGIC {
        return Err("metadata CRC mismatch: bad magic".to_string());
    }
    if meta.version != META_VERSION {
        return Err(format!("metadata version {} unsupported", meta.version));
    }
    let path_len = usize::try_from(meta.elf_path_len)
        .map_err(|_| "metadata CRC mismatch: path length overflow".to_string())?;
    let elf_path = elf_stream::read_bounded_tail(&mut file, path_len, MAX_META_PATH_LEN)
        .map_err(|error| format!("metadata CRC mismatch: {}", error))?;
    if metadata_crc(&header, &elf_path) != stored_crc {
        return Err("metadata CRC mismatch".to_string());
    }
    Ok((meta, elf_path))
}
// Commit metadata: direct overwrite of META_PATH then close+reopen to verify
// metadata_crc32. No tmp/rename (librs blueos Sys::rename is a no-op stub).
// A torn write fails the reread and surfaces as "metadata CRC mismatch" on the
// next read_meta -- safe: a bad meta is never executed.
fn write_meta(meta: &ImageMetadata, elf_path: &str) -> Result<(), String> {
    let bytes = serialize_meta(meta, elf_path.as_bytes())?;
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(META_PATH)
        .map_err(|e| format!("create {}: {}", META_PATH, e))?;
    f.write_all(&bytes)
        .map_err(|e| format!("write {}: {}", META_PATH, e))?;
    // Close first: BlueOS FATFS close -> fsync -> flush so the bytes (and the
    // dir entry/length) land on the block device before the verify read.
    drop(f);

    // Reread through the real block-device path to confirm metadata_crc32.
    let (reread, _) = read_meta()?;
    if reread.metadata_crc32 != meta.metadata_crc32 {
        return Err(format!(
            "metadata commit verify failed: wrote crc={:#010x} reread crc={:#010x}",
            meta.metadata_crc32, reread.metadata_crc32
        ));
    }
    println!("metadata committed: {}", META_PATH);
    Ok(())
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct XipLayout {
    mapped_vaddr_base: u32,
    physical_offset: u32,
    region_offset: u32,
    image_size: u32,
    erase_size: u32,
    entry: u32,
}

fn derive_xip_layout(elf: &[u8]) -> Result<XipLayout, String> {
    if elf.len() < EHDR_SIZE || elf[..4] != EI_MAG {
        return Err("bad ELF magic".to_string());
    }
    if elf[4] != ELFCLASS32 {
        return Err("not ELF32".to_string());
    }
    let entry = u32_from_le(&elf[24..28]);
    let phoff = u32_from_le(&elf[28..32]) as usize;
    let phentsize = u16_from_le(&elf[42..44]) as usize;
    let phnum = u16_from_le(&elf[44..46]) as usize;
    if phentsize < PHDR_SIZE || phnum == 0 {
        return Err(format!(
            "bad phdr table: entsize={} num={}",
            phentsize, phnum
        ));
    }

    let mut min_vaddr = u32::MAX;
    let mut span_end = 0u32;
    let mut entry_is_executable = false;
    for index in 0..phnum {
        let offset = phoff
            .checked_add(
                index
                    .checked_mul(phentsize)
                    .ok_or_else(|| "phdr overflow".to_string())?,
            )
            .ok_or_else(|| "phdr overflow".to_string())?;
        let end = offset
            .checked_add(PHDR_SIZE)
            .ok_or_else(|| "phdr overflow".to_string())?;
        if end > elf.len() {
            return Err("phdr past end of buffer".to_string());
        }
        let ph = &elf[offset..end];
        if u32_from_le(&ph[0..4]) != PT_LOAD {
            continue;
        }
        let filesz = u32_from_le(&ph[16..20]);
        if filesz == 0 {
            continue;
        }
        let vaddr = u32_from_le(&ph[8..12]);
        if !(XIP_VADDR_BASE..XIP_VADDR_END).contains(&vaddr) {
            continue;
        }
        let segment_end = vaddr
            .checked_add(filesz)
            .ok_or_else(|| "vaddr+filesz overflow".to_string())?;
        if segment_end > XIP_VADDR_END {
            return Err("XIP segment exceeds IROM window".to_string());
        }
        min_vaddr = core::cmp::min(min_vaddr, vaddr);
        span_end = core::cmp::max(span_end, segment_end);
        let flags = u32_from_le(&ph[24..28]);
        if flags & PF_X != 0 && (vaddr..segment_end).contains(&entry) {
            entry_is_executable = true;
        }
    }
    if min_vaddr == u32::MAX {
        return Err("no PT_LOAD segment falls in the XIP window".to_string());
    }
    if !entry_is_executable {
        return Err("e_entry is not inside an executable XIP segment".to_string());
    }

    let mapped_vaddr_base = min_vaddr & !(FLASH_MMU_PAGE_SIZE - 1);
    let physical_offset = mapped_vaddr_base
        .checked_sub(IROM_VADDR_BASE)
        .ok_or_else(|| "mapped base below IROM base".to_string())?;
    let region_offset = physical_offset
        .checked_sub(LOADABLE_REGION_BASE)
        .ok_or_else(|| "mapped base below loadable region".to_string())?;
    let image_size = span_end
        .checked_sub(mapped_vaddr_base)
        .ok_or_else(|| "XIP image size underflow".to_string())?;
    let erase_size = align_up_sector(image_size)?;
    let region_end = region_offset
        .checked_add(erase_size)
        .ok_or_else(|| "XIP region overflow".to_string())?;
    if region_end > LOADABLE_REGION_SIZE {
        return Err("XIP image exceeds loadable region".to_string());
    }

    Ok(XipLayout {
        mapped_vaddr_base,
        physical_offset,
        region_offset,
        image_size,
        erase_size,
        entry,
    })
}

fn verify_xip_segments(elf: &[u8], layout: &XipLayout) -> Result<usize, String> {
    let phoff = u32_from_le(&elf[28..32]) as usize;
    let phentsize = u16_from_le(&elf[42..44]) as usize;
    let phnum = u16_from_le(&elf[44..46]) as usize;
    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEV)
        .map_err(|e| format!("open {}: {}", DEV, e))?;
    let mut total = 0usize;
    let mut segment_index = 0usize;
    let mut scratch = vec![0u8; CHUNK];

    for index in 0..phnum {
        let offset = phoff + index * phentsize;
        if offset + PHDR_SIZE > elf.len() {
            return Err("phdr past end of buffer".to_string());
        }
        let ph = &elf[offset..offset + PHDR_SIZE];
        let filesz = u32_from_le(&ph[16..20]);
        if u32_from_le(&ph[0..4]) != PT_LOAD || filesz == 0 {
            continue;
        }
        let vaddr = u32_from_le(&ph[8..12]);
        if !(layout.mapped_vaddr_base..layout.mapped_vaddr_base + layout.image_size)
            .contains(&vaddr)
        {
            continue;
        }
        let source_offset = u32_from_le(&ph[4..8]);
        let source_end = source_offset
            .checked_add(filesz)
            .ok_or_else(|| "segment source overflow".to_string())?
            as usize;
        if source_end > elf.len() {
            return Err("segment exceeds ELF".to_string());
        }
        let device_offset = layout
            .region_offset
            .checked_add(vaddr - layout.mapped_vaddr_base)
            .ok_or_else(|| "segment device offset overflow".to_string())?;
        dev.seek(SeekFrom::Start(device_offset as u64))
            .map_err(|e| format!("seek segment {}: {}", segment_index, e))?;

        let expected = &elf[source_offset as usize..source_end];
        let mut compared = 0usize;
        while compared < expected.len() {
            let want = core::cmp::min(scratch.len(), expected.len() - compared);
            let count = dev
                .read(&mut scratch[..want])
                .map_err(|e| format!("read segment {}: {}", segment_index, e))?;
            if count == 0 {
                return Err(format!("segment {} unexpected EOF", segment_index));
            }
            if scratch[..count] != expected[compared..compared + count] {
                let bad = scratch[..count]
                    .iter()
                    .zip(&expected[compared..compared + count])
                    .position(|(actual, expected)| actual != expected)
                    .unwrap_or(0);
                return Err(format!(
                    "XIP segment {} verify failed: flash_off={:#x}, segment_off={:#x}, expected={:#04x}, actual={:#04x}",
                    segment_index,
                    device_offset,
                    compared + bad,
                    expected[compared + bad],
                    scratch[bad]
                ));
            }
            compared += count;
        }
        total += expected.len();
        segment_index += 1;
    }
    println!("verified {} actual XIP bytes", total);
    Ok(total)
}

fn validate_elf_for_meta(meta: &ImageMetadata, elf: &[u8]) -> Result<XipLayout, String> {
    let actual_crc = !crc32_update(0xFFFF_FFFF, elf);
    if actual_crc != meta.elf_crc32 {
        return Err(format!(
            "ELF CRC mismatch: actual={:#010x}, expected={:#010x}",
            actual_crc, meta.elf_crc32
        ));
    }
    let layout = derive_xip_layout(elf)?;
    if layout.entry != meta.entry {
        return Err(format!(
            "entry mismatch: elf={:#010x} meta={:#010x}",
            layout.entry, meta.entry
        ));
    }
    if layout.region_offset != meta.region_offset {
        return Err(format!(
            "region offset mismatch: elf={:#x} meta={:#x}",
            layout.region_offset, meta.region_offset
        ));
    }
    if layout.image_size != meta.image_size {
        return Err(format!(
            "image size mismatch: elf={} meta={}",
            layout.image_size, meta.image_size
        ));
    }
    Ok(layout)
}

// Power-loss recovery: no erase/write/MAP. Reads META_PATH, verifies metadata
// CRC, re-reads the ELF and verifies its CRC, cross-checks entry + image_size
// against the ELF, then verify_xip_segments. Any failure refuses to map/exec.
// Returns the metadata plus the in-hand ELF buffer (needed to restore
// out-of-window RW segments at execute time).
fn recover_loaded_image() -> Result<(ImageMetadata, File, ElfHeader), String> {
    println!("recovering installed loader");
    let (meta, elf_path_bytes) = read_meta()?;
    let elf_path = core::str::from_utf8(&elf_path_bytes)
        .map_err(|_| "metadata CRC mismatch: bad elf_path utf8".to_string())?;

    let mut file = File::open(elf_path).map_err(|e| format!("open '{}': {}", elf_path, e))?;
    let header = elf_stream::read_header(&mut file)?;
    let layout = validate_elf_for_meta_stream(&meta, &mut file, &header)?;
    verify_xip_segments_stream(&mut file, &header, &layout)?;
    Ok((meta, file, header))
}

// Restore out-of-window RW segments (loader_app's .data/.rodata link to SRAM,
// e.g. 0x3fcd6000): copy file bytes to p_vaddr and zero the BSS tail. The
// installed image only holds in-window segments, so these bytes come from the
// in-hand ELF buf, not flash. safe_base bounds the copy above kernel heap/stack.
//
// NOTE: the safe_base..DRAM_TOP check is a loader_app-specific coincidence, not a
// general invariant -- esp32c3 has no PHYS_DRAM_BASE and the buddy allocator is
// off, so this SRAM range is genuinely free today; enabling buddy would make it
// allocator-managed and this check would not protect against that.
fn copy_rw_segments(buf: &[u8], safe_base: u32) -> Result<(), String> {
    if buf.len() < EHDR_SIZE || buf[..4] != EI_MAG {
        return Err("bad ELF magic".to_string());
    }
    if buf[4] != ELFCLASS32 {
        return Err("not ELF32".to_string());
    }
    let e_phoff = u32_from_le(&buf[28..32]) as usize;
    let e_phentsize = u16_from_le(&buf[42..44]) as usize;
    let e_phnum = u16_from_le(&buf[44..46]) as usize;
    if e_phentsize < PHDR_SIZE || e_phnum == 0 {
        return Err(format!(
            "bad phdr table: entsize={} num={}",
            e_phentsize, e_phnum
        ));
    }

    for i in 0..e_phnum {
        let off = e_phoff + i * e_phentsize;
        if off + PHDR_SIZE > buf.len() {
            return Err("phdr past end of buffer".to_string());
        }
        let ph = &buf[off..off + PHDR_SIZE];
        let p_type = u32_from_le(&ph[0..4]);
        let p_filesz = u32_from_le(&ph[16..20]);
        if p_type != PT_LOAD || p_filesz == 0 {
            continue;
        }
        let p_vaddr = u32_from_le(&ph[8..12]);
        let p_offset = u32_from_le(&ph[4..8]);
        let p_memsz = u32_from_le(&ph[20..24]);
        if p_filesz > p_memsz {
            return Err(format!(
                "PT_LOAD filesz {} exceeds memsz {} at vaddr {:#010x}",
                p_filesz, p_memsz, p_vaddr
            ));
        }
        // Only out-of-window segments need copying; in-window ones are flash-XIP.
        if (XIP_VADDR_BASE..XIP_VADDR_END).contains(&p_vaddr) {
            continue;
        }
        let seg_end = p_vaddr
            .checked_add(p_memsz)
            .ok_or_else(|| "vaddr+memsz overflow".to_string())?;
        if p_vaddr < safe_base || seg_end > DRAM_TOP {
            return Err(format!(
                "RW seg at {:#010x}..{:#010x} outside safe SRAM [{:#010x},{:#010x})",
                p_vaddr, seg_end, safe_base, DRAM_TOP
            ));
        }
        let src_end = p_offset
            .checked_add(p_filesz)
            .ok_or_else(|| "offset+filesz overflow".to_string())? as usize;
        if src_end > buf.len() {
            return Err("segment file bytes past end of buffer".to_string());
        }
        // SAFETY: p_vaddr is a physical SRAM address on C3 (flat, no MMU/PMP) and
        // was just bounds-checked against [safe_base, DRAM_TOP). The source slice
        // lives in `buf` for the duration of the copy (non-overlapping by region).
        unsafe {
            ptr::copy_nonoverlapping(
                buf[p_offset as usize..src_end].as_ptr(),
                p_vaddr as *mut u8,
                p_filesz as usize,
            );
            let bss = p_memsz - p_filesz;
            if bss > 0 {
                ptr::write_bytes((p_vaddr + p_filesz) as *mut u8, 0, bss as usize);
            }
        }
        println!(
            "copied RW seg {:#010x}..{:#010x} (filesz={}, bss={})",
            p_vaddr,
            seg_end,
            p_filesz,
            p_memsz - p_filesz
        );
    }
    Ok(())
}

// Reserved: parse an ELF32 header off the mapped IROM window and locate the entry
// of the executable segment containing e_entry, by pointer (no read ioctl). load
// does not call this today (it uses build_xip_image's e_entry), but the path is kept.
#[allow(dead_code)]
fn entry_from_mapped(base: u32) -> Result<u64, String> {
    let mut ehdr = [0u8; EHDR_SIZE];
    unsafe { read_mapped(base, 0, &mut ehdr) };
    if ehdr[..4] != EI_MAG {
        return Err("bad ELF magic".to_string());
    }
    if ehdr[4] != ELFCLASS32 {
        return Err("not ELF32".to_string());
    }
    let e_entry = u32_from_le(&ehdr[24..28]);
    let e_phoff = u32_from_le(&ehdr[28..32]) as usize;
    let e_phentsize = u16_from_le(&ehdr[42..44]) as usize;
    let e_phnum = u16_from_le(&ehdr[44..46]) as usize;
    if e_phentsize < PHDR_SIZE || e_phnum == 0 {
        return Err(format!(
            "bad phdr table: entsize={} num={}",
            e_phentsize, e_phnum
        ));
    }

    // Find the executable segment that contains e_entry. The kernel maps the
    // whole installed image as-is, so a segment at any file offset is reachable.
    let mut phdr = [0u8; PHDR_SIZE];
    let mut found: Option<(u32, u32)> = None; // (p_offset, p_vaddr)
    for i in 0..e_phnum {
        let off = e_phoff + i * e_phentsize;
        unsafe { read_mapped(base, off, &mut phdr) };
        let p_type = u32_from_le(&phdr[0..4]);
        let p_offset = u32_from_le(&phdr[4..8]);
        let p_vaddr = u32_from_le(&phdr[8..12]);
        let p_filesz = u32_from_le(&phdr[16..20]);
        let p_flags = u32_from_le(&phdr[24..28]);
        if p_type == PT_LOAD
            && (p_flags & PF_X) != 0
            && e_entry >= p_vaddr
            && e_entry < p_vaddr.wrapping_add(p_filesz)
        {
            found = Some((p_offset, p_vaddr));
            break;
        }
    }
    let (p_offset, p_vaddr) =
        found.ok_or_else(|| "no PT_LOAD+PF_X segment contains e_entry".to_string())?;

    // The kernel maps the image as-is, so the entry's file offset is
    // p_offset + (e_entry - p_vaddr); add the mapped base to get the entry addr.
    let entry_off = p_offset.wrapping_add(e_entry.wrapping_sub(p_vaddr));
    Ok((base as u64).wrapping_add(entry_off as u64))
}

// Stream the compact XIP image relative to its selected 64-KB MMU page.
fn build_xip_image<W: Write>(elf: &[u8], out: &mut W) -> Result<XipLayout, String> {
    let layout = derive_xip_layout(elf)?;
    let phoff = u32_from_le(&elf[28..32]) as usize;
    let phentsize = u16_from_le(&elf[42..44]) as usize;
    let phnum = u16_from_le(&elf[44..46]) as usize;
    let mut segments = Vec::new();

    for index in 0..phnum {
        let offset = phoff + index * phentsize;
        if offset + PHDR_SIZE > elf.len() {
            return Err("phdr past end of buffer".to_string());
        }
        let ph = &elf[offset..offset + PHDR_SIZE];
        let filesz = u32_from_le(&ph[16..20]);
        if u32_from_le(&ph[0..4]) != PT_LOAD || filesz == 0 {
            continue;
        }
        let vaddr = u32_from_le(&ph[8..12]);
        if !(layout.mapped_vaddr_base..layout.mapped_vaddr_base + layout.image_size)
            .contains(&vaddr)
        {
            continue;
        }
        let source_offset = u32_from_le(&ph[4..8]);
        let source_end = source_offset
            .checked_add(filesz)
            .ok_or_else(|| "segment source overflow".to_string())?
            as usize;
        if source_end > elf.len() {
            return Err("segment file bytes past end of buffer".to_string());
        }
        segments.push((vaddr, source_offset, filesz));
    }
    segments.sort_by_key(|segment| segment.0);

    let padding = [0xFFu8; CHUNK];
    let mut cursor = 0usize;
    for (vaddr, source_offset, filesz) in segments {
        let destination = (vaddr - layout.mapped_vaddr_base) as usize;
        if destination < cursor {
            return Err("overlapping XIP PT_LOAD segments".to_string());
        }
        while cursor < destination {
            let count = core::cmp::min(CHUNK, destination - cursor);
            out.write_all(&padding[..count])
                .map_err(|e| format!("write gap: {}", e))?;
            cursor += count;
        }
        let source_end = (source_offset + filesz) as usize;
        out.write_all(&elf[source_offset as usize..source_end])
            .map_err(|e| format!("write segment: {}", e))?;
        cursor += filesz as usize;
    }
    if cursor != layout.image_size as usize {
        return Err(format!(
            "compact image size mismatch: wrote {} expected {}",
            cursor, layout.image_size
        ));
    }
    Ok(layout)
}

// Same-session volatile cache of the installed image's metadata only (36 bytes,
// Copy). The ELF buffer is NOT cached: the heap is ~264KB and loader_app is
// ~43KB, so caching it plus the per-run clone OOMs on a repeated `image run`.
// run() re-reads the ELF off external flash each call (cheap relative to the
// flash erase/write install did) and drops it after executing. Cleared by a
// board reset (RAM gone). shell runs single-threaded for its whole lifetime, so
// a plain static needs no lock.
static mut CURRENT: Option<ImageMetadata> = None;

// Cache the just-loaded/recovered meta so subsequent runs reuse it without
// re-parsing /data/image.meta. Called only after the install/recovery is valid.
fn set_current(meta: ImageMetadata) {
    unsafe { CURRENT = Some(meta) };
}

// Return a copy of the cached meta if this session already loaded or recovered
// an image. The ELF buf is re-read each run, not cached.
fn current_loaded() -> Option<ImageMetadata> {
    unsafe { CURRENT }
}

// Load: read an ELF, lay its PT_LOAD segments into the Loadable Region, install
// it, fully read back + CRC-verify, commit metadata to /data/image.meta, copy RW
// segments to SRAM, and cache e_entry for a later run(). The ELF must be linked
// so its in-window PT_LOAD segments fall in [XIP_VADDR_BASE, XIP_VADDR_END).
// Leaves the device in Idle (installed, not mapped). Does NOT map or call --
// run() does that, so a loaded image can be run repeatedly without re-reading or
// re-installing, and after a reset the persisted metadata lets run() recover
// without re-loading.
fn load(path: &str) -> Result<(), String> {
    let elf_path_len = validate_meta_path(path.as_bytes())?;
    let mut file = File::open(path).map_err(|e| format!("open '{}': {}", path, e))?;
    let header = elf_stream::read_header(&mut file)?;
    let elf_crc32 = elf_stream::crc32(&mut file, header.file_size)?;
    let layout = derive_xip_layout_stream(&mut file, &header)?;

    let temp = File::create(XIP_LOAD_TMP).map_err(|e| format!("create {}: {}", XIP_LOAD_TMP, e))?;
    let mut writer = CrcWriter::new(temp);
    build_xip_image_stream(&mut file, &header, &layout, &mut writer)?;
    let (temp, image_crc32) = writer.finalize();
    drop(temp);
    println!(
        "xip image: {} bytes, erase={} bytes, region_off={:#x}, mapped_base={:#010x}, entry={:#010x}, crc={:#010x}",
        layout.image_size,
        layout.erase_size,
        layout.region_offset,
        layout.mapped_vaddr_base,
        layout.entry,
        image_crc32
    );

    install_inner(
        XIP_LOAD_TMP,
        layout.region_offset,
        layout.erase_size,
        Some(image_crc32),
    )?;
    let _ = std::fs::remove_file(META_PATH);
    println!("verified full image: crc={:#010x}", image_crc32);

    let meta = ImageMetadata {
        magic: META_MAGIC,
        version: META_VERSION,
        region_offset: layout.region_offset,
        image_size: layout.image_size,
        image_crc32,
        elf_crc32,
        entry: layout.entry,
        elf_path_len,
        metadata_crc32: 0,
    };
    let bytes = serialize_meta(&meta, path.as_bytes())?;
    let meta = ImageMetadata {
        metadata_crc32: u32_from_le(&bytes[32..36]),
        ..meta
    };
    write_meta(&meta, path)?;

    let safe_base = query_dram_safe()?;
    copy_rw_segments_stream(&mut file, &header, safe_base)?;
    set_current(meta);
    println!("loaded: entry={:#010x}", layout.entry);
    Ok(())
}

fn read_elf_for_meta(meta: &ImageMetadata) -> Result<(File, ElfHeader), String> {
    let (readback, elf_path_bytes) = read_meta()?;
    if readback.metadata_crc32 != meta.metadata_crc32 {
        return Err("metadata changed since load: crc mismatch".to_string());
    }
    let elf_path = core::str::from_utf8(&elf_path_bytes)
        .map_err(|_| "metadata CRC mismatch: bad elf_path utf8".to_string())?;
    let mut file = File::open(elf_path).map_err(|e| format!("open '{}': {}", elf_path, e))?;
    let header = elf_stream::read_header(&mut file)?;
    validate_elf_for_meta_stream(meta, &mut file, &header)?;
    Ok((file, header))
}

fn run() -> Result<(), String> {
    if let Some(meta) = current_loaded() {
        let (mut elf, header) = read_elf_for_meta(&meta)?;
        return execute_loaded_image(&meta, &mut elf, &header);
    }
    let (meta, mut elf, header) = recover_loaded_image()?;
    set_current(meta);
    execute_loaded_image(&meta, &mut elf, &header)
}

// Restore RW segments, map the installed image, fence, call entry, unmap.
// Shared by both run() paths. map_exec(region_offset, image_size) needs only
// Idle, which is what makes the post-reset path work without a re-load: the
// kernel treats flash as a stateless mapping resource.
fn execute_loaded_image(
    meta: &ImageMetadata,
    elf: &mut File,
    header: &ElfHeader,
) -> Result<(), String> {
    // Restore out-of-window RW segments (.data/.rodata/.got in SRAM) from the
    // ELF so each run starts from a clean .data and zeroed .bss.
    let safe_base = query_dram_safe()?;
    copy_rw_segments_stream(elf, header, safe_base)?;

    // map_exec needs Idle; if it fails the device is likely already Mapped.
    let addr = map_exec(meta.region_offset, meta.image_size)
        .map_err(|e| format!("{} (try `image unmap`)", e))?;
    let res = (|| {
        // Kernel converts region-relative offset once to physical flash, then
        // the MMU maps that physical offset into the IROM window.
        let expected = IROM_VADDR_BASE + LOADABLE_REGION_BASE + meta.region_offset;
        if addr != expected {
            return Err(format!(
                "mapped base {:#010x} != expected {:#010x}",
                addr, expected
            ));
        }
        // fence.i: SRAM is uncached on C3 (ICache-only, no DCache), so this is
        // belt-and-suspenders rather than a required coherency barrier.
        unsafe { asm!("fence.i", options(nostack, preserves_flags)) };
        let entry: unsafe extern "C" fn() -> u32 =
            unsafe { core::mem::transmute::<usize, _>(meta.entry as usize) };
        let got = unsafe { entry() };
        println!("entry returned: {:#010x}", got);
        Ok(())
    })();
    let cleanup = unmap();
    match (res, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(cleanup_error)) => Err(cleanup_error),
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(cleanup_error)) => Err(format!(
            "{}; additionally unmap failed: {}",
            primary, cleanup_error
        )),
    }
}

// Stash the build-time-embedded ELF to /data on external flash so the bytes
// are reachable at runtime. Upload-only: write then read back and CRC-verify.
fn stash() -> Result<(), String> {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(PAYLOAD_DST)
        .map_err(|e| format!("create {}: {}", PAYLOAD_DST, e))?;
    f.write_all(PAYLOAD_ELF)
        .map_err(|e| format!("write {}: {}", PAYLOAD_DST, e))?;
    // Must close first: triggers BlueOS FATFS close -> fsync -> flush so dirty
    // clusters + dir entry land on the block device. Reading back through the
    // same handle would hit cached dirty data and hide a write-path defect.
    drop(f);

    // Reopen and verify through the real block-device read path.
    let mut verify_file =
        File::open(PAYLOAD_DST).map_err(|e| format!("reopen {}: {}", PAYLOAD_DST, e))?;
    let written = crc32_of_file(&mut verify_file, PAYLOAD_ELF.len())?;
    drop(verify_file);
    let embedded = !crc32_update(0xFFFF_FFFF, PAYLOAD_ELF);
    if written != embedded {
        return Err(format!(
            "stash verify failed: wrote crc={:#010x} embedded crc={:#010x}",
            written, embedded
        ));
    }
    println!(
        "stashed {} bytes to {} (crc={:#010x})",
        PAYLOAD_ELF.len(),
        PAYLOAD_DST,
        written
    );
    Ok(())
}

pub fn command(args: &[&str]) -> Result<(), String> {
    if args.is_empty() {
        return Err(
            "Usage: image <install|verify|clear|map|unmap|stash|load|run> [path]".to_string(),
        );
    }
    match args[0] {
        "install" => {
            if args.len() < 2 {
                return Err("Usage: image install <path>".to_string());
            }
            install(args[1])
        }
        "verify" => {
            if args.len() < 2 {
                return Err("Usage: image verify <path>".to_string());
            }
            verify(args[1])
        }
        "clear" => clear(),
        "map" => map_from_meta().map(|_| ()),
        "unmap" => unmap(),
        "stash" => stash(),
        "load" => {
            if args.len() < 2 {
                return Err("Usage: image load <elf-path>".to_string());
            }
            load(args[1])
        }
        "run" => run(),
        other => Err(format!("unknown subcommand '{}'", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_app_uses_compact_xip_layout() {
        let layout = derive_xip_layout(PAYLOAD_ELF).unwrap();

        assert_eq!(layout.mapped_vaddr_base, 0x4220_0000);
        assert_eq!(layout.physical_offset, 0x0020_0000);
        assert_eq!(layout.region_offset, 0x0000_0000);
        assert_eq!(layout.image_size, 0x1CFC);
        assert_eq!(layout.erase_size, 0x2000);
        assert_eq!(layout.entry, 0x4220_01C8);
    }

    #[test]
    fn loadable_region_reserves_one_mib() {
        assert_eq!(LOADABLE_REGION_BASE, 0x0020_0000);
        assert_eq!(LOADABLE_REGION_SIZE, 0x0010_0000);
        assert_eq!(LOADABLE_REGION_END, 0x0030_0000);
        assert_eq!(XIP_VADDR_BASE, 0x4220_0000);
        assert_eq!(XIP_VADDR_END, 0x4230_0000);
    }
}
