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
mod error;
mod http;
mod sse;
mod tls;
mod wifi;

use alloc::string::ToString;
use std::{env, io::Write as _, thread};

use api::chat::{ChatCompletionRequest, ChatMessage};
use tls::EmbeddedTlsTransport;

fn main() {
    /*
    thread::Builder::new()
        .name("agent_loop".to_string())
        .stack_size(64 << 10)
        .spawn(move || {
            println!("Hello, agent_loop!");
            wifi::connect_wifi();
            main_loop();
        })
        .unwrap()
        .join()
        .unwrap();
        */
    
            println!("Hello, agent_loop!");
            wifi::connect_wifi();
            main_loop();
}

fn main_loop() {
    let api_key = env::var("OPENAI_API_KEY").unwrap_or_else(|_| String::from(""));
    let endpoint =
        //env::var("OPENAI_API_BASE").unwrap_or_else(|_| String::from("https://124.72.129.70"));
        env::var("OPENAI_API_BASE").unwrap_or_else(|_| String::from("https://14.116.174.67"));
    let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| String::from("deepseek-chat"));

    println!("get model server info");

    let client = match client::Client::builder(
        EmbeddedTlsTransport::new().with_sni("api.deepseek.com"))
        .with_endpoint(endpoint)
        .with_host_header("api.deepseek.com")
        .with_api_key(api_key)
        .with_max_response_body_size(100 * 1024)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client build failed: {e}");
            return;
        }
    };

    println!("init success!");

    let mut client = client;
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--once") {
        let prompt = match args.get(1) {
            Some(p) => p,
            None => {
                eprintln!("--once requires a prompt");
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
