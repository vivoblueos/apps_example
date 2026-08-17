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
use std::io::{Read, Seek, SeekFrom, Write};

pub const IO_CHUNK: usize = 1024;
const ELF_HEADER_SIZE: usize = 52;
const PROGRAM_HEADER_SIZE: usize = 32;
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

#[derive(Clone, Copy, Debug)]
pub struct ElfHeader {
    pub entry: u32,
    pub program_offset: usize,
    pub program_entry_size: usize,
    pub program_count: usize,
    pub file_size: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct ProgramHeader {
    pub kind: u32,
    pub offset: u32,
    pub vaddr: u32,
    pub file_size: u32,
    pub memory_size: u32,
    pub flags: u32,
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
        .map_err(|error| format!("read at {}: {}", offset, error))
}

pub fn read_header<R: Read + Seek>(reader: &mut R) -> Result<ElfHeader, String> {
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

pub fn read_program_header<R: Read + Seek>(
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

pub fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc ^= u32::from(byte);
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

pub fn crc32<R: Read + Seek>(reader: &mut R, size: usize) -> Result<u32, String> {
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

pub fn copy_range<R: Read + Seek, W: Write>(
    reader: &mut R,
    offset: u32,
    size: u32,
    output: &mut W,
) -> Result<(), String> {
    reader
        .seek(SeekFrom::Start(u64::from(offset)))
        .map_err(|error| format!("seek segment: {}", error))?;
    let mut buffer = [0u8; IO_CHUNK];
    let mut remaining = size as usize;
    while remaining > 0 {
        let wanted = core::cmp::min(IO_CHUNK, remaining);
        reader
            .read_exact(&mut buffer[..wanted])
            .map_err(|error| format!("read segment: {}", error))?;
        output
            .write_all(&buffer[..wanted])
            .map_err(|error| format!("write segment: {}", error))?;
        remaining -= wanted;
    }
    Ok(())
}

pub fn read_bounded_tail<R: Read>(
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
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const ELF: &[u8] = include_bytes!("../../../../loader_app");

    struct BoundedReader<R> {
        inner: R,
        max_read: usize,
    }

    impl<R: Read> Read for BoundedReader<R> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            assert!(output.len() <= self.max_read);
            self.inner.read(output)
        }
    }

    impl<R: Seek> Seek for BoundedReader<R> {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[test]
    fn parses_loader_without_large_reads() {
        let mut reader = BoundedReader {
            inner: Cursor::new(ELF),
            max_read: IO_CHUNK,
        };
        let header = read_header(&mut reader).unwrap();
        assert_eq!(header.entry, 0x4220_01c8);
        assert!(header.program_count > 0);
        assert_eq!(header.file_size, ELF.len());
        assert_eq!(crc32(&mut reader, header.file_size).unwrap(), 0x8107_3659);
    }

    #[test]
    fn reads_program_header_fields() {
        let mut reader = Cursor::new(ELF);
        let header = read_header(&mut reader).unwrap();
        let program = read_program_header(&mut reader, &header, 0).unwrap();
        assert_ne!(program.kind, 0);
    }

    #[test]
    fn bounded_tail_rejects_oversize_and_trailing_data() {
        let mut oversized = Cursor::new([0u8; 8]);
        assert!(read_bounded_tail(&mut oversized, 8, 4).is_err());

        let mut trailing = Cursor::new([1u8, 2, 3]);
        assert!(read_bounded_tail(&mut trailing, 2, 4).is_err());

        let mut exact = Cursor::new([1u8, 2]);
        assert_eq!(read_bounded_tail(&mut exact, 2, 4).unwrap(), [1, 2]);
    }
}
