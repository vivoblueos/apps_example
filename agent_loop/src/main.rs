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
extern crate librs;
extern crate rsrt;

mod api;
mod client;
mod download;
mod elf_stream;
mod error;
mod http;
mod image;
mod sse;
mod tls;
mod transfer;
mod wifi;

use alloc::string::ToString;
use std::{env, fs, io::Write as _, thread};

use api::chat::{ChatCompletionRequest, ChatMessage};
use tls::EmbeddedTlsTransport;

fn main() {
    thread::Builder::new()
        .name("agent_loop".to_string())
        .stack_size(24 << 10)
        .spawn(move || {
            println!("Hello, agent_loop!");
            wifi::connect_wifi();
            download::start_update_worker();
            main_loop();
        })
        .unwrap()
        .join()
        .unwrap();
}

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

fn command_rm(path: &str) -> Result<(), String> {
    fs::remove_file(path).map_err(|error| format!("rm: cannot remove '{}': {}", path, error))?;
    println!("removed {}", path);
    Ok(())
}

fn build_client(
    endpoint: &str,
    api_key: &str,
) -> Result<client::Client<EmbeddedTlsTransport>, String> {
    client::Client::builder(EmbeddedTlsTransport::new().with_sni("api.deepseek.com"))
        .with_endpoint(endpoint)
        .with_host_header("api.deepseek.com")
        .with_api_key(api_key)
        .with_max_response_body_size(100 * 1024)
        .build()
        .map_err(|error| format!("client build failed: {error}"))
}

fn main_loop() {
    let api_key = env::var("OPENAI_API_KEY").unwrap_or_else(|_| String::new());
    let endpoint =
        env::var("OPENAI_API_BASE").unwrap_or_else(|_| String::from("https://124.72.129.71"));
    let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| String::from("deepseek-chat"));

    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--once") {
        let prompt = match args.get(1) {
            Some(p) => p,
            None => {
                eprintln!("--once requires a prompt");
                return;
            }
        };
        let mut client = match build_client(&endpoint, &api_key) {
            Ok(client) => client,
            Err(error) => {
                eprintln!("{error}");
                return;
            }
        };
        match run_turn(&mut client, &model, &mut Vec::new(), prompt) {
            Ok(reply) => println!("{reply}"),
            Err(e) => eprintln!("error: {e}"),
        }
        return;
    }

    println!("first run");

    let mut messages = Vec::new();
    println!("Model: {model}. Enter /quit to exit.");
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

        if input == "image" || input.starts_with("image ") {
            let parts: Vec<&str> = input.split_whitespace().collect();
            if let Err(error) = download::with_fs_lock(|| image::command(&parts[1..])) {
                eprintln!("image failed: {error}");
            }
            continue;
        }

        let mut client = match build_client(&endpoint, &api_key) {
            Ok(client) => client,
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
        };
        match run_turn(&mut client, &model, &mut messages, input) {
            Ok(reply) => println!("assistant> {reply}"),
            Err(e) => eprintln!("request failed: {e}"),
        }
    }
}

fn run_turn(
    client: &mut client::Client<EmbeddedTlsTransport>,
    model: &str,
    messages: &mut Vec<ChatMessage>,
    prompt: &str,
) -> Result<String, alloc::string::String> {
    messages.push(ChatMessage::user(prompt));
    let request = ChatCompletionRequest::new(model, messages.clone());
    let response = match client.chat_completion(&request) {
        Ok(response) => response,
        Err(error) => {
            messages.pop();
            return Err(error.to_string());
        }
    };
    let reply = response
        .data
        .choices
        .first()
        .and_then(|choice| choice.message.content.clone())
        .ok_or_else(|| "response did not contain assistant text".to_string())?;
    messages.push(ChatMessage::assistant(reply.clone()));
    Ok(reply)
}
