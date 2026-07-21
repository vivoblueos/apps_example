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

use alloc::{format, string::String, vec::Vec};
use core::fmt::Write;
use serde::{de::DeserializeOwned, Serialize};

use crate::{
    api::{
        chat::{ChatCompletionRequest, ChatCompletionResponse},
        common::DeletionStatus,
        embeddings::{EmbeddingRequest, EmbeddingResponse},
        models::{Model, ModelList},
        responses::{
            CountTokensRequest, CountTokensResponse, CreateResponseRequest, ResponseObject,
        },
    },
    error::{ApiErrorEnvelope, ConfigError, Error},
    http::{
        self, Header, HttpBody, HttpError, HttpResponseBody, Method, Request, Response, Scheme,
        SocketTransport,
    },
    sse::ApiStream,
};

pub const DEFAULT_API_ENDPOINT: &str = "https://api.openai.com/v1";
const DEFAULT_MAX_RESPONSE_BODY_SIZE: usize = 1024 * 1024;
const DEFAULT_MAX_RESPONSE_HEADER_SIZE: usize = 16 * 1024;
const READ_BUFFER_SIZE: usize = 1024;

#[derive(Debug)]
pub struct ApiResponse<T> {
    pub status: u16,
    pub headers: Vec<Header>,
    pub data: T,
}

impl<T> ApiResponse<T> {
    pub fn into_inner(self) -> T {
        self.data
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.is_name(name))
            .map(|header| header.value.as_str())
    }
}

#[derive(Debug, Clone)]
struct Endpoint {
    scheme: Scheme,
    host: String,
    port: u16,
    authority: String,
    base_path: String,
}

impl Endpoint {
    fn parse(endpoint: &str) -> Result<Self, ConfigError> {
        if endpoint.is_empty() {
            return Err(ConfigError::EmptyEndpoint);
        }
        if contains_line_break(endpoint) || endpoint.contains(['?', '#', '@']) {
            return Err(ConfigError::InvalidEndpoint);
        }

        let (scheme, rest) = endpoint
            .split_once("://")
            .ok_or(ConfigError::InvalidEndpoint)?;
        let scheme = match scheme {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            _ => return Err(ConfigError::InvalidEndpoint),
        };
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return Err(ConfigError::InvalidEndpoint);
        }

        let (host, port) = parse_authority(authority, scheme.default_port())?;
        let mut base_path = String::from(path.trim_end_matches('/'));
        if base_path == "/" {
            base_path.clear();
        }

        Ok(Self {
            scheme,
            host,
            port,
            authority: String::from(authority),
            base_path,
        })
    }
}

pub struct ClientBuilder<T> {
    transport: T,
    endpoint: String,
    api_key: Option<String>,
    organization: Option<String>,
    project: Option<String>,
    headers: Vec<Header>,
    host_header: Option<String>,
    max_response_body_size: usize,
    max_response_header_size: usize,
}

impl<T> ClientBuilder<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            endpoint: String::from(DEFAULT_API_ENDPOINT),
            api_key: None,
            organization: None,
            project: None,
            headers: Vec::new(),
            host_header: None,
            max_response_body_size: DEFAULT_MAX_RESPONSE_BODY_SIZE,
            max_response_header_size: DEFAULT_MAX_RESPONSE_HEADER_SIZE,
        }
    }

    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    pub fn with_organization(mut self, organization: impl Into<String>) -> Self {
        self.organization = Some(organization.into());
        self
    }

    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        upsert_header(&mut self.headers, Header::new(name, value));
        self
    }

    pub fn with_host_header(mut self, host: impl Into<String>) -> Self {
        self.host_header = Some(host.into());
        self
    }

    pub fn with_max_response_body_size(mut self, bytes: usize) -> Self {
        self.max_response_body_size = bytes;
        self
    }

    pub fn with_max_response_header_size(mut self, bytes: usize) -> Self {
        self.max_response_header_size = bytes;
        self
    }

    pub fn build(self) -> Result<Client<T>, ConfigError> {
        let endpoint = Endpoint::parse(self.endpoint.trim_end_matches('/'))?;

        if let Some(api_key) = &self.api_key {
            if contains_line_break(api_key) {
                return Err(ConfigError::InvalidApiKey);
            }
        }
        for header in &self.headers {
            validate_header(header)?;
        }
        if let Some(organization) = &self.organization {
            validate_header(&Header::new("OpenAI-Organization", organization.clone()))?;
        }
        if let Some(project) = &self.project {
            validate_header(&Header::new("OpenAI-Project", project.clone()))?;
        }

        Ok(Client {
            transport: self.transport,
            endpoint,
            api_key: self.api_key,
            organization: self.organization,
            project: self.project,
            headers: self.headers,
            host_header: self.host_header,
            max_response_body_size: self.max_response_body_size,
            max_response_header_size: self.max_response_header_size,
        })
    }
}

pub struct Client<T> {
    transport: T,
    endpoint: Endpoint,
    api_key: Option<String>,
    organization: Option<String>,
    project: Option<String>,
    headers: Vec<Header>,
    host_header: Option<String>,
    max_response_body_size: usize,
    max_response_header_size: usize,
}

impl<T> Client<T> {
    pub fn builder(transport: T) -> ClientBuilder<T> {
        ClientBuilder::new(transport)
    }

    pub fn new(transport: T, api_key: impl Into<String>) -> Result<Self, ConfigError> {
        Self::builder(transport).with_api_key(api_key).build()
    }

    pub fn endpoint(&self) -> String {
        let mut endpoint = format!(
            "{}://{}",
            match self.endpoint.scheme {
                Scheme::Http => "http",
                Scheme::Https => "https",
            },
            self.endpoint.authority
        );
        endpoint.push_str(&self.endpoint.base_path);
        endpoint
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn into_transport(self) -> T {
        self.transport
    }
}

impl<T: SocketTransport> Client<T> {
    pub fn get_json<R>(&mut self, path: &str) -> Result<ApiResponse<R>, Error<T::Error>>
    where
        R: DeserializeOwned,
    {
        let response = self.send_buffered(Method::Get, path, &[], false)?;
        decode_json(response)
    }

    pub fn post_json<Q, R>(
        &mut self,
        path: &str,
        request: &Q,
    ) -> Result<ApiResponse<R>, Error<T::Error>>
    where
        Q: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let body = serde_json::to_vec(request).map_err(Error::Serialize)?;
        let response = self.send_buffered(Method::Post, path, &body, true)?;
        decode_json(response)
    }

    pub fn delete_json<R>(&mut self, path: &str) -> Result<ApiResponse<R>, Error<T::Error>>
    where
        R: DeserializeOwned,
    {
        let response = self.send_buffered(Method::Delete, path, &[], false)?;
        decode_json(response)
    }

    pub fn send_raw(
        &mut self,
        method: Method,
        path: &str,
        body: &[u8],
        content_type: Option<&str>,
    ) -> Result<ApiResponse<Vec<u8>>, Error<T::Error>> {
        self.send_buffered_with_content_type(method, path, body, content_type)
    }

    pub fn post_json_stream<'a, Q>(
        &'a mut self,
        path: &str,
        request: &Q,
    ) -> Result<ApiStream<HttpResponseBody<T::Socket<'a>>>, Error<T::Error>>
    where
        Q: Serialize + ?Sized,
    {
        let body = stream_json_body(request)?;
        self.send_stream(path, &body)
    }

    pub fn create_response(
        &mut self,
        request: &CreateResponseRequest,
    ) -> Result<ApiResponse<ResponseObject>, Error<T::Error>> {
        self.post_json("responses", request)
    }

    pub fn create_response_stream<'a>(
        &'a mut self,
        request: &CreateResponseRequest,
    ) -> Result<ApiStream<HttpResponseBody<T::Socket<'a>>>, Error<T::Error>> {
        self.post_json_stream("responses", request)
    }

    pub fn retrieve_response(
        &mut self,
        response_id: &str,
    ) -> Result<ApiResponse<ResponseObject>, Error<T::Error>> {
        self.get_json(&format!("responses/{}", encode_path_segment(response_id)))
    }

    pub fn delete_response(
        &mut self,
        response_id: &str,
    ) -> Result<ApiResponse<DeletionStatus>, Error<T::Error>> {
        self.delete_json(&format!("responses/{}", encode_path_segment(response_id)))
    }

    pub fn cancel_response(
        &mut self,
        response_id: &str,
    ) -> Result<ApiResponse<ResponseObject>, Error<T::Error>> {
        self.post_json(
            &format!("responses/{}/cancel", encode_path_segment(response_id)),
            &serde_json::json!({}),
        )
    }

    pub fn count_response_input_tokens(
        &mut self,
        request: &CountTokensRequest,
    ) -> Result<ApiResponse<CountTokensResponse>, Error<T::Error>> {
        self.post_json("responses/input_tokens", request)
    }

    pub fn chat_completion(
        &mut self,
        request: &ChatCompletionRequest,
    ) -> Result<ApiResponse<ChatCompletionResponse>, Error<T::Error>> {
        self.post_json("chat/completions", request)
    }

    pub fn chat_completion_stream<'a>(
        &'a mut self,
        request: &ChatCompletionRequest,
    ) -> Result<ApiStream<HttpResponseBody<T::Socket<'a>>>, Error<T::Error>> {
        self.post_json_stream("chat/completions", request)
    }

    pub fn create_embedding(
        &mut self,
        request: &EmbeddingRequest,
    ) -> Result<ApiResponse<EmbeddingResponse>, Error<T::Error>> {
        self.post_json("embeddings", request)
    }

    pub fn list_models(&mut self) -> Result<ApiResponse<ModelList>, Error<T::Error>> {
        self.get_json("models")
    }

    pub fn retrieve_model(
        &mut self,
        model_id: &str,
    ) -> Result<ApiResponse<Model>, Error<T::Error>> {
        self.get_json(&format!("models/{}", encode_path_segment(model_id)))
    }

    fn send_buffered(
        &mut self,
        method: Method,
        path: &str,
        body: &[u8],
        is_json: bool,
    ) -> Result<ApiResponse<Vec<u8>>, Error<T::Error>> {
        self.send_buffered_with_content_type(
            method,
            path,
            body,
            is_json.then_some("application/json"),
        )
    }

    fn send_buffered_with_content_type(
        &mut self,
        method: Method,
        path: &str,
        body: &[u8],
        content_type: Option<&str>,
    ) -> Result<ApiResponse<Vec<u8>>, Error<T::Error>> {
        let max_response_body_size = self.max_response_body_size;
        let response = self.send_http(
            method,
            path,
            body,
            content_type,
            false,
            method == Method::Post || content_type.is_some() || !body.is_empty(),
        )?;
        let status = response.status;
        let headers = response.headers;
        let body = read_body(response.body, max_response_body_size)?;
        if !(200..300).contains(&status) {
            return Err(api_error(status, body));
        }
        Ok(ApiResponse {
            status,
            headers,
            data: body,
        })
    }

    fn send_stream<'a>(
        &'a mut self,
        path: &str,
        body: &[u8],
    ) -> Result<ApiStream<HttpResponseBody<T::Socket<'a>>>, Error<T::Error>> {
        let max_response_body_size = self.max_response_body_size;
        let response = self.send_http(
            Method::Post,
            path,
            body,
            Some("application/json"),
            true,
            true,
        )?;
        if !(200..300).contains(&response.status) {
            let status = response.status;
            let body = read_body(response.body, max_response_body_size)?;
            return Err(api_error(status, body));
        }
        Ok(ApiStream::new(
            response.status,
            response.headers,
            response.body,
        ))
    }

    fn send_http<'a>(
        &'a mut self,
        method: Method,
        path: &str,
        body: &[u8],
        content_type: Option<&str>,
        stream: bool,
        body_present: bool,
    ) -> Result<Response<HttpResponseBody<T::Socket<'a>>>, Error<T::Error>> {
        let target = self.build_target(path)?;
        let headers = self.build_headers(content_type, stream);
        let host = self.endpoint.host.clone();
        let authority = self
            .host_header
            .clone()
            .unwrap_or_else(|| self.endpoint.authority.clone());
        let port = self.endpoint.port;
        let scheme = self.endpoint.scheme;
        let socket = self
            .transport
            .connect(&host, port, scheme)
            .map_err(|error| Error::Http(HttpError::Transport(error)))?;
        http::send_request(
            socket,
            Request {
                method,
                target: &target,
                host: &authority,
                headers: &headers,
                body: body_present.then_some(body),
            },
            self.max_response_header_size,
        )
        .map_err(Error::Http)
    }

    fn build_target(&self, path: &str) -> Result<String, Error<T::Error>> {
        if contains_line_break(path) {
            return Err(Error::InvalidPath);
        }
        let path = path.trim_start_matches('/');
        if self.endpoint.base_path.is_empty() {
            Ok(format!("/{path}"))
        } else if path.is_empty() {
            Ok(self.endpoint.base_path.clone())
        } else {
            Ok(format!("{}/{path}", self.endpoint.base_path))
        }
    }

    fn build_headers(&self, content_type: Option<&str>, stream: bool) -> Vec<Header> {
        let mut headers = self.headers.clone();
        upsert_header(
            &mut headers,
            Header::new(
                "Accept",
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            ),
        );
        if let Some(content_type) = content_type {
            upsert_header(&mut headers, Header::new("Content-Type", content_type));
        }
        if let Some(api_key) = &self.api_key {
            upsert_header(
                &mut headers,
                Header::new("Authorization", format!("Bearer {api_key}")),
            );
        }
        if let Some(organization) = &self.organization {
            upsert_header(
                &mut headers,
                Header::new("OpenAI-Organization", organization.clone()),
            );
        }
        if let Some(project) = &self.project {
            upsert_header(&mut headers, Header::new("OpenAI-Project", project.clone()));
        }
        headers
    }
}

fn stream_json_body<Q: Serialize + ?Sized, E>(request: &Q) -> Result<Vec<u8>, Error<E>> {
    let mut value = serde_json::to_value(request).map_err(Error::Serialize)?;
    let object = value.as_object_mut().ok_or_else(|| {
        Error::Serialize(<serde_json::Error as serde::ser::Error>::custom(
            "streaming request must serialize to a JSON object",
        ))
    })?;
    object.insert(String::from("stream"), serde_json::Value::Bool(true));
    serde_json::to_vec(&value).map_err(Error::Serialize)
}

fn read_body<B, E>(mut body: B, limit: usize) -> Result<Vec<u8>, Error<E>>
where
    B: HttpBody<Error = HttpError<E>>,
{
    let mut output = Vec::new();
    let mut chunk = [0u8; READ_BUFFER_SIZE];
    let mut next_milestone = 4096usize;
    loop {
        let count = body.read(&mut chunk).map_err(Error::Http)?;
        if count == 0 {
            println!("[http] body complete: {} bytes", output.len());
            return Ok(output);
        }
        if output.len().saturating_add(count) > limit {
            return Err(Error::ResponseTooLarge { limit });
        }
        output.extend_from_slice(&chunk[..count]);
        if output.len() >= next_milestone {
            println!("[http] body: {} bytes received...", output.len());
            next_milestone = output.len() + 4096;
        }
    }
}

fn decode_json<T, E>(response: ApiResponse<Vec<u8>>) -> Result<ApiResponse<T>, Error<E>>
where
    T: DeserializeOwned,
{
    match serde_json::from_slice(&response.data) {
        Ok(data) => Ok(ApiResponse {
            status: response.status,
            headers: response.headers,
            data,
        }),
        Err(source) => Err(Error::Deserialize {
            source,
            body: response.data,
        }),
    }
}

fn api_error<E>(status: u16, body: Vec<u8>) -> Error<E> {
    let error = serde_json::from_slice::<ApiErrorEnvelope>(&body)
        .ok()
        .map(|envelope| envelope.error);
    Error::Api {
        status,
        error,
        body,
    }
}

fn parse_authority(authority: &str, default_port: u16) -> Result<(String, u16), ConfigError> {
    if authority.starts_with('[') {
        let end = authority.find(']').ok_or(ConfigError::InvalidEndpoint)?;
        let host = &authority[1..end];
        let suffix = &authority[end + 1..];
        let port = if suffix.is_empty() {
            default_port
        } else {
            suffix
                .strip_prefix(':')
                .ok_or(ConfigError::InvalidEndpoint)?
                .parse()
                .map_err(|_| ConfigError::InvalidEndpoint)?
        };
        if host.is_empty() {
            return Err(ConfigError::InvalidEndpoint);
        }
        return Ok((String::from(host), port));
    }

    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (
            host,
            port.parse().map_err(|_| ConfigError::InvalidEndpoint)?,
        ),
        _ => (authority, default_port),
    };
    if host.is_empty() || host.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(ConfigError::InvalidEndpoint);
    }
    Ok((String::from(host), port))
}

fn validate_header(header: &Header) -> Result<(), ConfigError> {
    if header.name.is_empty()
        || !header
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(ConfigError::InvalidHeaderName(header.name.clone()));
    }
    if contains_line_break(&header.value) {
        return Err(ConfigError::InvalidHeaderValue(header.name.clone()));
    }
    Ok(())
}

fn contains_line_break(value: &str) -> bool {
    value.bytes().any(|byte| byte == b'\r' || byte == b'\n')
}

fn upsert_header(headers: &mut Vec<Header>, new_header: Header) {
    if let Some(header) = headers
        .iter_mut()
        .find(|header| header.is_name(&new_header.name))
    {
        *header = new_header;
    } else {
        headers.push(new_header);
    }
}

fn encode_path_segment(segment: &str) -> String {
    let mut encoded = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(&mut encoded, "%{byte:02X}");
        }
    }
    encoded
}
