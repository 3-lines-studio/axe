use crate::curlffi::Easy;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
        easy.url(request.url).map_err(Error::Transport)?;
        easy.post().map_err(Error::Transport)?;
        easy.post_fields(request.body).map_err(Error::Transport)?;
        easy.fail_on_error(false).map_err(Error::Transport)?;
        easy.connect_timeout(request.options.connect_timeout)
            .map_err(Error::Transport)?;
        if request.options.transfer_timeout > 0 {
            easy.timeout(request.options.transfer_timeout)
                .map_err(Error::Transport)?;
        }
        easy.low_speed(1, request.options.low_speed_time)
            .map_err(Error::Transport)?;
        easy.headers(request.headers).map_err(Error::Transport)?;

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
