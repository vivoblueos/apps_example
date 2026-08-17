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
fn stream_segment_end(header: &ElfHeader, offset: u32, size: u32) -> Result<u32, String> {
    let end = offset
        .checked_add(size)
        .ok_or_else(|| "segment source overflow".to_string())?;
    if end as usize > header.file_size {
        return Err("segment file bytes past end of ELF".to_string());
    }
    Ok(end)
}

fn derive_xip_layout_stream<R: Read + Seek>(
    elf: &mut R,
    header: &ElfHeader,
) -> Result<XipLayout, String> {
    let mut min_vaddr = u32::MAX;
    let mut span_end = 0u32;
    let mut entry_is_executable = false;
    for index in 0..header.program_count {
        let program = elf_stream::read_program_header(elf, header, index)?;
        if program.kind != PT_LOAD || program.file_size == 0 {
            continue;
        }
        stream_segment_end(header, program.offset, program.file_size)?;
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
    Ok(XipLayout {
        mapped_vaddr_base,
        physical_offset,
        region_offset,
        image_size,
        erase_size,
        entry: header.entry,
    })
}

fn build_xip_image_stream<R: Read + Seek, W: Write>(
    elf: &mut R,
    header: &ElfHeader,
    layout: &XipLayout,
    output: &mut W,
) -> Result<(), String> {
    let padding = [0xFFu8; IO_CHUNK];
    let mut cursor = 0usize;
    let mut previous: Option<(u32, usize)> = None;
    loop {
        let mut selected = None;
        for index in 0..header.program_count {
            let program = elf_stream::read_program_header(elf, header, index)?;
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
        elf_stream::copy_range(elf, program.offset, program.file_size, output)?;
        cursor += program.file_size as usize;
        previous = Some(key);
    }
    if cursor != layout.image_size as usize {
        return Err(format!(
            "compact image size mismatch: wrote {} expected {}",
            cursor, layout.image_size
        ));
    }
    Ok(())
}

fn validate_elf_for_meta_stream<R: Read + Seek>(
    meta: &ImageMetadata,
    elf: &mut R,
    header: &ElfHeader,
) -> Result<XipLayout, String> {
    let actual_crc = elf_stream::crc32(elf, header.file_size)?;
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
        let program = elf_stream::read_program_header(elf, header, index)?;
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
        let program = elf_stream::read_program_header(elf, header, index)?;
        if program.kind != PT_LOAD || program.memory_size == 0 {
            continue;
        }
        if program.file_size > program.memory_size {
            return Err(format!(
                "PT_LOAD filesz {} exceeds memsz {} at vaddr {:#010x}",
                program.file_size, program.memory_size, program.vaddr
            ));
        }
        if (XIP_VADDR_BASE..XIP_VADDR_END).contains(&program.vaddr) {
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
