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
use core::sync::atomic::{AtomicU32, Ordering};
use libc::{c_int, c_ulong, timespec};
use librs::time::{clock_gettime, CLOCK_MONOTONIC};

// Match fatfs cluster size (vfs BLOCK_SIZE = 4096): sub-cluster writes force
// fatfs to read-modify-write the whole 4K cluster.
const IO_CHUNK: usize = 4096;

static COPY_READ_MS: AtomicU32 = AtomicU32::new(0);
static COPY_WRITE_MS: AtomicU32 = AtomicU32::new(0);

fn now_ms() -> u32 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts as *mut timespec) };
    (ts.tv_sec as u32) * 1000 + (ts.tv_nsec as u32) / 1_000_000
}
const ELF_HEADER_SIZE: usize = 52;
const PROGRAM_HEADER_SIZE: usize = 32;
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

#[derive(Clone, Copy, Debug)]
struct ElfHeader {
    entry: u32,
    program_offset: usize,
    program_entry_size: usize,
    program_count: usize,
    file_size: usize,
}

#[derive(Clone, Copy, Debug)]
struct ProgramHeader {
    kind: u32,
    offset: u32,
    vaddr: u32,
    file_size: u32,
    memory_size: u32,
    flags: u32,
}

fn u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_exact_at<R: Read + Seek>(
    reader: &mut R,
    offset: usize,
    output: &mut [u8],
) -> Result<(), String> {
    reader
        .seek(SeekFrom::Start(offset as u64))
        .map_err(|error| format!("seek to {}: {}", offset, error))?;
    reader
        .read_exact(output)
        .map_err(|error| format!("read at {}: {}", offset, error))?;
    Ok(())
}

fn read_header<R: Read + Seek>(reader: &mut R) -> Result<ElfHeader, String> {
    let file_size = usize::try_from(
        reader
            .seek(SeekFrom::End(0))
            .map_err(|error| format!("seek ELF end: {}", error))?,
    )
    .map_err(|_| "ELF size does not fit usize".to_string())?;
    if file_size < ELF_HEADER_SIZE {
        return Err("ELF header is truncated".to_string());
    }

    let mut bytes = [0u8; ELF_HEADER_SIZE];
    read_exact_at(reader, 0, &mut bytes)?;
    if bytes[..4] != ELF_MAGIC {
        return Err("bad ELF magic".to_string());
    }
    if bytes[4] != 1 {
        return Err("not ELF32".to_string());
    }
    let program_offset = u32_le(&bytes[28..32]) as usize;
    let program_entry_size = u16_le(&bytes[42..44]) as usize;
    let program_count = u16_le(&bytes[44..46]) as usize;
    if program_entry_size < PROGRAM_HEADER_SIZE || program_count == 0 {
        return Err(format!(
            "bad phdr table: entsize={} num={}",
            program_entry_size, program_count
        ));
    }
    let table_size = program_entry_size
        .checked_mul(program_count)
        .ok_or_else(|| "phdr table overflow".to_string())?;
    let table_end = program_offset
        .checked_add(table_size)
        .ok_or_else(|| "phdr table overflow".to_string())?;
    if table_end > file_size {
        return Err("phdr table exceeds ELF".to_string());
    }
    Ok(ElfHeader {
        entry: u32_le(&bytes[24..28]),
        program_offset,
        program_entry_size,
        program_count,
        file_size,
    })
}

fn read_program_header<R: Read + Seek>(
    reader: &mut R,
    header: &ElfHeader,
    index: usize,
) -> Result<ProgramHeader, String> {
    if index >= header.program_count {
        return Err(format!("program header {} out of range", index));
    }
    let offset = header.program_offset + index * header.program_entry_size;
    let mut bytes = [0u8; PROGRAM_HEADER_SIZE];
    read_exact_at(reader, offset, &mut bytes)?;
    Ok(ProgramHeader {
        kind: u32_le(&bytes[0..4]),
        offset: u32_le(&bytes[4..8]),
        vaddr: u32_le(&bytes[8..12]),
        file_size: u32_le(&bytes[16..20]),
        memory_size: u32_le(&bytes[20..24]),
        flags: u32_le(&bytes[24..28]),
    })
}

fn crc32<R: Read + Seek>(reader: &mut R, size: usize) -> Result<u32, String> {
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek ELF: {}", error))?;
    let mut buffer = [0u8; IO_CHUNK];
    let mut crc = 0xFFFF_FFFF;
    let mut done = 0usize;
    while done < size {
        let wanted = core::cmp::min(IO_CHUNK, size - done);
        reader
            .read_exact(&mut buffer[..wanted])
            .map_err(|error| format!("read ELF at {}: {}", done, error))?;
        crc = crc32_update(crc, &buffer[..wanted]);
        done += wanted;
    }
    Ok(!crc)
}

fn copy_range<R: Read + Seek, W: Write>(
    reader: &mut R,
    offset: u32,
    size: u32,
    output: &mut W,
) -> Result<(), String> {
    reader
        .seek(SeekFrom::Start(u64::from(offset)))
        .map_err(|error| format!("seek segment: {}", error))?;
    let mut buffer = [0u8; IO_CHUNK];
    let total = size as usize;
    let mut remaining = total;
    let mut chunks = 0usize;
    while remaining > 0 {
        let wanted = core::cmp::min(IO_CHUNK, remaining);
        let t_r = now_ms();
        reader
            .read_exact(&mut buffer[..wanted])
            .map_err(|error| {
                format!("read segment at {}/{}: {}", total - remaining, total, error)
            })?;
        let t_w = now_ms();
        COPY_READ_MS.fetch_add(t_w - t_r, Ordering::Relaxed);
        output
            .write_all(&mut buffer[..wanted])
            .map_err(|error| {
                format!("write segment at {}/{}: {}", total - remaining, total, error)
            })?;
        let t_e = now_ms();
        COPY_WRITE_MS.fetch_add(t_e - t_w, Ordering::Relaxed);
        remaining -= wanted;
        chunks += 1;
        if chunks % 64 == 0 {
            println!("copy_range: {}/{} bytes", total - remaining, total);
        }
    }
    Ok(())
}

fn read_bounded_tail<R: Read>(
    reader: &mut R,
    length: usize,
    maximum: usize,
) -> Result<Vec<u8>, String> {
    if length > maximum {
        return Err(format!("length {} exceeds limit {}", length, maximum));
    }
    let mut bytes = vec![0u8; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| format!("bounded read: {}", error))?;
    let mut trailing = [0u8; 1];
    if reader
        .read(&mut trailing)
        .map_err(|error| format!("trailing read: {}", error))?
        != 0
    {
        return Err("unexpected trailing data".to_string());
    }
    Ok(bytes)
}

fn stream_segment_end(header: &ElfHeader, offset: u32, size: u32) -> Result<u32, String> {
    let end = offset
        .checked_add(size)
        .ok_or_else(|| "segment source overflow".to_string())?;
    if end as usize > header.file_size {
        return Err("segment file bytes past end of ELF".to_string());
    }
    Ok(end)
}

// MMU entry id for a vaddr in the shared C3 table: (vaddr & 0x7F_FFFF) >> 16.
// IROM (0x4200_0000~) and DROM (0x3C00_0000~) vaddrs at the same offset share
// one entry, so a .text entry and .rodata entry colliding is a hard error.
fn mmu_entry_id(vaddr: u32) -> u32 {
    (vaddr & 0x007F_FFFF) >> 16
}

// Compute the DROM layout for a .rodata PT_LOAD whose vaddr is in the DROM
// window. `text_region_end` is the exclusive end of the already-placed .text
// region in Loadable-Region-relative bytes (i.e. .text erase boundary). The
// .rodata physical offset must be >= text_region_end, 64K-aligned to match the
// DROM vaddr's low bits (hardware MMU constraint), and stay in the region.
fn derive_drom(
    drom_vaddr: u32,
    filesz: u32,
    program_offset: u32,
    text_region_end: u32,
    text_entry_lo: u32,
    text_entry_hi: u32,
) -> Result<DromLayout, String> {
    let drom_end = drom_vaddr
        .checked_add(filesz)
        .ok_or_else(|| "DROM vaddr+filesz overflow".to_string())?;
    if drom_vaddr < DROM_VADDR_BASE || drom_end > DROM_VADDR_END {
        return Err("DROM segment exceeds DROM window".to_string());
    }
    // Loader-only entry-conflict check: shared MMU table means .rodata entries
    // must not overlap .text entries.
    let (ro_lo, ro_hi) = (mmu_entry_id(drom_vaddr), mmu_entry_id(drom_end - 1));
    if !(ro_hi < text_entry_lo || ro_lo > text_entry_hi) {
        return Err(format!(
            "DROM entries {:#x}..{:#x} collide with XIP entries {:#x}..{:#x}",
            ro_lo, ro_hi, text_entry_lo, text_entry_hi
        ));
    }

    // 64K alignment: physical offset low bits must equal drom_vaddr low bits.
    // Work in absolute physical space so the residue math is comparable, then
    // convert back to a Loadable-Region-relative offset for the ioctl.
    let align = FLASH_MMU_PAGE_SIZE;
    let vaddr_residue = drom_vaddr % align;
    let text_phys_end = LOADABLE_REGION_BASE
        .checked_add(text_region_end)
        .ok_or_else(|| "text physical end overflow".to_string())?;
    // First >= text_phys_end with the matching residue.
    let cur_residue = text_phys_end % align;
    let delta = (vaddr_residue + align - cur_residue) % align;
    let phys = text_phys_end
        .checked_add(delta)
        .ok_or_else(|| "DROM physical offset overflow".to_string())?;
    let region_offset = phys
        .checked_sub(LOADABLE_REGION_BASE)
        .ok_or_else(|| "DROM physical below loadable region".to_string())?;
    let page_base = drom_vaddr & !(align - 1);
    let mapped_size = align_up_to(filesz + (drom_vaddr - page_base), align)?;
    let region_end = region_offset
        .checked_add(mapped_size)
        .ok_or_else(|| "DROM region overflow".to_string())?;
    if region_end > LOADABLE_REGION_SIZE {
        return Err("DROM image exceeds loadable region".to_string());
    }
    Ok(DromLayout {
        drom_vaddr,
        program_offset,
        filesz,
        physical_offset: phys,
        region_offset,
        image_size: mapped_size,
    })
}

fn align_up_to(value: u32, align: u32) -> Result<u32, String> {
    let mask = align - 1;
    value
        .checked_add(mask)
        .map(|v| v & !mask)
        .ok_or_else(|| "align_up overflow".to_string())
}

fn derive_xip_layout_stream<R: Read + Seek>(
    elf: &mut R,
    header: &ElfHeader,
) -> Result<XipLayout, String> {
    let mut min_vaddr = u32::MAX;
    let mut span_end = 0u32;
    let mut entry_is_executable = false;
    let mut drom_seg: Option<(u32, u32, u32)> = None; // (vaddr, filesz, program_offset)
    for index in 0..header.program_count {
        let program = read_program_header(elf, header, index)?;
        if program.kind != PT_LOAD || program.file_size == 0 {
            continue;
        }
        stream_segment_end(header, program.offset, program.file_size)?;
        if (DROM_VADDR_BASE..DROM_VADDR_END).contains(&program.vaddr) {
            if drom_seg.is_some() {
                return Err("multiple DROM-window PT_LOAD segments".to_string());
            }
            drom_seg = Some((program.vaddr, program.file_size, program.offset));
            continue;
        }
        if !(XIP_VADDR_BASE..XIP_VADDR_END).contains(&program.vaddr) {
            continue;
        }
        let segment_end = program
            .vaddr
            .checked_add(program.file_size)
            .ok_or_else(|| "vaddr+filesz overflow".to_string())?;
        if segment_end > XIP_VADDR_END {
            return Err("XIP segment exceeds IROM window".to_string());
        }
        min_vaddr = core::cmp::min(min_vaddr, program.vaddr);
        span_end = core::cmp::max(span_end, segment_end);
        if program.flags & PF_X != 0 && (program.vaddr..segment_end).contains(&header.entry) {
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

    // .text MMU entries (for DROM conflict check) span the mapped page range.
    let text_entry_lo = mmu_entry_id(mapped_vaddr_base);
    let text_entry_hi = mmu_entry_id(mapped_vaddr_base + image_size - 1);
    // DROM physical must start at/after .text erase boundary (region-relative).
    let text_region_end = region_offset
        .checked_add(erase_size)
        .ok_or_else(|| "text region end overflow".to_string())?;
    let drom = match drom_seg {
        Some((vaddr, filesz, prog_off)) => Some(derive_drom(
            vaddr,
            filesz,
            prog_off,
            text_region_end,
            text_entry_lo,
            text_entry_hi,
        )?),
        None => None,
    };

    Ok(XipLayout {
        mapped_vaddr_base,
        physical_offset,
        region_offset,
        image_size,
        erase_size,
        entry: header.entry,
        drom,
    })
}

fn build_xip_image_stream<R: Read + Seek, W: Write>(
    elf: &mut R,
    header: &ElfHeader,
    layout: &XipLayout,
    output: &mut W,
) -> Result<usize, String> {
    let padding = [0xFFu8; IO_CHUNK];
    let mut cursor = 0usize;
    let mut previous: Option<(u32, usize)> = None;
    loop {
        let mut selected = None;
        for index in 0..header.program_count {
            let program = read_program_header(elf, header, index)?;
            if program.kind != PT_LOAD
                || program.file_size == 0
                || !(layout.mapped_vaddr_base..layout.mapped_vaddr_base + layout.image_size)
                    .contains(&program.vaddr)
            {
                continue;
            }
            stream_segment_end(header, program.offset, program.file_size)?;
            let key = (program.vaddr, index);
            if previous.map_or(false, |last| key <= last) {
                continue;
            }
            if selected.map_or(true, |(best, _)| key < best) {
                selected = Some((key, program));
            }
        }
        let Some((key, program)) = selected else {
            break;
        };
        let destination = (program.vaddr - layout.mapped_vaddr_base) as usize;
        if destination < cursor {
            return Err("overlapping XIP PT_LOAD segments".to_string());
        }
        while cursor < destination {
            let count = core::cmp::min(IO_CHUNK, destination - cursor);
            output
                .write_all(&padding[..count])
                .map_err(|error| format!("write gap: {}", error))?;
            cursor += count;
        }
        println!(
            "build_xip: seg vaddr={:#010x} off={:#x} filesz={} dest={}",
            program.vaddr, program.offset, program.file_size, destination
        );
        copy_range(elf, program.offset, program.file_size, output)?;
        println!("build_xip: seg done cursor={}", cursor + program.file_size as usize);
        cursor += program.file_size as usize;
        previous = Some(key);
    }
    if cursor != layout.image_size as usize {
        return Err(format!(
            "compact image size mismatch: wrote {} expected {}",
            cursor, layout.image_size
        ));
    }
    // Append the DROM (.rodata) segment after .text. It lives at a distinct
    // 64K-aligned physical offset (see derive_drom); the gap between .text end
    // and that offset is erased flash padding, filled with 0xFF here. Temp byte
    // 0 maps to flash layout.physical_offset, so the DROM dest is its physical
    // offset minus the .text base — NOT minus LOADABLE_REGION_BASE.
    if let Some(drom) = layout.drom {
        let drom_dest = (drom.physical_offset - layout.physical_offset) as usize;
        if drom_dest < cursor {
            return Err("DROM physical offset below .text cursor".to_string());
        }
        while cursor < drom_dest {
            let count = core::cmp::min(IO_CHUNK, drom_dest - cursor);
            output
                .write_all(&padding[..count])
                .map_err(|error| format!("write DROM gap: {}", error))?;
            cursor += count;
        }
        println!(
            "build_xip: drom seg vaddr={:#010x} off={:#x} filesz={} dest={}",
            drom.drom_vaddr, drom.program_offset, drom.filesz, drom_dest
        );
        copy_range(elf, drom.program_offset, drom.filesz, output)?;
        cursor += drom.filesz as usize;
        println!("build_xip: drom seg done cursor={}", cursor);
    }
    Ok(cursor)
}

fn validate_elf_for_meta_stream<R: Read + Seek>(
    meta: &ImageMetadata,
    elf: &mut R,
    header: &ElfHeader,
) -> Result<XipLayout, String> {
    let actual_crc = crc32(elf, header.file_size)?;
    if actual_crc != meta.elf_crc32 {
        return Err(format!(
            "ELF CRC mismatch: actual={:#010x}, expected={:#010x}",
            actual_crc, meta.elf_crc32
        ));
    }
    let layout = derive_xip_layout_stream(elf, header)?;
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
    // Cross-check DROM: meta.drom_vaddr == 0 ↔ layout.drom == None, and the
    // fields match when present. Prevents a stale meta from running a re-linked ELF.
    match (layout.drom, meta.drom_vaddr) {
        (None, 0) => {}
        (Some(d), v) if v != 0 => {
            if d.drom_vaddr != meta.drom_vaddr
                || d.region_offset != meta.drom_region_offset
                || d.image_size != meta.drom_image_size
            {
                return Err(format!(
                    "DROM mismatch: elf(vaddr={:#010x} off={:#x} sz={}) meta(vaddr={:#010x} off={:#x} sz={})",
                    d.drom_vaddr, d.region_offset, d.image_size,
                    meta.drom_vaddr, meta.drom_region_offset, meta.drom_image_size
                ));
            }
        }
        (Some(_), 0) => {
            return Err("ELF has DROM segment but meta has none".to_string());
        }
        (None, v) if v != 0 => {
            return Err("meta has DROM segment but ELF has none".to_string());
        }
        _ => {}
    }
    Ok(layout)
}

fn verify_xip_segments_stream<R: Read + Seek>(
    elf: &mut R,
    header: &ElfHeader,
    layout: &XipLayout,
) -> Result<usize, String> {
    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEV)
        .map_err(|error| format!("open {}: {}", DEV, error))?;
    let mut expected = [0u8; IO_CHUNK];
    let mut actual = [0u8; IO_CHUNK];
    let mut total = 0usize;
    let mut segment_index = 0usize;
    for index in 0..header.program_count {
        let program = read_program_header(elf, header, index)?;
        if program.kind != PT_LOAD
            || program.file_size == 0
            || !(layout.mapped_vaddr_base..layout.mapped_vaddr_base + layout.image_size)
                .contains(&program.vaddr)
        {
            continue;
        }
        stream_segment_end(header, program.offset, program.file_size)?;
        let device_offset = layout
            .region_offset
            .checked_add(program.vaddr - layout.mapped_vaddr_base)
            .ok_or_else(|| "segment device offset overflow".to_string())?;
        elf.seek(SeekFrom::Start(u64::from(program.offset)))
            .map_err(|error| format!("seek ELF segment: {}", error))?;
        dev.seek(SeekFrom::Start(u64::from(device_offset)))
            .map_err(|error| format!("seek segment {}: {}", segment_index, error))?;
        let mut compared = 0usize;
        while compared < program.file_size as usize {
            let count = core::cmp::min(IO_CHUNK, program.file_size as usize - compared);
            elf.read_exact(&mut expected[..count])
                .map_err(|error| format!("read ELF segment {}: {}", segment_index, error))?;
            dev.read_exact(&mut actual[..count])
                .map_err(|error| format!("read flash segment {}: {}", segment_index, error))?;
            if actual[..count] != expected[..count] {
                let bad = actual[..count]
                    .iter()
                    .zip(&expected[..count])
                    .position(|(actual, expected)| actual != expected)
                    .unwrap_or(0);
                return Err(format!(
                    "XIP segment {} verify failed at {:#x}: expected={:#04x}, actual={:#04x}",
                    segment_index,
                    compared + bad,
                    expected[bad],
                    actual[bad]
                ));
            }
            compared += count;
        }
        total += program.file_size as usize;
        segment_index += 1;
    }
    println!("verified {} actual XIP bytes", total);
    Ok(total)
}

fn copy_rw_segments_stream<R: Read + Seek>(
    elf: &mut R,
    header: &ElfHeader,
    safe_base: u32,
) -> Result<(), String> {
    let mut buffer = [0u8; IO_CHUNK];
    for index in 0..header.program_count {
        let program = read_program_header(elf, header, index)?;
        if program.kind != PT_LOAD || program.memory_size == 0 {
            continue;
        }
        if program.file_size > program.memory_size {
            return Err(format!(
                "PT_LOAD filesz {} exceeds memsz {} at vaddr {:#010x}",
                program.file_size, program.memory_size, program.vaddr
            ));
        }
        // XIP (I-bus code) and DROM (D-bus .rodata) segments are flash-mapped,
        // not copied to SRAM — skip both so a DROM .rodata isn't mistaken for RW.
        if (XIP_VADDR_BASE..XIP_VADDR_END).contains(&program.vaddr)
            || (DROM_VADDR_BASE..DROM_VADDR_END).contains(&program.vaddr)
        {
            continue;
        }
        let segment_end = program
            .vaddr
            .checked_add(program.memory_size)
            .ok_or_else(|| "vaddr+memsz overflow".to_string())?;
        if program.vaddr < safe_base || segment_end > DRAM_TOP {
            return Err(format!(
                "RW seg at {:#010x}..{:#010x} outside safe SRAM [{:#010x},{:#010x})",
                program.vaddr, segment_end, safe_base, DRAM_TOP
            ));
        }
        if program.file_size > 0 {
            stream_segment_end(header, program.offset, program.file_size)?;
            elf.seek(SeekFrom::Start(u64::from(program.offset)))
                .map_err(|error| format!("seek RW segment: {}", error))?;
            let mut copied = 0usize;
            while copied < program.file_size as usize {
                let count = core::cmp::min(IO_CHUNK, program.file_size as usize - copied);
                elf.read_exact(&mut buffer[..count])
                    .map_err(|error| format!("read RW segment: {}", error))?;
                unsafe {
                    ptr::copy_nonoverlapping(
                        buffer.as_ptr(),
                        (program.vaddr as usize + copied) as *mut u8,
                        count,
                    );
                }
                copied += count;
            }
        }
        let bss = program.memory_size - program.file_size;
        if bss > 0 {
            unsafe {
                ptr::write_bytes(
                    (program.vaddr + program.file_size) as *mut u8,
                    0,
                    bss as usize,
                );
            }
        }
        println!(
            "copied RW seg {:#010x}..{:#010x} (filesz={}, bss={})",
            program.vaddr, segment_end, program.file_size, bss
        );
    }
    Ok(())
}

const DEV: &str = "/dev/esp32-flash0";
const ESP32_FLASH_ERASE_RANGE: u32 = 0x40;
const ESP32_FLASH_MAP_EXEC: u32 = 0x44;
const FLASH_IOCTL_ABI_VERSION: u32 = 1;
const ESP32_FLASH_UNMAP: u32 = 0x45;
const ESP32_FLASH_QUERY_DRAM_SAFE: u32 = 0x46;
const ESP32_FLASH_MAP_DROM: u32 = 0x47;
const CHUNK: usize = 4096;
// ESP32-C3 DRAM upper bound (SRAM ends at 0x3FCE0000). Bounds the load-time copy
// of RW segments so it never scribbles past physical SRAM. This file already
// targets esp32-flash0 (riscv32-only), so a board constant here adds no new
// architecture coupling.
const DRAM_TOP: u32 = 0x3FCE_0000;


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
const LOADABLE_REGION_BASE: u32 = 0x0011_0000;
const LOADABLE_REGION_END: u32 = 0x0040_0000;
const LOADABLE_REGION_SIZE: u32 = LOADABLE_REGION_END - LOADABLE_REGION_BASE;
const XIP_VADDR_BASE: u32 = IROM_VADDR_BASE + LOADABLE_REGION_BASE;
const XIP_VADDR_END: u32 = IROM_VADDR_BASE + LOADABLE_REGION_END;
// DROM window (.rodata). Same values as the kernel board constants.
const DROM_VADDR_BASE: u32 = 0x3C00_0000;
const DROM_VADDR_END: u32 = 0x3C80_0000;
const FLASH_MMU_PAGE_SIZE: u32 = 0x0001_0000;
const FLASH_SECTOR_SIZE: u32 = 4096;
#[allow(dead_code)]
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
    let t_prog = now_ms();
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
    let t_prog_end = now_ms();
    let want = expected_crc.unwrap_or(!src_crc);
    let t_verify = now_ms();
    let actual = crc32_of_file_at(&mut dev, region_offset as u64, size)?;
    println!(
        "diag: install program={}ms verify={}ms",
        t_prog_end - t_prog,
        now_ms() - t_verify,
    );
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

// Erase only the persisted image range, then remove its metadata. Covers the
// full text+DROM span when a DROM segment was installed.
fn clear() -> Result<(), String> {
    let (meta, _) = match read_meta() {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let erase_size = if meta.drom_vaddr != 0 {
        let span = meta
            .drom_region_offset
            .checked_add(meta.drom_image_size)
            .and_then(|end| end.checked_sub(meta.region_offset))
            .ok_or_else(|| "clear span overflow".to_string())?;
        align_up_sector(span)?
    } else {
        align_up_sector(meta.image_size)?
    };
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

// Map the DROM (.rodata) segment via the kernel MAP_DROM ioctl. Must run after
// map_exec (kernel requires irom_handle set). Returns the kernel-out page-aligned
// DROM vaddr. region_offset is Loadable-Region relative; image_size is the mapped
// (page-aligned) size; drom_vaddr is the ELF .rodata vaddr (map target).
fn map_drom(region_offset: u32, image_size: u32, drom_vaddr: u32) -> Result<u32, String> {
    let dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEV)
        .map_err(|e| format!("open {}: {}", DEV, e))?;
    let mut req = MapDromRequest {
        version: FLASH_IOCTL_ABI_VERSION,
        size: core::mem::size_of::<MapDromRequest>() as u32,
        flags: 0,
        region_offset,
        image_size,
        drom_vaddr,
        mapped_address: 0,
    };
    dev_ioctl(
        &dev,
        ESP32_FLASH_MAP_DROM,
        &mut req as *mut MapDromRequest as usize,
    )?;
    println!(
        "drom mapped: vaddr={:#010x} (target={:#010x})",
        req.mapped_address, drom_vaddr
    );
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

// D-bus (DROM) map request. region_offset/image_size locate .rodata in the
// Loadable Region; drom_vaddr is the ELF's .rodata vaddr (target DROM window
// addr). mapped_address is the kernel out-pointer (page-aligned DROM vaddr).
#[repr(C)]
struct MapDromRequest {
    version: u32,
    size: u32,
    flags: u32,
    region_offset: u32,
    image_size: u32,
    drom_vaddr: u32,
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
// recover_loaded_image(). On-disk layout = fixed header (9 u32 = 36 bytes, LE)
// followed by an optional DROM block (3 u32 = 12 bytes) followed by elf_path_len
// raw bytes. META_VERSION stays 2: the DROM block is distinguished by file size
// (remaining == path_len → old v2 with no DROM; remaining == 12 + path_len → new
// v2 with DROM). metadata_crc32 covers the 32 bytes preceding it plus everything
// after the header (DROM block, if present, and elf_path), and is the torn-write
// sentinel. drom_vaddr == 0 means no DROM segment (loader_app / old v2 meta).
const META_MAGIC: u32 = 0x4D45_5441; // "META"
const META_VERSION: u32 = 2;
const META_HEADER_SIZE: usize = 36;
const META_DROM_SIZE: usize = 12;
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
    drom_vaddr: u32,
    drom_region_offset: u32,
    drom_image_size: u32,
    metadata_crc32: u32,
}

// CRC covers the fixed header excluding its CRC field plus every byte after the
// header (the DROM block, if present, and elf_path). Identical for old v2 (no
// DROM block → tail is just elf_path) and new v2 (tail is DROM block + elf_path).
fn metadata_crc(header: &[u8; 32], tail: &[u8]) -> u32 {
    let crc = crc32_update(0xFFFF_FFFF, header);
    !crc32_update(crc, tail)
}

// Serialize header (36B LE) + DROM block (12B LE) + elf_path bytes. drom_vaddr
// == 0 still writes the 12-byte zero block so new v2 always carries the block.
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
    let mut hdr = [0u8; 32];
    hdr[0..4].copy_from_slice(&meta.magic.to_le_bytes());
    hdr[4..8].copy_from_slice(&meta.version.to_le_bytes());
    hdr[8..12].copy_from_slice(&meta.region_offset.to_le_bytes());
    hdr[12..16].copy_from_slice(&meta.image_size.to_le_bytes());
    hdr[16..20].copy_from_slice(&meta.image_crc32.to_le_bytes());
    hdr[20..24].copy_from_slice(&meta.elf_crc32.to_le_bytes());
    hdr[24..28].copy_from_slice(&meta.entry.to_le_bytes());
    hdr[28..32].copy_from_slice(&meta.elf_path_len.to_le_bytes());
    let mut drom = [0u8; META_DROM_SIZE];
    drom[0..4].copy_from_slice(&meta.drom_vaddr.to_le_bytes());
    drom[4..8].copy_from_slice(&meta.drom_region_offset.to_le_bytes());
    drom[8..12].copy_from_slice(&meta.drom_image_size.to_le_bytes());
    let mut tail = Vec::with_capacity(META_DROM_SIZE + elf_path.len());
    tail.extend_from_slice(&drom);
    tail.extend_from_slice(elf_path);
    let crc = metadata_crc(&hdr, &tail);
    let mut buf = Vec::with_capacity(META_HEADER_SIZE + tail.len());
    buf.extend_from_slice(&hdr);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&tail);
    Ok(buf)
}

// Read and fully validate META_PATH: magic, version, elf_path_len consistency,
// DROM-block presence (by file size), and metadata_crc32. Returns the metadata
// and its trailing elf_path bytes.
fn read_meta() -> Result<(ImageMetadata, Vec<u8>), String> {
    let mut file = File::open(META_PATH).map_err(|e| format!("open {}: {}", META_PATH, e))?;
    let mut fixed = [0u8; META_HEADER_SIZE];
    file.read_exact(&mut fixed)
        .map_err(|error| format!("metadata CRC mismatch: truncated: {}", error))?;

    let mut header = [0u8; 32];
    header.copy_from_slice(&fixed[..32]);
    let stored_crc = u32_from_le(&fixed[32..36]);
    let magic = u32_from_le(&header[0..4]);
    let version = u32_from_le(&header[4..8]);
    let region_offset = u32_from_le(&header[8..12]);
    let image_size = u32_from_le(&header[12..16]);
    let image_crc32 = u32_from_le(&header[16..20]);
    let elf_crc32 = u32_from_le(&header[20..24]);
    let entry = u32_from_le(&header[24..28]);
    let elf_path_len = u32_from_le(&header[28..32]);
    if magic != META_MAGIC {
        return Err("metadata CRC mismatch: bad magic".to_string());
    }
    if version != META_VERSION {
        return Err(format!("metadata version {} unsupported", version));
    }
    let path_len = usize::try_from(elf_path_len)
        .map_err(|_| "metadata CRC mismatch: path length overflow".to_string())?;
    if path_len == 0 || path_len > MAX_META_PATH_LEN {
        return Err("metadata CRC mismatch: bad path length".to_string());
    }

    // Read everything after the 36B header. New v2 = DROM block (12B) + path;
    // old v2 = path only. CRC covers header[0..32] + this whole tail either way.
    let total = file
        .metadata()
        .map_err(|e| format!("metadata CRC mismatch: stat: {}", e))?
        .len();
    let tail_len = usize::try_from(total.saturating_sub(META_HEADER_SIZE as u64))
        .map_err(|_| "metadata CRC mismatch: size overflow".to_string())?;
    let mut tail = vec![0u8; tail_len];
    file.read_exact(&mut tail)
        .map_err(|error| format!("metadata CRC mismatch: truncated tail: {}", error))?;
    if metadata_crc(&header, &tail) != stored_crc {
        return Err("metadata CRC mismatch".to_string());
    }

    // Split tail into optional DROM block + path. Old v2 (tail == path) → no DROM.
    let (drom_vaddr, drom_region_offset, drom_image_size, elf_path) = if tail_len
        == path_len
    {
        (0, 0, 0, tail)
    } else if tail_len == META_DROM_SIZE + path_len {
        let drom_vaddr = u32_from_le(&tail[0..4]);
        let drom_region_offset = u32_from_le(&tail[4..8]);
        let drom_image_size = u32_from_le(&tail[8..12]);
        let elf_path = tail[META_DROM_SIZE..].to_vec();
        (drom_vaddr, drom_region_offset, drom_image_size, elf_path)
    } else {
        return Err("metadata CRC mismatch: unexpected tail size".to_string());
    };

    let meta = ImageMetadata {
        magic,
        version,
        region_offset,
        image_size,
        image_crc32,
        elf_crc32,
        entry,
        elf_path_len,
        drom_vaddr,
        drom_region_offset,
        drom_image_size,
        metadata_crc32: stored_crc,
    };
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

// D-bus (DROM) layout for a .rodata PT_LOAD whose vaddr is in the DROM window.
// `program_offset`/`filesz` locate the bytes in the ELF; the rest locate them
// in the installed image / Loadable Region for the MAP_DROM ioctl.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct DromLayout {
    drom_vaddr: u32, // ELF .rodata vaddr (0x3c120000), also the map target
    program_offset: u32, // source offset in the ELF
    filesz: u32, // raw .rodata bytes
    physical_offset: u32, // absolute flash offset (LOADABLE_REGION_BASE + region_offset)
    region_offset: u32, // Loadable-Region relative, passed to MAP_DROM ioctl
    image_size: u32, // page-aligned mapped size (>= filesz)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct XipLayout {
    mapped_vaddr_base: u32,
    physical_offset: u32,
    region_offset: u32,
    image_size: u32,
    erase_size: u32,
    entry: u32,
    drom: Option<DromLayout>, // None = no DROM-window .rodata (loader_app)
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
    let mut drom_seg: Option<(u32, u32, u32)> = None; // (vaddr, filesz, program_offset)
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
        let prog_off = u32_from_le(&ph[4..8]);
        if (DROM_VADDR_BASE..DROM_VADDR_END).contains(&vaddr) {
            if drom_seg.is_some() {
                return Err("multiple DROM-window PT_LOAD segments".to_string());
            }
            drom_seg = Some((vaddr, filesz, prog_off));
            continue;
        }
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

    let text_entry_lo = mmu_entry_id(mapped_vaddr_base);
    let text_entry_hi = mmu_entry_id(mapped_vaddr_base + image_size - 1);
    let text_region_end = region_offset
        .checked_add(erase_size)
        .ok_or_else(|| "text region end overflow".to_string())?;
    let drom = match drom_seg {
        Some((vaddr, filesz, prog_off)) => Some(derive_drom(
            vaddr,
            filesz,
            prog_off,
            text_region_end,
            text_entry_lo,
            text_entry_hi,
        )?),
        None => None,
    };

    Ok(XipLayout {
        mapped_vaddr_base,
        physical_offset,
        region_offset,
        image_size,
        erase_size,
        entry,
        drom,
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
    let header = read_header(&mut file)?;
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

pub fn current_elf_path() -> Option<String> {
    match read_meta() {
        Ok((_, bytes)) => String::from_utf8(bytes).ok(),
        Err(_) => None,
    }
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
    let header = read_header(&mut file)?;
    println!("load: opened file_size={} phnum={}", header.file_size, header.program_count);
    let elf_crc32 = crc32(&mut file, header.file_size)?;
    println!("load: elf crc done {:#010x}", elf_crc32);
    let layout = derive_xip_layout_stream(&mut file, &header)?;
    println!(
        "load: layout off={:#x} image_size={} erase={} drom={}",
        layout.region_offset, layout.image_size, layout.erase_size,
        if layout.drom.is_some() { "yes" } else { "no" }
    );

    let (drom_vaddr, drom_region_offset, drom_image_size, total_erase) = match layout.drom {
        Some(d) => {
            let total_len = d
                .region_offset
                .checked_add(d.image_size)
                .and_then(|end| end.checked_sub(layout.region_offset))
                .ok_or_else(|| "DROM total span overflow".to_string())?;
            let total = align_up_sector(total_len)?;
            (d.drom_vaddr, d.region_offset, d.image_size, total)
        }
        None => (0, 0, 0, layout.erase_size),
    };

    // Stream the XIP image straight onto on-chip flash instead of staging it
    // through /data/.xip_load.img on the external W25Q64. Writing ~850KB to the
    // external flash via fatfs is ~50s (per-cluster read-modify-write); writing
    // the same bytes to on-chip flash is ~3s. Erase first, then build_xip writes
    // sequentially from region_offset while CrcWriter folds each byte into crc.
    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEV)
        .map_err(|e| format!("open {}: {}", DEV, e))?;
    let t_erase = now_ms();
    erase_range(&dev, layout.region_offset, total_erase)?;
    dev.seek(SeekFrom::Start(layout.region_offset as u64))
        .map_err(|e| format!("seek device: {}", e))?;

    let mut writer = CrcWriter::new(dev);
    let t_build = now_ms();
    let written = build_xip_image_stream(&mut file, &header, &layout, &mut writer)?;
    let t_built = now_ms();
    println!("load: xip image built (streamed {} bytes)", written);
    let (mut dev, image_crc32) = writer.finalize();
    println!(
        "diag: erase={}ms build_xip={}ms copy_read={}ms copy_write={}ms",
        t_build - t_erase,
        t_built - t_build,
        COPY_READ_MS.load(Ordering::Relaxed),
        COPY_WRITE_MS.load(Ordering::Relaxed),
    );

    println!(
        "xip image: text={} erase={} total_erase={} region_off={:#x} mapped_base={:#010x} entry={:#010x} crc={:#010x} drom_vaddr={:#010x}",
        layout.image_size,
        layout.erase_size,
        total_erase,
        layout.region_offset,
        layout.mapped_vaddr_base,
        layout.entry,
        image_crc32,
        drom_vaddr
    );

    let t_verify = now_ms();
    let actual = crc32_of_file_at(&mut dev, layout.region_offset as u64, written)?;
    println!("diag: verify={}ms", now_ms() - t_verify);
    if actual != image_crc32 {
        return Err(format!(
            "verify failed: dev crc={:#010x} expected={:#010x}",
            actual, image_crc32
        ));
    }
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
        drom_vaddr,
        drom_region_offset,
        drom_image_size,
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
    let header = read_header(&mut file)?;
    validate_elf_for_meta_stream(meta, &mut file, &header)?;
    Ok((file, header))
}

fn run() -> Result<(), String> {
    // Opening + reading the source ELF touches the shared FAT FileSystem
    // (RefCell), which the main thread's auto_ota_loop also touches — a
    // concurrent borrow panics with BorrowMutError. So the ELF read + RW
    // restore run under the FS lock. Everything after (map + entry) is ioctl +
    // a function-pointer call, not FAT, so it stays outside the lock to avoid
    // holding it across the slent entry (which never returns).
    if let Some(meta) = current_loaded() {
        let meta = meta;
        crate::download::with_fs_lock(|| {
            let (mut elf, header) = read_elf_for_meta(&meta)?;
            let safe_base = query_dram_safe()?;
            copy_rw_segments_stream(&mut elf, &header, safe_base)
        })?;
        return call_mapped_image(&meta);
    }
    let meta = crate::download::with_fs_lock(|| {
        let (meta, mut elf, header) = recover_loaded_image()?;
        let safe_base = query_dram_safe()?;
        copy_rw_segments_stream(&mut elf, &header, safe_base)?;
        Ok::<ImageMetadata, String>(meta)
    })?;
    set_current(meta);
    let meta = current_loaded().ok_or_else(|| "current meta vanished after set_current".to_string())?;
    call_mapped_image(&meta)
}

// Map the installed image, fence, call entry, unmap. Pure ioctl + a function
// pointer call — no FAT access, so it is safe outside the FS lock. map_exec
// needs only Idle, which is what makes the post-reset path work without a
// re-load: the kernel treats flash as a stateless mapping resource.
fn call_mapped_image(meta: &ImageMetadata) -> Result<(), String> {
    let addr = map_exec(meta.region_offset, meta.image_size)
        .map_err(|e| format!("{} (try `image unmap`)", e))?;
    let drom_addr = if meta.drom_vaddr != 0 {
        match map_drom(meta.drom_region_offset, meta.drom_image_size, meta.drom_vaddr) {
            Ok(a) => Some(a),
            Err(e) => {
                let _ = unmap();
                return Err(format!("map_drom: {}", e));
            }
        }
    } else {
        None
    };
    let res = (|| {
        let expected = IROM_VADDR_BASE + LOADABLE_REGION_BASE + meta.region_offset;
        if addr != expected {
            return Err(format!(
                "mapped base {:#010x} != expected {:#010x}",
                addr, expected
            ));
        }
        if let Some(d) = drom_addr {
            if d != meta.drom_vaddr {
                return Err(format!(
                    "drom mapped {:#010x} != expected {:#010x}",
                    d, meta.drom_vaddr
                ));
            }
        }
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

pub fn command(args: &[&str]) -> Result<(), String> {
    if args.is_empty() {
        return Err(
            "Usage: image <install|verify|clear|map|unmap|load|run> [path]".to_string(),
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
    fn loadable_region_reserves_xip_window() {
        assert_eq!(LOADABLE_REGION_BASE, 0x0011_0000);
        assert_eq!(LOADABLE_REGION_SIZE, 0x002F_0000);
        assert_eq!(LOADABLE_REGION_END, 0x0040_0000);
        assert_eq!(XIP_VADDR_BASE, 0x4211_0000);
        assert_eq!(XIP_VADDR_END, 0x4240_0000);
    }
}
