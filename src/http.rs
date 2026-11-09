use crate::curlffi::Easy;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const MAX_RESPONSE: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Options {
    pub connect_timeout: i64,
    pub transfer_timeout: i64,
    pub low_speed_time: i64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            connect_timeout: 10,
            transfer_timeout: 0,
            low_speed_time: 60,
        }
    }
}

pub struct Request<'a> {
    pub method: Method,
    pub url: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
    pub options: Options,
}

#[derive(Debug)]
pub enum Error {
    Transport(String),
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Transport(message) => f.write_str(message),
            Error::Cancelled => f.write_str("request cancelled"),
        }
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub struct Client {
    easy: Mutex<Option<Easy>>,
}

impl Client {
    pub fn new() -> Arc<Client> {
        Arc::new(Client {
            easy: Mutex::new(None),
        })
    }

    pub fn send(
        &self,
        request: &Request<'_>,
        cancel: Option<&AtomicBool>,
        on_data: &mut dyn FnMut(&[u8]),
    ) -> Result<u16, Error> {
        let mut guard = self.easy.lock().unwrap_or_else(|error| error.into_inner());
        if guard.is_none() {
            *guard = Some(Easy::new().map_err(Error::Transport)?);
        }
        let easy = guard.as_mut().unwrap();
        prepare(easy, request)?;

        let mut state = WriteState {
            on_data,
            cancel,
            cancelled: false,
        };
        let status = {
            let mut transfer = easy.transfer();
            transfer.write_function(write_cb, &mut state as *mut WriteState as *mut c_void);
            if cancel.is_some() {
                transfer
                    .progress_function(progress_cb, &mut state as *mut WriteState as *mut c_void);
            }
            transfer.perform().map_err(|error| {
                if state.cancelled || cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                    Error::Cancelled
                } else {
                    Error::Transport(error)
                }
            })?;
            easy.response_code().map_err(Error::Transport)? as u16
        };
        if state.cancelled {
            return Err(Error::Cancelled);
        }
        Ok(status)
    }

    pub fn request(
        &self,
        request: &Request<'_>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Response, Error> {
        let mut guard = self.easy.lock().unwrap_or_else(|error| error.into_inner());
        if guard.is_none() {
            *guard = Some(Easy::new().map_err(Error::Transport)?);
        }
        let easy = guard.as_mut().unwrap();
        prepare(easy, request)?;

        let mut state = ResponseState {
            cancel,
            ..ResponseState::default()
        };
        let status = {
            let mut transfer = easy.transfer();
            transfer.write_function(
                response_write_cb,
                &mut state as *mut ResponseState as *mut c_void,
            );
            transfer.header_function(
                response_header_cb,
                &mut state as *mut ResponseState as *mut c_void,
            );
            if cancel.is_some() {
                transfer.progress_function(
                    response_progress_cb,
                    &mut state as *mut ResponseState as *mut c_void,
                );
            }
            transfer.perform().map_err(|error| {
                if state.cancelled || cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                    Error::Cancelled
                } else {
                    Error::Transport(error)
                }
            })?;
            easy.response_code().map_err(Error::Transport)? as u16
        };
        if state.cancelled {
            return Err(Error::Cancelled);
        }
        if state.too_large {
            return Err(Error::Transport(format!(
                "response exceeded {MAX_RESPONSE} bytes"
            )));
        }
        Ok(Response {
            status,
            headers: state.headers,
            body: state.body,
        })
    }
}

fn prepare(easy: &mut Easy, request: &Request<'_>) -> Result<(), Error> {
    easy.url(request.url).map_err(Error::Transport)?;
    easy.method(request.method.as_str())
        .map_err(Error::Transport)?;
    if !request.body.is_empty() {
        easy.post_fields(request.body).map_err(Error::Transport)?;
    }
    easy.fail_on_error(false).map_err(Error::Transport)?;
    easy.follow_location(false).map_err(Error::Transport)?;
    easy.connect_timeout(request.options.connect_timeout)
        .map_err(Error::Transport)?;
    easy.timeout(request.options.transfer_timeout)
        .map_err(Error::Transport)?;
    easy.low_speed(1, request.options.low_speed_time)
        .map_err(Error::Transport)?;
    easy.headers(request.headers).map_err(Error::Transport)?;
    Ok(())
}

struct WriteState<'a> {
    on_data: &'a mut dyn FnMut(&[u8]),
    cancel: Option<&'a AtomicBool>,
    cancelled: bool,
}

unsafe extern "C" fn write_cb(
    chunk: *mut c_char,
    size: usize,
    count: usize,
    data: *mut c_void,
) -> usize {
    let state = unsafe { &mut *(data as *mut WriteState) };
    if state
        .cancel
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
    {
        state.cancelled = true;
        return 0;
    }
    let len = size * count;
    let slice = unsafe { std::slice::from_raw_parts(chunk as *const u8, len) };
    (state.on_data)(slice);
    len
}

unsafe extern "C" fn progress_cb(
    data: *mut c_void,
    _dltotal: f64,
    _dlnow: f64,
    _ultotal: f64,
    _ulnow: f64,
) -> c_int {
    let state = unsafe { &mut *(data as *mut WriteState) };
    if state
        .cancel
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
    {
        state.cancelled = true;
        return 1;
    }
    0
}

#[derive(Default)]
struct ResponseState<'a> {
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    cancel: Option<&'a AtomicBool>,
    too_large: bool,
    cancelled: bool,
}

unsafe extern "C" fn response_write_cb(
    chunk: *mut c_char,
    size: usize,
    count: usize,
    data: *mut c_void,
) -> usize {
    let state = unsafe { &mut *(data as *mut ResponseState) };
    if state
        .cancel
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
    {
        state.cancelled = true;
        return 0;
    }
    let len = size * count;
    if state.body.len() + len > MAX_RESPONSE {
        state.too_large = true;
        return 0;
    }
    let slice = unsafe { std::slice::from_raw_parts(chunk as *const u8, len) };
    state.body.extend_from_slice(slice);
    len
}

unsafe extern "C" fn response_header_cb(
    chunk: *mut c_char,
    size: usize,
    count: usize,
    data: *mut c_void,
) -> usize {
    let state = unsafe { &mut *(data as *mut ResponseState) };
    let len = size * count;
    let line = unsafe { std::slice::from_raw_parts(chunk as *const u8, len) };
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches(['\r', '\n']);
    if line.starts_with("HTTP/") {
        state.headers.clear();
    } else if let Some((name, value)) = line.split_once(':') {
        state
            .headers
            .push((name.trim().to_string(), value.trim().to_string()));
    }
    len
}

unsafe extern "C" fn response_progress_cb(
    data: *mut c_void,
    _dltotal: f64,
    _dlnow: f64,
    _ultotal: f64,
    _ulnow: f64,
) -> c_int {
    let state = unsafe { &mut *(data as *mut ResponseState) };
    if state
        .cancel
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
    {
        state.cancelled = true;
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn serve(replies: Vec<(u16, &str, &str)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let replies: Vec<(u16, String, String)> = replies
            .into_iter()
            .map(|(status, headers, body)| (status, headers.to_string(), body.to_string()))
            .collect();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, headers, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                requests.push(read_request(&mut stream));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
            requests
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            while buffer.len() < end + 4 + length {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            return String::from_utf8_lossy(&buffer).into_owned();
        }
        String::from_utf8_lossy(&buffer).into_owned()
    }

    fn request<'a>(
        method: Method,
        url: &'a str,
        headers: &'a [(String, String)],
        body: &'a [u8],
    ) -> Request<'a> {
        Request {
            method,
            url,
            headers,
            body,
            options: Options::default(),
        }
    }

    #[test]
    fn a_get_returns_the_body_and_the_response_headers() {
        let (base, server) = serve(vec![(200, "X-Test: yes\r\n", "hello")]);
        let client = Client::new();
        let url = format!("{base}/path");
        let response = client
            .request(&request(Method::Get, &url, &[], &[]), None)
            .unwrap();
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /path HTTP/1.1"),
            "{}",
            requests[0]
        );
        assert_eq!(response.status, 200);
        assert_eq!(response.header("x-test"), Some("yes"));
        assert_eq!(response.text(), "hello");
    }

    #[test]
    fn a_put_sends_its_body_and_method() {
        let (base, server) = serve(vec![(200, "", "")]);
        let client = Client::new();
        let url = format!("{base}/item");
        let response = client
            .request(&request(Method::Put, &url, &[], b"hello"), None)
            .unwrap();
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("PUT /item HTTP/1.1"),
            "{}",
            requests[0]
        );
        assert!(requests[0].ends_with("\r\n\r\nhello"), "{}", requests[0]);
        assert_eq!(response.status, 200);
    }
}
