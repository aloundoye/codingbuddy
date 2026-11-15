use anyhow::Result;
use codingbuddy_core::CancellationToken;
use std::{
    future::Future,
    io::{self, BufRead, Read},
    time::Duration,
};

pub(crate) struct ChatTransport {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    cancellation: Option<CancellationToken>,
}

impl ChatTransport {
    pub fn new(timeout: Duration, cancellation: Option<CancellationToken>) -> Result<Self> {
        Ok(Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
            client: reqwest::Client::builder()
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            cancellation,
        })
    }

    fn wait<T>(&self, future: impl Future<Output = T>) -> Result<T> {
        self.runtime.block_on(async {
            let mut future = std::pin::pin!(future);
            loop {
                if self
                    .cancellation
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
                {
                    anyhow::bail!("Turn interrupted");
                }
                if let Ok(value) =
                    tokio::time::timeout(Duration::from_millis(30), future.as_mut()).await
                {
                    return Ok(value);
                }
            }
        })
    }

    pub fn post(
        &self,
        endpoint: &str,
        headers: reqwest::header::HeaderMap,
        payload: &serde_json::Value,
    ) -> Result<Result<reqwest::Response, reqwest::Error>> {
        self.wait(async {
            self.client
                .post(endpoint)
                .headers(headers)
                .json(payload)
                .send()
                .await
        })
    }

    pub fn text(&self, response: reqwest::Response) -> Result<String> {
        Ok(self
            .wait(response.text())?
            .map_err(reqwest::Error::without_url)?)
    }

    pub fn reader(&self, response: reqwest::Response) -> impl Read + '_ {
        ResponseReader {
            transport: self,
            response,
            current: io::Cursor::new(Vec::new()),
        }
    }
}

struct ResponseReader<'a> {
    transport: &'a ChatTransport,
    response: reqwest::Response,
    current: io::Cursor<Vec<u8>>,
}

impl Read for ResponseReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let size = self.current.read(output)?;
            if size != 0 {
                return Ok(size);
            }
            let next = self
                .transport
                .wait(self.response.chunk())
                .map_err(io::Error::other)?
                .map_err(|err| io::Error::other(err.without_url()))?;
            match next {
                Some(bytes) => self.current = io::Cursor::new(bytes.to_vec()),
                None => return Ok(0),
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct SseFrame {
    pub event: String,
    pub data: String,
}

pub(crate) fn next_frame(reader: &mut impl BufRead) -> io::Result<Option<SseFrame>> {
    let mut frame = SseFrame::default();
    let mut line = String::new();
    let mut has_data = false;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            if has_data {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated SSE frame",
                ));
            }
            return Ok(None);
        }
        let content = line.trim_end_matches(['\r', '\n']);
        if content.is_empty() && has_data {
            return Ok(Some(frame));
        }
        if content.is_empty() {
            frame.event.clear();
        }
        if let Some(value) = content.strip_prefix("event:") {
            frame.event = value.strip_prefix(' ').unwrap_or(value).to_owned();
        }
        if let Some(value) = content.strip_prefix("data:") {
            if has_data {
                frame.data.push('\n');
            }
            frame
                .data
                .push_str(value.strip_prefix(' ').unwrap_or(value));
            if frame.data.len() > 16 * 1024 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SSE frame exceeds size limit",
                ));
            }
            has_data = true;
        }
    }
}
