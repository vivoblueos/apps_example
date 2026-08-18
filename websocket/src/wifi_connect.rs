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

const WIFI_CONNECT_WAIT_MS: u32 = 15000;
const WIFI_SSID: &[u8] = blueos_kconfig::CONFIG_WLAN_SSID;
const WIFI_PASSWORD: &[u8] = blueos_kconfig::CONFIG_WLAN_PASSWORD;

pub fn wifi_connect(fd: i32) -> std::io::Result<()> {
    // SIOCSIWENCODE — cache passphrase for WPA2 network
    let mut iwreq = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data {
            encoding: libc::iw_point {
                pointer: WIFI_PASSWORD.as_ptr() as *mut libc::c_void,
                length: WIFI_PASSWORD.len() as u16,
                flags: 0,
            },
        },
    };
    let ret = unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCSIWENCODE,
            &mut iwreq as *mut _ as *mut libc::c_void,
        )
    };
    if let Err(librs::errno::Errno(errno)) = ret {
        eprintln!("SIOCSIWENCODE failed: errno={}", errno);
    }  

    // SIOCSIWESSID — trigger connect
    let mut iwreq = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data {
            essid: libc::iw_point {
                pointer: WIFI_SSID.as_ptr() as *mut libc::c_void,
                // The length should not include the null terminator, so we subtract 1.
                length: (WIFI_SSID.len() - 1) as u16,
                flags: 1,
            },
        },
    };
    let ret = unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCSIWESSID,
            &mut iwreq as *mut _ as *mut libc::c_void,
        )
    };
    if let Err(librs::errno::Errno(errno)) = ret {
        eprintln!("SIOCSIWESSID failed: errno={}", errno);
        return Err(std::io::Error::from_raw_os_error(errno));
    }

    Ok(())
}