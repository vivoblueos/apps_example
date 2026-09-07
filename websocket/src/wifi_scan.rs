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

use crate::wlan0_name;
use librs::syscall::Syscall;

const SCAN_POLL_ATTEMPTS: usize = 25;
const SCAN_POLL_INTERVAL_MS: u32 = 200;

fn security_name(sec: u8) -> &'static str {
    match sec {
        0 => "Open",
        1 => "WEP",
        2 => "WPA",
        3 => "WPA2",
        4 => "WPA3",
        _ => "Unknown",
    }
}

pub fn wifi_scan(fd: i32, wifi_ssid: &str) -> std::io::Result<()> {
    println!("Triggering WiFi scan...");
    let scan_req = libc::iw_scan_req {
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
    let mut iwreq = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data {
            essid: libc::iw_point {
                pointer: &scan_req as *const libc::iw_scan_req as *mut libc::c_void,
                length: core::mem::size_of::<libc::iw_scan_req>() as u16,
                flags: 0,
            },
        },
    };
    let ret = unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCSIWSCAN,
            &mut iwreq as *mut _ as *mut libc::c_void,
        )
    };
    if let Err(librs::errno::Errno(errno)) = ret {
        eprintln!("SIOCSIWSCAN failed: errno={}", errno);
        return Err(std::io::Error::from_raw_os_error(errno));
    }
    println!("Scan triggered, waiting for results...");

    let mut buf = vec![0u8; 4096];
    let data = libc::iw_point {
        pointer: buf.as_mut_ptr() as *mut libc::c_void,
        length: buf.len() as u16,
        flags: 0,
    };
    let mut iwreq = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data { data },
    };
    let mut total = None;
    for attempt in 0..SCAN_POLL_ATTEMPTS {
        let ret = unsafe {
            librs::syscall::sys::Sys::ioctl(
                fd,
                libc::SIOCGIWSCAN,
                &mut iwreq as *mut _ as *mut libc::c_void,
            )
        };
        match ret {
            Ok(n) if n >= 0 => {
                total = Some(n as usize);
                println!("WiFi scan results ready after {} poll(s)", attempt + 1);
                break;
            }
            Err(librs::errno::Errno(errno)) if attempt + 1 < SCAN_POLL_ATTEMPTS => {
                if errno != libc::EAGAIN {
                    eprintln!("SIOCGIWSCAN failed: errno={}", errno);
                    return Err(std::io::Error::from_raw_os_error(errno));
                }
                let _ = librs::time::msleep(SCAN_POLL_INTERVAL_MS);
            }
            Err(librs::errno::Errno(errno)) => {
                eprintln!(
                    "SIOCGIWSCAN failed or scan results are not ready: errno={}",
                    errno
                );
                return Err(std::io::Error::from_raw_os_error(errno));
            }
            _ => {
                eprintln!("SIOCGIWSCAN returned an invalid success value");
                return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
            }
        }
    }
    let total = total.unwrap_or(0);

    if total < 4 {
        println!("No scan results ({} bytes returned)", total);
        return Ok(());
    }
    let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    println!("Found {} networks:\n", count);

    let mut off = 4usize;
    let mut found_ssid = false;
    for i in 0..count {
        if off + 4 > total {
            break;
        }
        let ssid_len = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        off += 4;

        if off + ssid_len as usize > total {
            break;
        }
        let ssid =
            core::str::from_utf8(&buf[off..off + ssid_len as usize]).unwrap_or("<invalid utf8>");
        off += ssid_len as usize;

        if off + 6 > total {
            break;
        }
        let bssid = &buf[off..off + 6];
        off += 6;

        if off + 1 > total {
            break;
        }
        let signal = buf[off] as i8;
        off += 1;

        if off + 2 > total {
            break;
        }
        let channel = u16::from_le_bytes([buf[off], buf[off + 1]]);
        off += 2;

        if off + 1 > total {
            break;
        }
        let security = buf[off];
        off += 1;

        println!("  {}. SSID: {}", i + 1, ssid);
        println!(
            "     BSSID: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            bssid[0], bssid[1], bssid[2], bssid[3], bssid[4], bssid[5]
        );
        println!("     Signal: {} dBm", signal);
        println!("     Channel: {}", channel);
        println!("     Security: {}", security_name(security));
        println!();

        if ssid == wifi_ssid {
            found_ssid = true;
        }
    }

    if !found_ssid {
        println!("SSID '{}' not found in scan results", wifi_ssid);
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("SSID '{}' not found in scan results", wifi_ssid),
        ));
    }

    Ok(())
}
