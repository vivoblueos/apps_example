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

use std::{
    fs::{remove_file, File},
    io::{Read, Write},
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Mutex, MutexGuard,
    },
    thread,
    time::Duration,
};

use crate::transfer::{
    self, FrameHeader, FrameType, TransferInfo, FRAME_HEADER_LEN, MAX_PAYLOAD_LEN,
};

const DOWNLOAD_SERVER: &str = "192.168.101.89:8000";
// Bare download keeps the default server file-to-device path mapping.
const DEFAULT_DOWNLOAD_FILE: &str = "loader_app";
const DOWNLOAD_DST: &str = "/data/payload.elf";
const WINDOW_SIZE: usize = 1024;
const PROGRESS_INTERVAL: usize = 16 * 1024;
const MAX_DOWNLOAD_SIZE: usize = 1024 * 1024;
const UPDATE_STACK_SIZE: usize = 12 * 1024;

const STATUS_STOPPED: u8 = 0;
const STATUS_CONNECTING: u8 = 1;
const STATUS_IDLE: u8 = 2;
const STATUS_RECEIVING: u8 = 3;
const STATUS_RETRYING: u8 = 4;

static TRANSFER_BUSY: AtomicBool = AtomicBool::new(false);
static UPDATE_STARTED: AtomicBool = AtomicBool::new(false);
static UPDATE_STATUS: AtomicU8 = AtomicU8::new(STATUS_STOPPED);
static FS_LOCK: Mutex<()> = Mutex::new(());
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

fn read_frame_raw_with_payload_len<'a>(
    stream: &mut TcpStream,
    expected_payload_len: usize,
    encoded: &'a mut [u8; FRAME_HEADER_LEN + MAX_PAYLOAD_LEN],
) -> Result<(FrameHeader, &'a [u8], bool), String> {
    let frame_len = FRAME_HEADER_LEN + expected_payload_len;
    stream
        .read_exact(&mut encoded[..frame_len])
        .map_err(|error| format!("read data frame failed: {}", error))?;
    transfer::decode_frame_raw_with_payload_len(&encoded[..frame_len], expected_payload_len)
        .map_err(|error| protocol_error("decode data frame failed", error))
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
    TcpStream::connect(DOWNLOAD_SERVER)
        .map_err(|error| format!("connect {} failed: {}", DOWNLOAD_SERVER, error))
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

fn receive_file(
    stream: &mut TcpStream,
    temporary: &str,
    info: &OwnedTransferInfo,
) -> Result<(), String> {
    let _fs_guard = acquire_fs_lock();
    let mut output = File::create(temporary)
        .map_err(|error| format!("create {} failed: {}", temporary, error))?;
    let mut encoded_frame = [0u8; FRAME_HEADER_LEN + MAX_PAYLOAD_LEN];
    let mut window = [0u8; WINDOW_SIZE];
    let mut expected_sequence = 0u8;
    let mut received = 0usize;
    let mut transfer_crc = 0xFFFF_FFFF;

    while received < info.size {
        let target = core::cmp::min(WINDOW_SIZE, info.size - received);
        let window_sequence = expected_sequence;
        let mut retries = 0usize;
        let window_length;

        loop {
            let mut candidate_length = 0usize;
            let mut candidate_sequence = window_sequence;
            let mut window_valid = true;

            while candidate_length < target {
                let expected_payload_len =
                    core::cmp::min(MAX_PAYLOAD_LEN, target - candidate_length);
                let (header, frame_payload, crc_valid) = read_frame_raw_with_payload_len(
                    stream,
                    expected_payload_len,
                    &mut encoded_frame,
                )?;
                let length = frame_payload.len();
                if header.frame_type != FrameType::Data
                    || header.sequence != candidate_sequence
                    || !crc_valid
                {
                    window_valid = false;
                }
                window[candidate_length..candidate_length + length].copy_from_slice(frame_payload);
                candidate_length += length;
                candidate_sequence = candidate_sequence.wrapping_add(1);
            }

            if window_valid {
                expected_sequence = candidate_sequence;
                window_length = candidate_length;
                break;
            }

            send_offset_frame(stream, FrameType::Nak, window_sequence, received)?;
            retries += 1;
            if retries > 3 {
                return Err(format!("window retry limit reached at {}", received));
            }
        }

        transfer_crc = transfer::crc32_update(transfer_crc, &window[..window_length]);

        output
            .write_all(&window[..window_length])
            .map_err(|error| format!("write {} failed: {}", temporary, error))?;
        thread::yield_now();

        received += window_length;
        send_offset_frame(
            stream,
            FrameType::Ack,
            expected_sequence.wrapping_sub(1),
            received,
        )?;
        if received == info.size || received % PROGRESS_INTERVAL == 0 {
            println!(
                "download: {}/{} bytes ({}%)",
                received,
                info.size,
                received * 100 / info.size
            );
        }
    }

    let (end, end_payload, crc_valid) =
        read_frame_raw_with_payload_len(stream, 0, &mut encoded_frame)?;
    if end.frame_type != FrameType::TransferEnd || !end_payload.is_empty() || !crc_valid {
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
    Ok(())
}

fn install_file(
    temporary: &str,
    destination: &str,
    expected_size: usize,
    expected_crc: u32,
) -> Result<(), String> {
    let mut source =
        File::open(temporary).map_err(|error| format!("open {} failed: {}", temporary, error))?;
    let mut target = File::create(destination)
        .map_err(|error| format!("create {} failed: {}", destination, error))?;
    let mut buffer = [0u8; 4096];
    loop {
        let count = source
            .read(&mut buffer)
            .map_err(|error| format!("read {} failed: {}", temporary, error))?;
        if count == 0 {
            break;
        }
        target
            .write_all(&buffer[..count])
            .map_err(|error| format!("write {} failed: {}", destination, error))?;
    }
    target
        .flush()
        .map_err(|error| format!("flush {} failed: {}", destination, error))?;
    drop(target);
    drop(source);
    verify_file(destination, expected_size, expected_crc)?;
    remove_file(temporary).map_err(|error| format!("remove {} failed: {}", temporary, error))
}

fn receive_and_install(
    stream: &mut TcpStream,
    destination: &str,
    info: &OwnedTransferInfo,
) -> Result<(), String> {
    let temporary = format!("{}.part", destination);
    let _ = remove_file(&temporary);
    if let Err(error) = receive_file(stream, &temporary, info) {
        let _ = remove_file(&temporary);
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
        Some(_) => format!("/data/{}", source_file),
        None => DOWNLOAD_DST.to_string(),
    };
    let _permit = TransferPermit::acquire()?;

    println!("download: connecting to {}", DOWNLOAD_SERVER);
    let mut stream = connect_server()?;
    perform_handshake(&mut stream, b"download")?;
    send_frame(
        &mut stream,
        FrameType::DownloadRequest,
        0,
        source_file.as_bytes(),
    )?;
    let mut payload = [0u8; MAX_PAYLOAD_LEN];
    let (header, length) = read_frame(&mut stream, &mut payload)?;
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
    if send_unchanged_if_matches(&mut stream, &destination, &info)? {
        println!(
            "download: unchanged {} ({} bytes, crc={:#010x})",
            destination, info.size, info.crc32
        );
        return Ok(());
    }
    send_frame(&mut stream, FrameType::Ready, 0, &[])?;
    if let Err(error) = receive_and_install(&mut stream, &destination, &info) {
        send_protocol_error(&mut stream, &error);
        return Err(error);
    }
    println!(
        "downloaded {} bytes to {} (crc={:#010x})",
        info.size, destination, info.crc32
    );
    Ok(())
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
                let destination = format!("/data/{}", info.filename);
                if send_unchanged_if_matches(&mut stream, &destination, &info)? {
                    println!(
                        "update: unchanged {} ({} bytes, crc={:#010x})",
                        destination, info.size, info.crc32
                    );
                    continue;
                }
                let permit = match TransferPermit::acquire() {
                    Ok(permit) => permit,
                    Err(error) => {
                        send_protocol_error(&mut stream, &error);
                        continue;
                    }
                };
                UPDATE_STATUS.store(STATUS_RECEIVING, Ordering::Release);
                println!(
                    "update: receiving {} ({} bytes, crc={:#010x})",
                    info.filename, info.size, info.crc32
                );
                send_frame(&mut stream, FrameType::Ready, 0, &[])?;
                let result = receive_and_install(&mut stream, &destination, &info);
                drop(permit);
                UPDATE_STATUS.store(STATUS_IDLE, Ordering::Release);
                match result {
                    Ok(()) => println!(
                        "update: replaced {} ({} bytes, crc={:#010x})",
                        destination, info.size, info.crc32
                    ),
                    Err(error) => {
                        send_protocol_error(&mut stream, &error);
                        return Err(error);
                    }
                }
            }
            FrameType::Error => {
                let message = core::str::from_utf8(&payload[..length]).unwrap_or("invalid error");
                return Err(format!("server error: {}", message));
            }
            other => return Err(format!("unexpected update frame: {:?}", other)),
        }
    }
}

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
