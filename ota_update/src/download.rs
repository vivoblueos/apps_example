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

use libc::timespec;
use librs::time::{clock_gettime, CLOCK_MONOTONIC};
use std::{
    fs::{remove_file, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering},
        Mutex, MutexGuard,
    },
    thread,
    time::Duration,
};

use crate::http::{self, HttpBody as _, Method, Request};
use crate::transfer::{
    self, FrameHeader, FrameType, TransferInfo, FRAME_HEADER_LEN, MAX_PAYLOAD_LEN,
};

const DOWNLOAD_SERVER: &str = "192.168.141.243:8000";
#[allow(dead_code)]
const DOWNLOAD_HTTP_SERVER: &str = "192.168.141.243:8001";
// Bare download keeps the default server file-to-device path mapping.
const DEFAULT_DOWNLOAD_FILE: &str = "loader_app";
const APPS_DIR: &str = "/data/apps";
const DOWNLOAD_DST: &str = "/data/apps/payload.elf";
const WINDOW_SIZE: usize = 8 * 1024;
const PROGRESS_INTERVAL: usize = 8 * 1024;
const MAX_DOWNLOAD_SIZE: usize = 1024 * 1024;
// 24KB stack for the debug-only separate-watcher variant (start_update_worker).
#[allow(dead_code)]
const UPDATE_STACK_SIZE: usize = 24 * 1024;
#[allow(dead_code)]
const HTTP_WRITE_RETRIES: u32 = 4;
#[allow(dead_code)]
const HTTP_WRITE_BACKOFF_MS: u64 = 5;

const STATUS_STOPPED: u8 = 0;
const STATUS_CONNECTING: u8 = 1;
const STATUS_IDLE: u8 = 2;
const STATUS_RECEIVING: u8 = 3;
const STATUS_RETRYING: u8 = 4;

static TRANSFER_BUSY: AtomicBool = AtomicBool::new(false);
static UPDATE_STARTED: AtomicBool = AtomicBool::new(false);
static UPDATE_STATUS: AtomicU8 = AtomicU8::new(STATUS_STOPPED);
static FS_LOCK: Mutex<()> = Mutex::new(());

// Static window buffer: receive_file is single-threaded (TRANSFER_BUSY permit
// + FS_LOCK), so it lives in BSS instead of heap/stack to dodge both OOM
// (contiguous heap unavailable once wifi/net_stack/socket allocated) and
// stack overflow (24KB thread stack can't hold an 8KB window).
static mut WINDOW_BUF: [u8; WINDOW_SIZE] = [0; WINDOW_SIZE];

static RECV_MS: AtomicU32 = AtomicU32::new(0);
static WRITE_MS: AtomicU32 = AtomicU32::new(0);
static ACK_MS: AtomicU32 = AtomicU32::new(0);
static EVICT_COUNT: AtomicU32 = AtomicU32::new(0);
static EVICT_MS: AtomicU32 = AtomicU32::new(0);
static WINDOW_COUNT: AtomicU32 = AtomicU32::new(0);
const EVICT_THRESHOLD_MS: u32 = 100;

fn now_ms() -> u32 {
    let mut ts = timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts as *mut timespec) };
    (ts.tv_sec as u32) * 1000 + (ts.tv_nsec as u32) / 1_000_000
}
fn acquire_fs_lock() -> MutexGuard<'static, ()> {
    FS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn with_fs_lock<T>(operation: impl FnOnce() -> T) -> T {
    fn lock_fs() -> MutexGuard<'static, ()> {
        FS_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    let _guard = lock_fs();
    operation()
}

struct TransferPermit;

impl TransferPermit {
    fn acquire() -> Result<Self, String> {
        TRANSFER_BUSY
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map(|_| Self)
            .map_err(|_| "another file transfer is in progress".to_string())
    }
}

impl Drop for TransferPermit {
    fn drop(&mut self) {
        TRANSFER_BUSY.store(false, Ordering::Release);
    }
}

#[derive(Debug)]
struct OwnedTransferInfo {
    filename: String,
    size: usize,
    crc32: u32,
}

fn protocol_error(context: &str, error: transfer::ProtocolError) -> String {
    format!("{}: {:?}", context, error)
}

fn send_frame(
    stream: &mut TcpStream,
    frame_type: FrameType,
    sequence: u8,
    payload: &[u8],
) -> Result<(), String> {
    let mut encoded = [0u8; FRAME_HEADER_LEN + MAX_PAYLOAD_LEN];
    let length = transfer::encode_frame(frame_type, sequence, payload, &mut encoded)
        .map_err(|error| protocol_error("encode frame failed", error))?;
    stream
        .write_all(&encoded[..length])
        .map_err(|error| format!("send {:?} failed: {}", frame_type, error))
}

fn read_frame_raw(
    stream: &mut TcpStream,
    payload: &mut [u8; MAX_PAYLOAD_LEN],
) -> Result<(FrameHeader, usize, bool), String> {
    let mut encoded_header = [0u8; FRAME_HEADER_LEN];
    stream
        .read_exact(&mut encoded_header)
        .map_err(|error| format!("read frame header failed: {}", error))?;
    let header = FrameHeader::decode(&encoded_header)
        .map_err(|error| protocol_error("decode frame header failed", error))?;
    let length = usize::from(header.payload_len);
    stream
        .read_exact(&mut payload[..length])
        .map_err(|error| format!("read {:?} payload failed: {}", header.frame_type, error))?;
    let crc_valid = transfer::crc32(&payload[..length]) == header.crc32;
    Ok((header, length, crc_valid))
}

fn read_frame(
    stream: &mut TcpStream,
    payload: &mut [u8; MAX_PAYLOAD_LEN],
) -> Result<(FrameHeader, usize), String> {
    let (header, length, crc_valid) = read_frame_raw(stream, payload)?;
    if !crc_valid {
        return Err(format!("frame {:?} CRC mismatch", header.frame_type));
    }
    Ok((header, length))
}

fn send_offset_frame(
    stream: &mut TcpStream,
    frame_type: FrameType,
    sequence: u8,
    offset: usize,
) -> Result<(), String> {
    let offset = u32::try_from(offset).map_err(|_| "transfer offset overflow".to_string())?;
    send_frame(stream, frame_type, sequence, &offset.to_le_bytes())
}

fn perform_handshake(stream: &mut TcpStream, role: &[u8]) -> Result<(), String> {
    send_frame(stream, FrameType::Hello, 0, role)?;
    let mut payload = [0u8; MAX_PAYLOAD_LEN];
    let (header, _) = read_frame(stream, &mut payload)?;
    if header.frame_type != FrameType::HelloAck {
        return Err(format!("expected HelloAck, got {:?}", header.frame_type));
    }
    Ok(())
}

fn connect_server() -> Result<TcpStream, String> {
    let stream = TcpStream::connect(DOWNLOAD_SERVER)
        .map_err(|error| format!("connect {} failed: {}", DOWNLOAD_SERVER, error))?;
    // Disable Nagle: the per-window Ack is a tiny 14B frame and must leave
    // immediately, not be coalesced/delayed (server already sets NODELAY).
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

fn decode_transfer_info(payload: &[u8]) -> Result<OwnedTransferInfo, String> {
    let info = TransferInfo::decode(payload)
        .map_err(|error| protocol_error("invalid transfer metadata", error))?;
    let size = usize::try_from(info.size).map_err(|_| "file size overflow".to_string())?;
    if size == 0 || size > MAX_DOWNLOAD_SIZE {
        return Err(format!(
            "file size {} exceeds limit {}",
            size, MAX_DOWNLOAD_SIZE
        ));
    }
    Ok(OwnedTransferInfo {
        filename: info.filename.to_string(),
        size,
        crc32: info.crc32,
    })
}

fn verify_file(path: &str, expected_size: usize, expected_crc: u32) -> Result<(), String> {
    let mut file = File::open(path).map_err(|error| format!("open {} failed: {}", path, error))?;
    let size = usize::try_from(
        file.metadata()
            .map_err(|error| format!("metadata {} failed: {}", path, error))?
            .len(),
    )
    .map_err(|_| "file length does not fit usize".to_string())?;
    if size != expected_size {
        return Err(format!(
            "file size verify failed: {} != {}",
            size, expected_size
        ));
    }

    let mut crc = 0xFFFF_FFFF;
    let mut buffer = [0u8; MAX_PAYLOAD_LEN];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("verify read {} failed: {}", path, error))?;
        if count == 0 {
            break;
        }
        crc = transfer::crc32_update(crc, &buffer[..count]);
    }
    let crc = !crc;
    if crc != expected_crc {
        return Err(format!(
            "file CRC verify failed: {:#010x} != {:#010x}",
            crc, expected_crc
        ));
    }
    Ok(())
}

fn send_unchanged_if_matches(
    stream: &mut TcpStream,
    path: &str,
    info: &OwnedTransferInfo,
) -> Result<bool, String> {
    let _fs_guard = acquire_fs_lock();
    if verify_file(path, info.size, info.crc32).is_err() {
        return Ok(false);
    }
    send_frame(stream, FrameType::Unchanged, 0, &[])?;
    Ok(true)
}

fn send_download_request(
    stream: &mut TcpStream,
    filename: &str,
    offset: usize,
) -> Result<(), String> {
    let off = u32::try_from(offset).map_err(|_| "transfer offset overflow".to_string())?;
    let mut payload = Vec::with_capacity(4 + filename.len());
    payload.extend_from_slice(&off.to_le_bytes());
    payload.extend_from_slice(filename.as_bytes());
    send_frame(stream, FrameType::DownloadRequest, 0, &payload)
}

fn crc_prefix(path: &str, offset: usize) -> Result<u32, String> {
    let mut file = File::open(path).map_err(|e| format!("open {} failed: {}", path, e))?;
    let mut crc = 0xFFFF_FFFF;
    let mut buffer = [0u8; MAX_PAYLOAD_LEN];
    let mut read_total = 0usize;
    while read_total < offset {
        let want = core::cmp::min(buffer.len(), offset - read_total);
        let n = file
            .read(&mut buffer[..want])
            .map_err(|e| format!("crc_prefix read failed: {}", e))?;
        if n == 0 {
            return Err(format!(
                "crc_prefix: {} short at {} < {}",
                path, read_total, offset
            ));
        }
        crc = transfer::crc32_update(crc, &buffer[..n]);
        read_total += n;
    }
    Ok(crc)
}

fn receive_file(
    stream: &mut TcpStream,
    temporary: &str,
    info: &OwnedTransferInfo,
    offset: usize,
) -> Result<(), String> {
    let _fs_guard = acquire_fs_lock();
    let mut output = if offset == 0 {
        File::create(temporary)
            .map_err(|error| format!("create {} failed: {}", temporary, error))?
    } else {
        let existing = std::fs::metadata(temporary)
            .map_err(|e| format!("resume {}: stat .part failed: {}", offset, e))?;
        let existing_len = usize::try_from(existing.len())
            .map_err(|_| "resume: part length does not fit usize".to_string())?;
        if existing_len < offset {
            return Err(format!(
                "resume: part len {} < requested offset {}",
                existing_len, offset
            ));
        }
        let mut file = OpenOptions::new()
            .write(true)
            .open(temporary)
            .map_err(|e| format!("resume: open {} failed: {}", temporary, e))?;
        file.seek(SeekFrom::Start(offset as u64))
            .map_err(|e| format!("resume: seek {} to {} failed: {}", temporary, offset, e))?;
        file
    };
    // SAFETY: receive_file is single-threaded — FS_LOCK guards entry and no
    // other accessor touches this buffer.
    let mut window = unsafe { &mut WINDOW_BUF[..] };
    let mut expected_sequence = 0u8;
    let mut received = offset;
    let mut transfer_crc = if offset == 0 {
        0xFFFF_FFFF
    } else {
        crc_prefix(temporary, offset)?
    };
    if offset > 0 {
        println!("download: {}/{} bytes (resumed)", received, info.size);
        let _ = std::io::stdout().flush();
    }

    while received < info.size {
        let target = core::cmp::min(WINDOW_SIZE, info.size - received);
        let window_sequence = expected_sequence;
        let mut retries = 0usize;
        let window_length;
        let t_recv_start = now_ms();

        loop {
            let mut payload = [0u8; MAX_PAYLOAD_LEN];
            let mut window_filled = 0usize;
            let mut window_valid = true;
            let mut seq = window_sequence;
            while window_filled < target {
                let (header, length, crc_valid) = read_frame_raw(stream, &mut payload)?;
                if header.frame_type != FrameType::Data || header.sequence != seq || !crc_valid {
                    window_valid = false;
                }
                window[window_filled..window_filled + length].copy_from_slice(&payload[..length]);
                window_filled += length;
                seq = seq.wrapping_add(1);
            }

            if window_valid {
                expected_sequence = seq;
                window_length = window_filled;
                break;
            }

            send_offset_frame(stream, FrameType::Nak, window_sequence, received)?;
            retries += 1;
            if retries > 3 {
                return Err(format!("window retry limit reached at {}", received));
            }
        }

        let t_recv_end = now_ms();
        RECV_MS.fetch_add(t_recv_end - t_recv_start, Ordering::Relaxed);

        transfer_crc = transfer::crc32_update(transfer_crc, &window[..window_length]);

        output
            .write_all(&window[..window_length])
            .map_err(|error| format!("write {} failed: {}", temporary, error))?;
        let t_write_end = now_ms();
        let write_ms = t_write_end - t_recv_end;
        WRITE_MS.fetch_add(write_ms, Ordering::Relaxed);
        if write_ms >= EVICT_THRESHOLD_MS {
            EVICT_COUNT.fetch_add(1, Ordering::Relaxed);
            EVICT_MS.fetch_add(write_ms, Ordering::Relaxed);
        }
        thread::yield_now();

        received += window_length;
        send_offset_frame(
            stream,
            FrameType::Ack,
            expected_sequence.wrapping_sub(1),
            received,
        )?;
        ACK_MS.fetch_add(now_ms() - t_write_end, Ordering::Relaxed);
        WINDOW_COUNT.fetch_add(1, Ordering::Relaxed);
        if received == info.size || received % PROGRESS_INTERVAL == 0 {
            println!(
                "download: {}/{} bytes ({}%)",
                received,
                info.size,
                received * 100 / info.size
            );
            let _ = std::io::stdout().flush();
            thread::yield_now();
        }
    }

    let mut end_header = [0u8; FRAME_HEADER_LEN];
    stream
        .read_exact(&mut end_header)
        .map_err(|error| format!("read transfer end failed: {}", error))?;
    let end = FrameHeader::decode(&end_header)
        .map_err(|error| protocol_error("decode transfer end failed", error))?;
    if end.frame_type != FrameType::TransferEnd || end.payload_len != 0 {
        return Err(format!("expected TransferEnd, got {:?}", end.frame_type));
    }
    output
        .flush()
        .map_err(|error| format!("flush {} failed: {}", temporary, error))?;
    drop(output);

    let actual_crc = !transfer_crc;
    if actual_crc != info.crc32 {
        return Err(format!(
            "transfer CRC mismatch: {:#010x} != {:#010x}",
            actual_crc, info.crc32
        ));
    }
    let windows = WINDOW_COUNT.load(Ordering::Relaxed);
    let recv_ms = RECV_MS.load(Ordering::Relaxed);
    let write_ms = WRITE_MS.load(Ordering::Relaxed);
    let ack_ms = ACK_MS.load(Ordering::Relaxed);
    let evict_count = EVICT_COUNT.load(Ordering::Relaxed);
    let evict_ms = EVICT_MS.load(Ordering::Relaxed);
    let total_ms = recv_ms + write_ms + ack_ms;
    println!(
        "diag: windows={} total={}ms recv={}ms write={}ms ack={}ms | evict={} cnt={}ms",
        windows, total_ms, recv_ms, write_ms, ack_ms, evict_count, evict_ms
    );
    let _ = std::io::stdout().flush();
    Ok(())
}

fn install_file(
    temporary: &str,
    destination: &str,
    _expected_size: usize,
    _expected_crc: u32,
) -> Result<(), String> {
    // rename-overwrite = in-place dir-entry swap (fast, no 817KB rewrite); .part
    // CRC already verified by receive_file, so no re-verify needed here.
    std::fs::rename(temporary, destination)
        .map_err(|error| format!("rename {} -> {} failed: {}", temporary, destination, error))
}

#[allow(dead_code)]
fn receive_file_http(temporary: &str, info: &OwnedTransferInfo) -> Result<(), String> {
    let _fs_guard = acquire_fs_lock();
    let mut output = File::create(temporary)
        .map_err(|error| format!("create {} failed: {}", temporary, error))?;

    let raw = TcpStream::connect(DOWNLOAD_HTTP_SERVER)
        .map_err(|error| format!("connect {} failed: {}", DOWNLOAD_HTTP_SERVER, error))?;
    let (host, port) = DOWNLOAD_HTTP_SERVER
        .split_once(':')
        .ok_or_else(|| format!("invalid http server '{}'", DOWNLOAD_HTTP_SERVER))?;
    let port: u16 = port
        .parse()
        .map_err(|_| format!("invalid http port in '{}'", DOWNLOAD_HTTP_SERVER))?;

    let target = format!("/files/{}", info.filename);
    let request = Request {
        method: Method::Get,
        target: &target,
        host,
        headers: &[],
        body: None,
    };

    let adapter = embedded_io_adapters::std::FromStd::new(raw);
    let response = http::send_request(adapter, request, 4 * 1024)
        .map_err(|error| format!("http request failed: {:?}", error))?;
    if !response.is_success() {
        return Err(format!(
            "http {} returned status {}",
            info.filename, response.status
        ));
    }
    println!(
        "download: http {} -> {}, streaming body...",
        info.filename, response.status
    );

    let mut body = response.body;
    let mut buffer = vec![0u8; 4096];
    let mut received = 0usize;
    let mut transfer_crc = 0xFFFF_FFFF;

    loop {
        let mut filled = 0usize;
        while filled < buffer.len() {
            let count = body
                .read(&mut buffer[filled..])
                .map_err(|error| format!("http body read failed: {:?}", error))?;
            if count == 0 {
                break;
            }
            filled += count;
        }
        if filled == 0 {
            break;
        }
        transfer_crc = transfer::crc32_update(transfer_crc, &buffer[..filled]);
        // write_all only — no per-chunk flush; let fatfs batch writes like the
        // frame protocol (flush once at the end). wait_busy fix removed the EIO
        // root cause; retry still guards residual SPI timing races.
        let mut retries = 0u32;
        loop {
            match output.write_all(&buffer[..filled]) {
                Ok(()) => break,
                Err(error) if retries < HTTP_WRITE_RETRIES => {
                    retries += 1;
                    thread::sleep(Duration::from_millis(HTTP_WRITE_BACKOFF_MS));
                }
                Err(error) => {
                    return Err(format!("write {} failed: {}", temporary, error));
                }
            }
        }
        received += filled;
        if received == info.size || received % PROGRESS_INTERVAL == 0 {
            println!(
                "download: {}/{} bytes ({}%)",
                received,
                info.size,
                received * 100 / info.size
            );
            // stdout is nonblocking under the current n_tty path; flush + yield
            // so the TX ring drains and the line actually leaves the board.
            let _ = std::io::stdout().flush();
            thread::yield_now();
        }
    }

    if let Err(error) = output.flush() {
        return Err(format!("flush {} failed: {}", temporary, error));
    }
    drop(output);

    if received != info.size {
        return Err(format!("http short read: {} != {}", received, info.size));
    }
    let actual_crc = !transfer_crc;
    if actual_crc != info.crc32 {
        return Err(format!(
            "transfer CRC mismatch: {:#010x} != {:#010x}",
            actual_crc, info.crc32
        ));
    }
    println!(
        "download: http {} complete ({} bytes, crc={:#010x})",
        info.filename, received, actual_crc
    );
    let _ = std::io::stdout().flush();
    thread::yield_now();
    Ok(())
}

#[allow(dead_code)]
fn install_file_http(temporary: &str, destination: &str) -> Result<(), String> {
    with_fs_lock(|| {
        std::fs::rename(temporary, destination)
            .map_err(|error| format!("rename {} -> {} failed: {}", temporary, destination, error))
    })
}

fn receive_and_install(
    stream: &mut TcpStream,
    destination: &str,
    info: &OwnedTransferInfo,
    offset: usize,
) -> Result<(), String> {
    let temporary = format!("{}.part", destination);
    if offset == 0 {
        let _ = remove_file(&temporary);
    }
    if let Err(error) = receive_file(stream, &temporary, info, offset) {
        return Err(error);
    }
    if let Err(error) =
        with_fs_lock(|| install_file(&temporary, destination, info.size, info.crc32))
    {
        let _ = remove_file(&temporary);
        return Err(error);
    }
    send_frame(stream, FrameType::Result, 0, b"OK")
}

fn send_protocol_error(stream: &mut TcpStream, error: &str) {
    let bytes = error.as_bytes();
    let length = core::cmp::min(bytes.len(), MAX_PAYLOAD_LEN);
    let _ = send_frame(stream, FrameType::Error, 0, &bytes[..length]);
}

pub fn command(file: Option<&str>) -> Result<(), String> {
    let source_file = file.unwrap_or(DEFAULT_DOWNLOAD_FILE);
    transfer::validate_filename(source_file)
        .map_err(|error| protocol_error("invalid download filename", error))?;
    let destination = match file {
        Some(_) => format!("{}/{}", APPS_DIR, source_file),
        None => DOWNLOAD_DST.to_string(),
    };
    let _permit = TransferPermit::acquire()?;

    let _ = std::fs::create_dir_all(APPS_DIR);
    clean_apps_dir(Some(&destination));

    let temporary = format!("{}.part", destination);
    let mut payload = [0u8; MAX_PAYLOAD_LEN];
    const MAX_RESUME_ATTEMPTS: usize = 8;
    let mut offset = 0usize;
    for attempt in 0..MAX_RESUME_ATTEMPTS {
        if offset == 0 {
            println!(
                "download: connecting to {} (attempt {})",
                DOWNLOAD_SERVER, attempt
            );
        } else {
            println!(
                "download: connecting to {} (attempt {}, resuming at {})",
                DOWNLOAD_SERVER, attempt, offset
            );
        }
        let mut stream = match connect_server() {
            Ok(s) => s,
            Err(error) => {
                eprintln!("download: attempt {} connect failed: {}", attempt, error);
                thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        if let Err(error) = perform_handshake(&mut stream, b"download") {
            eprintln!("download: attempt {} handshake failed: {}", attempt, error);
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        if let Err(error) = send_download_request(&mut stream, source_file, offset) {
            eprintln!("download: attempt {} request failed: {}", attempt, error);
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        let (header, length) = match read_frame(&mut stream, &mut payload) {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!(
                    "download: attempt {} read update failed: {}",
                    attempt, error
                );
                offset = std::fs::metadata(&temporary)
                    .map(|m| m.len() as usize)
                    .unwrap_or(0);
                thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        if header.frame_type != FrameType::Update {
            return Err(format!("expected Update, got {:?}", header.frame_type));
        }
        let info = decode_transfer_info(&payload[..length])?;
        if info.filename != source_file {
            return Err(format!(
                "server returned '{}' for requested '{}'",
                info.filename, source_file
            ));
        }
        if offset > 0 && offset >= info.size {
            return Err(format!(
                "resume offset {} >= size {}, corrupt",
                offset, info.size
            ));
        }
        if offset == 0 && send_unchanged_if_matches(&mut stream, &destination, &info)? {
            println!(
                "download: unchanged {} ({} bytes, crc={:#010x})",
                destination, info.size, info.crc32
            );
            return Ok(());
        }
        send_frame(&mut stream, FrameType::Ready, 0, &[])?;
        match receive_and_install(&mut stream, &destination, &info, offset) {
            Ok(()) => {
                println!(
                    "downloaded {} bytes to {} (crc={:#010x})",
                    info.size, destination, info.crc32
                );
                return Ok(());
            }
            Err(error) => {
                eprintln!(
                    "download: attempt {} failed at offset {}: {}",
                    attempt, offset, error
                );
                let next = std::fs::metadata(&temporary)
                    .map(|m| m.len() as usize)
                    .unwrap_or(0);
                if next >= info.size {
                    return Err(format!(
                        "part len {} >= size {}, corrupt after failure",
                        next, info.size
                    ));
                }
                offset = next;
                thread::sleep(Duration::from_millis(200));
                continue;
            }
        }
    }
    Err(format!(
        "download failed after {} resume attempts",
        MAX_RESUME_ATTEMPTS
    ))
}

fn run_update_connection() -> Result<(), String> {
    UPDATE_STATUS.store(STATUS_CONNECTING, Ordering::Release);
    let mut stream = connect_server()?;
    perform_handshake(&mut stream, b"watch")?;
    UPDATE_STATUS.store(STATUS_IDLE, Ordering::Release);
    println!(
        "update: connected to {}, waiting for changes",
        DOWNLOAD_SERVER
    );
    let mut payload = [0u8; MAX_PAYLOAD_LEN];

    loop {
        let (header, length) = read_frame(&mut stream, &mut payload)?;
        match header.frame_type {
            FrameType::Ping => {
                send_frame(&mut stream, FrameType::Pong, header.sequence, &[])?;
            }
            FrameType::Update => {
                let info = decode_transfer_info(&payload[..length])?;
                let destination = format!("{}/{}", APPS_DIR, info.filename);
                // Device already has an identical file — nothing to do.
                if send_unchanged_if_matches(&mut stream, &destination, &info)? {
                    println!(
                        "update: unchanged {} ({} bytes, crc={:#010x})",
                        destination, info.size, info.crc32
                    );
                    continue;
                }
                // Detected a new/changed file. The watcher only detects; it does
                // NOT push-receive here. Reply Unchanged to close this watch round
                // (server marks delivered, no re-push of this version), then run the
                // pull path — the same fast command() the REPL `download -f` uses
                // (~636s continuous). Doing the pull on this thread (24KB stack) is
                // fine; the REPL thread keeps blocking-read_line and is untouched.
                println!(
                    "update: detected {}, pulling via download -f {}",
                    info.filename, info.filename
                );
                send_frame(&mut stream, FrameType::Unchanged, 0, &[])?;
                drop(stream);
                let result = command(Some(&info.filename));
                match result {
                    Ok(()) => {
                        // Stage for boot-time apply: write marker + reboot. load+run
                        // happen at boot BEFORE wifi, when the heap is empty so the
                        // 64KB run-thread stack fits. In-process load+run here OOMs
                        // (net_stack+wifi already hold ~111KB of the ~171KB heap,
                        // leaving max_free < 64KB).
                        log_data_dir();
                        if let Err(e) =
                            std::fs::write(crate::PENDING_UPDATE_MARKER, destination.as_bytes())
                        {
                            eprintln!("update: write marker failed: {}, cannot apply", e);
                            return Ok(());
                        }
                        println!("update: staged {}, rebooting to apply", destination);
                        let _ = std::io::stdout().flush();
                        esp_rom_sys::rom::software_reset();
                    }
                    Err(error) => {
                        eprintln!(
                            "update: pull {} failed: {} (will retry on reconnect)",
                            info.filename, error
                        );
                    }
                }
                // Returning lets update_worker reconnect and resume watching. The
                // just-pulled version is now on-device (or failed — watcher reconnect
                // re-announces it; command() re-verifies via send_unchanged_if_matches).
                return Ok(());
            }
            FrameType::Error => {
                let message = core::str::from_utf8(&payload[..length]).unwrap_or("invalid error");
                return Err(format!("server error: {}", message));
            }
            other => return Err(format!("unexpected update frame: {:?}", other)),
        }
    }
}

// Remove files in /data/apps/. If `keep` is Some(path), that file and its
// matching `.part` (resume scratch) are kept; everything else (old ELF, stale
// .part, leftover .xip_load.img) is removed to free space before a new pull.
fn clean_apps_dir(keep: Option<&str>) {
    let entries = match std::fs::read_dir(APPS_DIR) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let part_keep = keep.map(|k| format!("{}.part", k));
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(k) = keep {
            if path == std::path::Path::new(k) {
                continue;
            }
            if let Some(pk) = &part_keep {
                if path == std::path::Path::new(pk) {
                    continue;
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

// Log the current on-device /data directory (file names + sizes) as a simple
// progress/audit line. Used at OTA loop entry and after a successful pull.
fn log_data_dir() {
    let entries = match std::fs::read_dir(APPS_DIR) {
        Ok(entries) => entries,
        Err(error) => {
            println!("data dir: cannot read {}: {}", APPS_DIR, error);
            return;
        }
    };
    let mut items: Vec<_> = entries.filter_map(Result::ok).collect();
    items.sort_by_key(|entry| entry.file_name());
    let mut line = String::from("data dir:");
    for entry in items {
        let name = entry.file_name();
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        line.push_str(&format!(" {}({}B)", name.to_string_lossy(), len));
    }
    println!("{}", line);
    let _ = std::io::stdout().flush();
}

// Single-thread auto OTA: the live entry point called from main(). Watches the
// server for new/changed files, pulls them on THIS thread (same fast command()
// path as REPL `download -f`), then clears/loads/runs the image in-place (no
// reboot). Entry clears /data/apps/ so a restart after an unexpected reboot
// starts clean instead of running out of space. On error it backs off and
// reconnects.
pub fn auto_ota_loop() {
    println!("update: entering auto OTA loop");
    let _ = std::fs::create_dir_all(APPS_DIR);
    clean_apps_dir(None);
    log_data_dir();
    const BACKOFF: [u64; 6] = [1, 2, 4, 8, 16, 30];
    let mut attempt = 0usize;
    loop {
        UPDATE_STATUS.store(STATUS_CONNECTING, Ordering::Release);
        match run_update_connection() {
            Ok(()) => attempt = 0,
            Err(error) => {
                UPDATE_STATUS.store(STATUS_RETRYING, Ordering::Release);
                let delay = BACKOFF[core::cmp::min(attempt, BACKOFF.len() - 1)];
                println!("update: {}. reconnecting in {}s", error, delay);
                thread::sleep(Duration::from_secs(delay));
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

#[allow(dead_code)] // separate-watcher variant kept for debug; auto_ota_loop is live
fn update_worker() {
    const BACKOFF: [u64; 6] = [1, 2, 4, 8, 16, 30];
    let mut attempt = 0usize;
    loop {
        match run_update_connection() {
            Ok(()) => attempt = 0,
            Err(error) => {
                UPDATE_STATUS.store(STATUS_RETRYING, Ordering::Release);
                let delay = BACKOFF[core::cmp::min(attempt, BACKOFF.len() - 1)];
                println!("update: {}. reconnecting in {}s", error, delay);
                thread::sleep(Duration::from_secs(delay));
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

#[allow(dead_code)] // debug REPL may start the watcher variant; auto OTA uses auto_ota_loop
pub fn start_update_worker() {
    if UPDATE_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(error) = thread::Builder::new()
        .name("firmware-update".to_string())
        .stack_size(UPDATE_STACK_SIZE)
        .spawn(update_worker)
    {
        UPDATE_STARTED.store(false, Ordering::Release);
        UPDATE_STATUS.store(STATUS_STOPPED, Ordering::Release);
        eprintln!("update: failed to start worker: {}", error);
    }
}

pub fn print_update_status() {
    let status = match UPDATE_STATUS.load(Ordering::Acquire) {
        STATUS_CONNECTING => "connecting",
        STATUS_IDLE => "idle",
        STATUS_RECEIVING => "receiving",
        STATUS_RETRYING => "retrying",
        _ => "stopped",
    };
    println!(
        "update: status={}, transfer_busy={}",
        status,
        TRANSFER_BUSY.load(Ordering::Acquire)
    );
}
