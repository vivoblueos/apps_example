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

//! Wi-Fi UDP client example.
//!
//! Configure a UDP server on the phone at `CONFIG_NET_ROUTER_IP` and
//! `PHONE_UDP_SERVER_PORT`. The example sends five datagrams to that endpoint.

extern crate esp_radio_sys;
extern crate librs;
extern crate rsrt;

use librs::syscall::Syscall;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

const WIFI_CONNECT_WAIT_MS: u32 = 15000;
const SCAN_POLL_ATTEMPTS: usize = 25;
const SCAN_POLL_INTERVAL_MS: u32 = 200;
const WIFI_SSID: &[u8] = blueos_kconfig::CONFIG_WLAN_SSID;
const WIFI_PASSWORD: &[u8] = blueos_kconfig::CONFIG_WLAN_PASSWORD;
const PHONE_UDP_SERVER_PORT: u16 = 34448;
const UDP_LOCAL_PORT: u16 = 34230;
const UDP_TEST_PAYLOAD: &[u8] = b"blueos-wifi-udp-link-test";
const UDP_RECV_BUF_SIZE: usize = 512;
const UDP_SEND_COUNT: usize = 5;
const UDP_SEND_INTERVAL_MS: u32 = 500;

fn wlan0_name() -> [libc::c_char; 16] {
    let mut name = [0 as libc::c_char; 16];
    let bytes = b"wlan0\0";
    let mut i = 0;
    while i < bytes.len() && i < name.len() {
        name[i] = bytes[i] as libc::c_char;
        i += 1;
    }
    name
}

fn parse_config_ipv4(config: &[u8]) -> std::io::Result<Ipv4Addr> {
    let len = config
        .iter()
        .position(|&byte| byte == b'\0')
        .unwrap_or(config.len());
    let value = core::str::from_utf8(&config[..len])
        .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let mut parts = value.split('.');
    let mut octet = || {
        parts
            .next()
            .and_then(|part| part.parse::<u8>().ok())
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))
    };
    let ip = Ipv4Addr::new(octet()?, octet()?, octet()?, octet()?);
    if parts.next().is_some() {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(ip)
}

fn connect_wifi(fd: libc::c_int) -> std::io::Result<()> {
    let scan_request = libc::iw_scan_req {
        scan_type: libc::IW_SCAN_TYPE_ACTIVE as u8,
        essid_len: 0,
        num_channels: 0,
        flags: 0,
        bssid: unsafe { core::mem::zeroed() },
        essid: [0u8; libc::IW_ESSID_MAX_SIZE],
        min_channel_time: 0,
        max_channel_time: 0,
        channel_list: [libc::iw_freq {
            m: 0,
            e: 0,
            i: 0,
            flags: 0,
        }; libc::IW_MAX_FREQUENCIES],
    };
    let mut scan_request_wrapper = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data {
            essid: libc::iw_point {
                pointer: &scan_request as *const libc::iw_scan_req as *mut libc::c_void,
                length: core::mem::size_of::<libc::iw_scan_req>() as u16,
                flags: 0,
            },
        },
    };
    let ret = unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCSIWSCAN,
            &mut scan_request_wrapper as *mut _ as *mut libc::c_void,
        )
    };
    if let Err(librs::errno::Errno(errno)) = ret {
        eprintln!("SIOCSIWSCAN failed: errno={}", errno);
        return Err(std::io::Error::from_raw_os_error(errno));
    }

    let mut scan_buffer = vec![0u8; 4096];
    let scan_data = libc::iw_point {
        pointer: scan_buffer.as_mut_ptr() as *mut libc::c_void,
        length: scan_buffer.len() as u16,
        flags: 0,
    };
    let mut scan_results = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data { data: scan_data },
    };
    let mut scan_ready = false;
    for attempt in 0..SCAN_POLL_ATTEMPTS {
        let ret = unsafe {
            librs::syscall::sys::Sys::ioctl(
                fd,
                libc::SIOCGIWSCAN,
                &mut scan_results as *mut _ as *mut libc::c_void,
            )
        };
        match ret {
            Ok(_) => {
                scan_ready = true;
                break;
            }
            Err(librs::errno::Errno(errno)) if errno == libc::EAGAIN => {
                if attempt + 1 < SCAN_POLL_ATTEMPTS {
                    let _ = librs::time::msleep(SCAN_POLL_INTERVAL_MS);
                }
            }
            Err(librs::errno::Errno(errno)) => {
                eprintln!("SIOCGIWSCAN failed: errno={}", errno);
                return Err(std::io::Error::from_raw_os_error(errno));
            }
        }
    }
    if !scan_ready {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Wi-Fi scan did not complete",
        ));
    }

    let mut password_request = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data {
            encoding: libc::iw_point {
                pointer: WIFI_PASSWORD.as_ptr() as *mut libc::c_void,
                length: (WIFI_PASSWORD.len() - 1) as u16,
                flags: 0,
            },
        },
    };
    let ret = unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCSIWENCODE,
            &mut password_request as *mut _ as *mut libc::c_void,
        )
    };
    if let Err(librs::errno::Errno(errno)) = ret {
        eprintln!("SIOCSIWENCODE failed: errno={}", errno);
    }

    let mut ssid_request = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data {
            essid: libc::iw_point {
                pointer: WIFI_SSID.as_ptr() as *mut libc::c_void,
                length: (WIFI_SSID.len() - 1) as u16,
                flags: 1,
            },
        },
    };
    let ret = unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCSIWESSID,
            &mut ssid_request as *mut _ as *mut libc::c_void,
        )
    };
    if let Err(librs::errno::Errno(errno)) = ret {
        eprintln!("SIOCSIWESSID failed: errno={}", errno);
        return Err(std::io::Error::from_raw_os_error(errno));
    }

    println!("Wi-Fi connect triggered, waiting for station connection...");
    let _ = librs::time::msleep(WIFI_CONNECT_WAIT_MS);
    Ok(())
}

fn run_udp_echo() -> std::io::Result<()> {
    let phone_addr = SocketAddrV4::new(
        parse_config_ipv4(blueos_kconfig::CONFIG_NET_ROUTER_IP)?,
        PHONE_UDP_SERVER_PORT,
    );
    let local_ip = parse_config_ipv4(blueos_kconfig::CONFIG_NET_STATIC_IP)?;
    let socket = UdpSocket::bind(SocketAddrV4::new(local_ip, UDP_LOCAL_PORT))?;

    println!("UDP local address: {}:{}", local_ip, UDP_LOCAL_PORT);
    println!("UDP server address: {}", phone_addr);

    for attempt in 1..=UDP_SEND_COUNT {
        let sent = socket.send_to(UDP_TEST_PAYLOAD, phone_addr)?;
        if sent != UDP_TEST_PAYLOAD.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "UDP datagram was only partially sent",
            ));
        }
        println!(
            "UDP TX OK: sent {}/{}: {} bytes to {}: {:?}",
            attempt, UDP_SEND_COUNT, sent, phone_addr, UDP_TEST_PAYLOAD
        );
        if attempt < UDP_SEND_COUNT {
            let _ = librs::time::msleep(UDP_SEND_INTERVAL_MS);
        }
    }

    let mut recv_buf = [0u8; UDP_RECV_BUF_SIZE];
    let (received, sender) = socket.recv_from(&mut recv_buf)?;
    println!(
        "UDP RX OK: received {} bytes from {}: {:?}",
        received,
        sender,
        &recv_buf[..received]
    );
    if let Ok(text) = core::str::from_utf8(&recv_buf[..received]) {
        println!("UDP RX text: {}", text);
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    println!("UDP client example");
    let fd = librs::net::socket::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    connect_wifi(fd)?;
    run_udp_echo()
}
