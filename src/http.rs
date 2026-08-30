//! Minimal HTTP performed with libcurl directly (src/openai.rs) so streaming
//! and cancellation stay on one thread. libcurl is dlopen'd lazily; see
//! src/curlffi.rs.

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

pub fn get(url: &str, headers: &[(String, String)]) -> Result<Response, String> {
    let mut easy = crate::curlffi::Easy::new()?;
    easy.url(url)?;
    easy.http_get()?;
    easy.connect_timeout(10)?;
    // No streaming and no cancel hook here: cap the whole transfer.
    easy.timeout(30)?;
    easy.headers(headers)?;
    let mut sink = Vec::new();
    crate::curlffi::perform_with_sink(&mut easy, &mut sink)?;
    let status = easy.response_code()? as u16;
    Ok(Response { status, body: sink })
}
