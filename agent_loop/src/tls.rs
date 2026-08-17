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

extern crate embedded_tls;
use alloc::{format, string::String, vec, vec::Vec};
use embedded_io::{ErrorType, Read, Write};
use embedded_io_adapters::std::{to_std_error, FromStd};
use embedded_tls::blocking::*;
use rand_core::{CryptoRng, RngCore};
use std::net::TcpStream;

const TLS_RECORD_BUF_SIZE: usize = 16640;

struct SimpleRng(fastrand::Rng);

impl RngCore for SimpleRng {
    fn next_u32(&mut self) -> u32 {
        self.0.u32(..)
    }

    fn next_u64(&mut self) -> u64 {
        self.0.u64(..)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.0.fill(dest);
        Ok(())
    }
}

impl CryptoRng for SimpleRng {}

pub struct TlsSocketStd<'a> {
    inner: TlsConnection<'a, FromStd<TcpStream>, Aes128GcmSha256>,
}

impl<'a> ErrorType for TlsSocketStd<'a> {
    type Error = std::io::Error;
}

impl<'a> Read for TlsSocketStd<'a> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.inner.read(buf).map_err(to_std_error)
    }
}

impl<'a> Write for TlsSocketStd<'a> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.inner.write(buf).map_err(to_std_error)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush().map_err(to_std_error)
    }
}

pub enum AgentSocket<'a> {
    Plain(FromStd<TcpStream>),
    Tls(TlsSocketStd<'a>),
}

impl<'a> ErrorType for AgentSocket<'a> {
    type Error = std::io::Error;
}

impl<'a> Read for AgentSocket<'a> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl<'a> Write for AgentSocket<'a> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}

pub struct EmbeddedTlsTransport {
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
    rng: SimpleRng,
    sni: Option<String>,
}

impl EmbeddedTlsTransport {
    pub fn new() -> Self {
        Self {
            read_buf: vec![0u8; TLS_RECORD_BUF_SIZE],
            write_buf: vec![0u8; TLS_RECORD_BUF_SIZE],
            rng: SimpleRng(fastrand::Rng::with_seed(0xDEAD_BEEF_CAFE_BABE)),
            sni: None,
        }
    }

    pub fn with_sni(mut self, name: impl Into<String>) -> Self {
        self.sni = Some(name.into());
        self
    }
}

impl Default for EmbeddedTlsTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::http::SocketTransport for EmbeddedTlsTransport {
    type Error = std::io::Error;
    type Socket<'a> = AgentSocket<'a>;

    fn connect<'a>(
        &'a mut self,
        host: &str,
        port: u16,
        scheme: crate::http::Scheme,
    ) -> Result<Self::Socket<'a>, Self::Error> {
        println!("[http] connecting to {}:{}...", host, port);
        let stream = TcpStream::connect((host, port))?;
        stream.set_nodelay(true)?;
        println!("[http] TCP connected");

        match scheme {
            crate::http::Scheme::Http => Ok(AgentSocket::Plain(FromStd::new(stream))),
            crate::http::Scheme::Https => {
                let server_name = self.sni.as_deref().unwrap_or(host);
                let write_buf_ptr = self.write_buf.as_ptr();
                let read_buf_ptr = self.read_buf.as_ptr();
                let mut tls: TlsConnection<FromStd<TcpStream>, Aes128GcmSha256> =
                    TlsConnection::new(
                        FromStd::new(stream),
                        &mut self.read_buf[..],
                        &mut self.write_buf[..],
                    );

                let config = TlsConfig::new()
                    .with_server_name(server_name)
                    .enable_rsa_signatures();
                println!("[http] TLS handshake...");
                let result =
                    tls.open::<SimpleRng, NoVerify>(TlsContext::new(&config, &mut self.rng));
                if let Err(ref e) = result {
                    println!("[http] TLS handshake failed: {:?}", e);
                    let wbuf = unsafe { core::slice::from_raw_parts(write_buf_ptr, 200) };
                    let rbuf = unsafe { core::slice::from_raw_parts(read_buf_ptr, 64) };
                    let w_hex: String = wbuf.iter().map(|b| format!("{:02x}", b)).collect();
                    let r_hex: String = rbuf.iter().map(|b| format!("{:02x}", b)).collect();
                    println!("[tls] ClientHello (write_buf first 200 bytes):");
                    println!("[tls] {}", w_hex);
                    println!("[tls] server response (read_buf first 64 bytes):");
                    println!("[tls] {}", r_hex);
                }
                result.map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        format!("TLS handshake failed: {e:?}"),
                    )
                })?;
                println!("[http] TLS established");

                Ok(AgentSocket::Tls(TlsSocketStd { inner: tls }))
            }
        }
    }
}
