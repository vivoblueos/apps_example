// Copyright (c) 2025 vivo Mobile Communication Co., Ltd.
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

extern crate esp_radio_sys;
extern crate rsrt;

mod wifi_connect;
mod wifi_scan;

use embedded_websocket as ws;
use librs::syscall::Syscall;
use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream},
};
use ws::{
    framer::{Framer, FramerError, ReadResult, Stream as WebSocketStream},
    WebSocketContext, WebSocketSendMessageType, WebSocketServer,
};

pub fn wlan0_name() -> [libc::c_char; 16] {
    let mut name = [0 as libc::c_char; 16];
    let bytes = b"wlan0\0";
    let mut i = 0;
    while i < bytes.len() && i < name.len() {
        name[i] = bytes[i] as libc::c_char;
        i += 1;
    }
    name
}

const WIFI_SSID: &[u8] = blueos_kconfig::CONFIG_WLAN_SSID;
const WIFI_CONNECT_WAIT_MS: u32 = 15000;
const WEBSOCKET_PORT: u16 = 34228;
const WEBSOCKET_PATH: &str = "/chat";
const HTTP_HEADER_BUF_SIZE: usize = 2048;
const WEBSOCKET_BUF_SIZE: usize = 2048;

#[derive(Debug)]
enum WebServerError {
    Io(io::Error),
    Http(httparse::Error),
    Framer(FramerError<io::Error>),
    WebSocket(ws::Error),
    HttpHeaderTooLarge,
}

impl From<io::Error> for WebServerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<httparse::Error> for WebServerError {
    fn from(error: httparse::Error) -> Self {
        Self::Http(error)
    }
}

impl From<FramerError<io::Error>> for WebServerError {
    fn from(error: FramerError<io::Error>) -> Self {
        Self::Framer(error)
    }
}

impl From<ws::Error> for WebServerError {
    fn from(error: ws::Error) -> Self {
        Self::WebSocket(error)
    }
}

struct TcpStreamAdapter(TcpStream);

impl WebSocketStream<io::Error> for TcpStreamAdapter {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, io::Error> {
        self.0.read(buffer)
    }

    fn write_all(&mut self, buffer: &[u8]) -> Result<(), io::Error> {
        self.0.write_all(buffer)
    }
}

fn config_bytes(value: &[u8]) -> &[u8] {
    let len = value
        .iter()
        .position(|&byte| byte == b'\0')
        .unwrap_or(value.len());
    &value[..len]
}

fn parse_ipv4(value: &[u8]) -> io::Result<Ipv4Addr> {
    let value = core::str::from_utf8(config_bytes(value))
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    value
        .parse()
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn write_http_error(stream: &mut TcpStream, status: &str) -> io::Result<()> {
    let response = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(response.as_bytes())
}

fn read_websocket_header(
    stream: &mut TcpStream,
) -> Result<Option<WebSocketContext>, WebServerError> {
    let mut request_buf = [0; HTTP_HEADER_BUF_SIZE];
    let mut request_len = 0;

    // Read one byte at a time so no websocket frame is consumed together with
    // the HTTP upgrade request. Framer owns the next read from the stream.
    loop {
        if request_len == request_buf.len() {
            return Err(WebServerError::HttpHeaderTooLarge);
        }

        let read_len = stream.read(&mut request_buf[request_len..request_len + 1])?;
        if read_len == 0 {
            return Ok(None);
        }
        request_len += read_len;

        if request_len >= 4 && &request_buf[request_len - 4..request_len] == b"\r\n\r\n" {
            break;
        }
    }

    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(&request_buf[..request_len])? {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => return Err(WebServerError::HttpHeaderTooLarge),
    }

    if request.method != Some("GET") {
        write_http_error(stream, "405 Method Not Allowed")?;
        return Ok(None);
    }
    if request.path != Some(WEBSOCKET_PATH) {
        write_http_error(stream, "404 Not Found")?;
        return Ok(None);
    }

    let context = ws::read_http_header(
        request
            .headers
            .iter()
            .map(|header| (header.name, header.value)),
    )?;
    if context.is_none() {
        write_http_error(stream, "400 Bad Request")?;
    }
    Ok(context)
}

fn handle_client(stream: TcpStream) -> Result<(), WebServerError> {
    println!("WebSocket client connected");

    let mut stream = stream;
    let Some(websocket_context) = read_websocket_header(&mut stream)? else {
        return Ok(());
    };

    let mut read_buf = [0; WEBSOCKET_BUF_SIZE];
    let mut read_cursor = 0;
    let mut write_buf = [0; WEBSOCKET_BUF_SIZE];
    let mut frame_buf = [0; WEBSOCKET_BUF_SIZE];
    let mut websocket = WebSocketServer::new_server();
    let mut framer = Framer::new(
        &mut read_buf,
        &mut read_cursor,
        &mut write_buf,
        &mut websocket,
    );
    let mut stream = TcpStreamAdapter(stream);

    framer.accept(&mut stream, &websocket_context)?;
    println!("WebSocket connection opened");

    loop {
        match framer.read(&mut stream, &mut frame_buf)? {
            ReadResult::Text(text) => {
                println!("RX text: {}", text);
                framer.write(
                    &mut stream,
                    WebSocketSendMessageType::Text,
                    true,
                    text.as_bytes(),
                )?;
            }
            ReadResult::Binary(data) => {
                println!("RX {} binary bytes", data.len());
                framer.write(&mut stream, WebSocketSendMessageType::Binary, true, data)?;
            }
            ReadResult::Pong(data) => {
                println!("RX pong ({} bytes)", data.len());
            }
            ReadResult::Closed => break,
        }
    }

    println!("WebSocket connection closed");
    Ok(())
}

fn main() -> io::Result<()> {
    println!("WebSocket example starting");
    let _ = librs::time::msleep(1000);

    println!("WebSocket example");
    let wifi_ssid = core::str::from_utf8(config_bytes(WIFI_SSID)).unwrap_or("<invalid utf8>");
    println!("Opening WLAN control socket...");
    let fd = librs::net::socket::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
    if fd < 0 {
        eprintln!("Failed to create socket: {}", io::Error::last_os_error());
        return Err(io::Error::last_os_error());
    }

    println!("Scanning for WiFi networks...");
    wifi_scan::wifi_scan(fd, wifi_ssid)?;
    println!("WiFi scan finished");
    println!("Connecting to WiFi SSID: {} (WPA2)...", wifi_ssid);
    wifi_connect::wifi_connect(fd)?;
    println!("WiFi connect triggered, waiting for station connection...");
    let _ = librs::time::msleep(WIFI_CONNECT_WAIT_MS);

    println!("WiFi wait finished, binding WebSocket listener...");
    let server_addr = SocketAddrV4::new(
        parse_ipv4(blueos_kconfig::CONFIG_NET_STATIC_IP)?,
        WEBSOCKET_PORT,
    );
    let listener = TcpListener::bind(server_addr)?;
    println!(
        "WebSocket server listening on ws://{}/{}",
        SocketAddr::V4(server_addr),
        WEBSOCKET_PATH.trim_start_matches('/')
    );

    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                if let Err(error) = handle_client(stream) {
                    eprintln!("WebSocket session with {} failed: {:?}", peer, error);
                }
            }
            Err(error) => eprintln!("WebSocket accept failed: {}", error),
        }
    }
}
