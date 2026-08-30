//! Minimal HTTP performed with libcurl directly (src/openai.rs) so streaming
//! and cancellation stay on one thread. libcurl is dlopen'd lazily; see
//! src/curlffi.rs.

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}
