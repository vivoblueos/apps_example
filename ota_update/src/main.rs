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

extern crate alloc;
extern crate esp_radio_sys;
extern crate esp_rom_sys; // for rom::software_reset() (reboot-based OTA)
extern crate librs;
extern crate rsrt;

mod download;
#[allow(dead_code)]
mod http; // copied from apps/example/agent_loop/src/http.rs (plain HTTP, no TLS)
mod image; // copied from apps/shell/src/commands/image.rs (DROM-aware, self-contained)
mod transfer;
mod wifi;

use alloc::string::ToString;
use std::{fs, io::Write as _, thread};

pub const PENDING_UPDATE_MARKER: &str = "/data/.pending_update";

fn main() {
    thread::Builder::new()
        .name("ota_update".to_string())
        .stack_size(24 << 10)
        .spawn(move || {
            println!("Hello, ota_update!");
            let pending = pending_update_path();
            // load (if pending) happens at boot BEFORE wifi, when the heap is
            // emptiest. wifi scan+connect runs next while heap is still loose.
            if let Some(path) = &pending {
                try_apply_pending_update(path);
            }
            wifi::connect_wifi();
            if pending.is_some() {
                // Run the installed image directly on THIS thread, not on a
                // spawned ota_run thread. This thread already runs on a 24KB
                // stack (set at app entry), so slint's stack needs are covered
                // without allocating a SECOND 24KB stack from the heap.
                //
                // Spawning carved another 24KB out of the heap (measured
                // 43KB→18KB) and run() then OOM'd on a 4KB alloc / trapped at
                // entry. A 3s sleep after wifi freed nothing (43KB→43KB):
                // wifi's transient buffers were already released; the 43KB
                // floor is net-stack resident + the thread stack, neither
                // recoverable by waiting. Inline, heap stays at 43KB so run()
                // has room. slint is slent, so auto_ota_loop below won't run
                // while the image is up — fine for pending=Some (just rebooted
                // to apply this update, no need to watch immediately).
                if let Err(e) = image::command(&["run"]) {
                    eprintln!("update: image run failed: {}", e);
                }
            }
            download::auto_ota_loop();
        })
        .unwrap()
        .join()
        .unwrap();
}

fn pending_update_path() -> Option<String> {
    match fs::read_to_string(PENDING_UPDATE_MARKER) {
        Ok(content) => Some(content.trim().to_string()),
        Err(_) => None, // no pending update — normal boot
    }
}

fn try_apply_pending_update(path: &str) {
    println!("update: pending update detected: {}", path);
    let load_result = download::with_fs_lock(|| {
        if let Err(error) = image::command(&["load", path]) {
            return Err(format!("image load {} failed: {}", path, error));
        }
        if let Err(error) = fs::remove_file(PENDING_UPDATE_MARKER) {
            eprintln!("update: warn: cannot remove marker: {}", error);
        }
        Ok::<(), String>(())
    });
    if let Err(error) = load_result {
        eprintln!(
            "update: pending apply failed: {} — falling back to OTA loop",
            error
        );
    }
}

#[allow(dead_code)] // REPL kept for debug; auto_ota_loop is the live path
fn command_ls(path: &str) -> Result<(), String> {
    let entries =
        fs::read_dir(path).map_err(|error| format!("ls: cannot read '{}': {}", path, error))?;
    let mut items: Vec<_> = entries.filter_map(Result::ok).collect();
    items.sort_by_key(|entry| entry.file_name());

    for entry in items {
        let metadata = entry
            .metadata()
            .map_err(|error| format!("ls: cannot stat '{}': {}", entry.path().display(), error))?;
        let name = entry.file_name();
        if metadata.is_dir() {
            println!("{}/", name.to_string_lossy());
        } else {
            println!("{} {}B", name.to_string_lossy(), metadata.len());
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn command_rm(path: &str) -> Result<(), String> {
    fs::remove_file(path).map_err(|error| format!("rm: cannot remove '{}': {}", path, error))?;
    println!("removed {}", path);
    Ok(())
}

#[allow(dead_code)]
fn command_rename(src: &str, dst: &str) -> Result<(), String> {
    std::fs::rename(src, dst)
        .map_err(|error| format!("rename {} -> {} failed: {}", src, dst, error))?;
    println!("renamed {} -> {}", src, dst);
    Ok(())
}

#[allow(dead_code)]
fn main_loop() {
    println!(
        "ota_update ready. Commands: update, download, ls, rm, rename, image, wifi, reboot, pending, /quit"
    );

    loop {
        print!("you> ");
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).unwrap_or(0) == 0 {
            break;
        }
        let input = input.trim();
        if input == "/quit" || input == "/exit" {
            break;
        }
        if input.is_empty() {
            continue;
        }
        if input == "update" || input == "update status" {
            download::print_update_status();
            continue;
        }
        if input.starts_with("update ") {
            eprintln!("Usage: update [status]");
            continue;
        }
        if input == "download" || input.starts_with("download ") {
            let parts: Vec<&str> = input.split_whitespace().collect();
            let file = match parts.as_slice() {
                ["download"] => Ok(None),
                ["download", "-f", name] | ["download", "--file", name] => Ok(Some(*name)),
                _ => Err("Usage: download [-f|--file] <name>".to_string()),
            };
            match file.and_then(download::command) {
                Ok(()) => {}
                Err(error) => eprintln!("download failed: {error}"),
            }
            continue;
        }
        if input == "ls" || input.starts_with("ls ") {
            let path = input.strip_prefix("ls").unwrap_or("").trim();
            let path = if path.is_empty() { "/data" } else { path };
            if let Err(error) = download::with_fs_lock(|| command_ls(path)) {
                eprintln!("{error}");
            }
            continue;
        }
        if input == "rm" {
            eprintln!("Usage: rm <path>");
            continue;
        }
        if let Some(path) = input.strip_prefix("rm ") {
            let path = path.trim();
            if path.is_empty() {
                eprintln!("Usage: rm <path>");
            } else if let Err(error) = download::with_fs_lock(|| command_rm(path)) {
                eprintln!("{error}");
            }
            continue;
        }

        if input == "rename" {
            eprintln!("Usage: rename <src> <dst>");
            continue;
        }
        if let Some(rest) = input.strip_prefix("rename ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() != 2 {
                eprintln!("Usage: rename <src> <dst>");
            } else if let Err(error) = download::with_fs_lock(|| command_rename(parts[0], parts[1]))
            {
                eprintln!("{error}");
            }
            continue;
        }

        if input == "image" || input.starts_with("image ") {
            let parts: Vec<&str> = input.split_whitespace().collect();
            if let Err(error) = download::with_fs_lock(|| image::command(&parts[1..])) {
                eprintln!("image failed: {error}");
            }
            continue;
        }

        if input == "wifi" {
            if let Err(error) = wifi::connect_wifi() {
                eprintln!("wifi: {error}");
            }
            continue;
        }

        if input == "pending" {
            match fs::read_to_string(PENDING_UPDATE_MARKER) {
                Ok(content) => println!("pending: {}", content.trim()),
                Err(error) => println!("pending: none ({})", error),
            }
            continue;
        }

        if input == "reboot" {
            println!("rebooting...");
            let _ = std::io::stdout().flush();
            esp_rom_sys::rom::software_reset(); // -> !, never returns
        }

        eprintln!(
            "unknown command: {} (try: update, ls, rm, rename, image, wifi, reboot, pending)",
            input
        );
    }
}
