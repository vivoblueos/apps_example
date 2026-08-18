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
mod wifi_scan;
mod wifi_connect;

use librs::syscall::Syscall;
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
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
const TCP_PORT: u16 = 34228;

fn parse_router_ip() -> std::io::Result<Ipv4Addr> {
    let ip_bytes = blueos_kconfig::CONFIG_NET_ROUTER_IP;
    let len = ip_bytes
        .iter()
        .position(|&byte| byte == b'\0')
        .unwrap_or(ip_bytes.len());
    let ip = core::str::from_utf8(&ip_bytes[..len])
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let mut parts = ip.split('.');

    let a = parts
        .next()
        .and_then(|part| part.parse::<u8>().ok())
        .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let b = parts
        .next()
        .and_then(|part| part.parse::<u8>().ok())
        .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let c = parts
        .next()
        .and_then(|part| part.parse::<u8>().ok())
        .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let d = parts
        .next()
        .and_then(|part| part.parse::<u8>().ok())
        .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?;

    if parts.next().is_some() {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }

    Ok(Ipv4Addr::new(a, b, c, d))
}

fn main() -> std::io::Result<()> {
    let _d = librs::time::msleep(1000);

    let wifi_ssid = std::str::from_utf8(WIFI_SSID).unwrap_or("<invalid utf8>");
    let fd = librs::net::socket::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
    if fd < 0 {
        eprintln!(
            "Failed to create socket: {}",
            std::io::Error::last_os_error()
        );
        return Err(std::io::Error::last_os_error());
    }

    wifi_scan::wifi_scan(fd, wifi_ssid)?;

    println!("Connecting to WiFi SSID: {} (WPA2)...", wifi_ssid);

    wifi_connect::wifi_connect(fd)?;

    println!("WiFi connect triggered, waiting for station connection...");

    let tcp_server = SocketAddrV4::new(parse_router_ip()?, TCP_PORT);
    let mut stream = TcpStream::connect(tcp_server)?;

    let mut websocket = WebSocketClient::new_client();

    Ok(())
}
